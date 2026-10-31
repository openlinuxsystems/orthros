// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! OHCI, a USB 1.1 host controller for low and full speed (the *Open Host
//! Controller Interface Specification for USB*, release 1.0a; ADR 0243):
//! the companions of AMD's and others' EHCI controllers, and the only USB
//! controller of older AMD, NVIDIA, SiS and VIA chipsets.
//!
//! An OHCI controller is one PCI function with its registers in BAR 0.
//! It reads its work from lists of endpoint descriptors (EDs) in memory,
//! each with a queue of transfer descriptors (TDs) it works through from
//! the ED's head to its tail:
//!
//! - the *control list*, here a head that is skipped and an ED for each
//!   device's default pipe, kept while the device is there;
//! - the *interrupt* EDs, reached from the 32 entries of the Host
//!   Controller Communications Area (HCCA), one a millisecond in turn:
//!   as on EHCI, they lead into a tree of skipped EDs for periods of 1 to
//!   32 ms, and a pipe's ED hangs after the one of its period and phase.
//!
//! An ED always ends its queue with an empty TD: a transfer fills that
//! one in, links a new empty one after it, and moves the ED's tail to the
//! new one. The controller retires each TD it finishes by moving the
//! ED's head past it; one that fails halts the ED. A TD moves up to 8 KiB
//! (two pages) of one transfer's data. The data toggle of a control
//! transfer is in each TD; an interrupt pipe's is carried in its ED.
//!
//! The driver is polled, as the EHCI one is. The firmware may drive the
//! controller through SMIs (its InterruptRouting bit) and lets go when
//! asked (OwnershipChangeRequest).

use core::ops::DerefMut;

use radix_hw::{Dma, Mmio, PAGE_SIZE, device_barrier};
use radix_usb::host::{Data, Endpoint, Error, Hcd, PipeId, Port, Target, Waiter};
use radix_usb::{Setup, Speed, TransferType};

use crate::{Handoff, Pool, Slot, wait_for};

/// The PCI class, subclass and programming interface of an OHCI
/// controller: serial bus, USB, OHCI.
pub const PCI_CLASS: (u8, u8, u8) = (0x0c, 0x03, 0x10);
/// The BAR that holds its registers.
pub const BAR: usize = 0;

/// The registers (7).
mod reg {
    pub const REVISION: usize = 0x00;
    pub const CONTROL: usize = 0x04;
    pub const COMMAND_STATUS: usize = 0x08;
    pub const INTERRUPT_STATUS: usize = 0x0c;
    pub const INTERRUPT_DISABLE: usize = 0x14;
    pub const HCCA: usize = 0x18;
    pub const CONTROL_HEAD: usize = 0x20;
    pub const CONTROL_CURRENT: usize = 0x24;
    pub const BULK_HEAD: usize = 0x28;
    pub const FM_INTERVAL: usize = 0x34;
    pub const FM_NUMBER: usize = 0x3c;
    pub const PERIODIC_START: usize = 0x40;
    pub const LS_THRESHOLD: usize = 0x44;
    pub const RH_DESCRIPTOR_A: usize = 0x48;
    pub const RH_STATUS: usize = 0x50;
    pub const RH_PORT_STATUS: usize = 0x54;

    /// CONTROL: the control to bulk ratio (4:1), the periodic and control
    /// lists on, the functional state (bits 6 and 7), and the SMI routing.
    pub const CBSR_4: u32 = 3;
    pub const PLE: u32 = 1 << 2;
    pub const CLE: u32 = 1 << 4;
    pub const HCFS_MASK: u32 = 3 << 6;
    pub const HCFS_RESET: u32 = 0 << 6;
    pub const HCFS_RESUME: u32 = 1 << 6;
    pub const HCFS_OPERATIONAL: u32 = 2 << 6;
    pub const IR: u32 = 1 << 8;
    /// COMMAND_STATUS: reset, the control list filled, ownership asked.
    pub const HCR: u32 = 1 << 0;
    pub const CLF: u32 = 1 << 1;
    pub const OCR: u32 = 1 << 3;
    /// INTERRUPT_STATUS: the done head written, an unrecoverable error; every bit, which a write of 1 clears.
    pub const WDH: u32 = 1 << 1;
    pub const UE: u32 = 1 << 4;
    pub const ALL: u32 = 0xc000_007f;
    /// RH_DESCRIPTOR_A: the ports, no power switching, the time power
    /// takes to be good (2 ms units, bits 24 to 31).
    pub const NDP: u32 = 0xff;
    pub const NPS: u32 = 1 << 9;
    /// RH_STATUS: SetGlobalPower, written.
    pub const LPSC: u32 = 1 << 16;
}

/// A root port's status (7.4.4): what it reads, and what writing a bit
/// does.
mod port {
    pub const CONNECTED: u32 = 1 << 0;
    pub const ENABLED: u32 = 1 << 1;
    pub const OVER_CURRENT: u32 = 1 << 3;
    pub const RESETTING: u32 = 1 << 4;
    pub const LOW_SPEED: u32 = 1 << 9;
    pub const CONNECT_CHANGE: u32 = 1 << 16;
    pub const ENABLE_CHANGE: u32 = 1 << 17;
    pub const SUSPEND_CHANGE: u32 = 1 << 18;
    pub const OVER_CURRENT_CHANGE: u32 = 1 << 19;
    pub const RESET_CHANGE: u32 = 1 << 20;
    pub const CHANGES: u32 = 0x1f << 16;
    /// Written: ClearPortEnable, SetPortReset, SetPortPower.
    pub const CLEAR_ENABLE: u32 = 1 << 0;
    pub const SET_RESET: u32 = 1 << 4;
    pub const SET_POWER: u32 = 1 << 8;
}

/// An ED's words (4.2), by offset.
mod ed {
    pub const INFO: usize = 0x00;
    pub const TAIL: usize = 0x04;
    pub const HEAD: usize = 0x08;
    pub const NEXT: usize = 0x0c;

    /// INFO: the address, the endpoint (bits 7 to 10), the direction from
    /// the TD, low speed, skip, and the packet size (16 to 26).
    pub const ENDPOINT_SHIFT: u32 = 7;
    pub const LOW_SPEED: u32 = 1 << 13;
    pub const SKIP: u32 = 1 << 14;
    pub const MAX_PACKET_SHIFT: u32 = 16;
    /// HEAD: halted, and the toggle carry.
    pub const HALTED: u32 = 1 << 0;
    pub const CARRY: u32 = 1 << 1;
    pub const ADDRESS: u32 = !0xf;
}

/// A general TD's words (4.3.1), by offset.
mod td {
    pub const INFO: usize = 0x00;
    pub const BUFFER: usize = 0x04;
    pub const NEXT: usize = 0x08;
    pub const END: usize = 0x0c;

    /// INFO: a short packet ends it well, the PID (bits 19 and 20), no
    /// interrupt, the toggle (from the ED, DATA0 or DATA1), the condition
    /// code (28 to 31: not yet done).
    pub const ROUNDING: u32 = 1 << 18;
    pub const SETUP: u32 = 0 << 19;
    pub const OUT: u32 = 1 << 19;
    pub const IN: u32 = 2 << 19;
    pub const NO_INTERRUPT: u32 = 7 << 21;
    pub const DATA0: u32 = 2 << 24;
    pub const DATA1: u32 = 3 << 24;
    pub const NOT_ACCESSED: u32 = 0xf << 28;
    pub const CC_SHIFT: u32 = 28;
}

/// Condition codes (4.3.3).
mod cc {
    pub const NO_ERROR: u32 = 0;
    pub const STALL: u32 = 4;
    pub const NOT_RESPONDING: u32 = 5;
    pub const DATA_OVERRUN: u32 = 8;
    pub const DATA_UNDERRUN: u32 = 9;
    pub const BUFFER_OVERRUN: u32 = 0xc;
    pub const BUFFER_UNDERRUN: u32 = 0xd;
}

/// How long the firmware is given to let go, a reset to finish, and a
/// root port's reset (the controller times 10 ms of it).
const HANDOFF_US: u64 = 1_000_000;
const RESET_US: u64 = 10_000;
const PORT_RESET_US: u64 = 100_000;
/// How long a frame is waited for.
const FRAME_US: u64 = 10_000;
/// The default frame interval: 12000 bit times, less 1.
const FRAME_INTERVAL: u32 = 11999;
const LEVELS: usize = 6;
const TREE: usize = (1 << LEVELS) - 1;
const MAX_PIPES: usize = 32;
const MAX_DEFAULT_PIPES: usize = 32;
/// Interrupt pipes' buffers: 1 KiB each.
const CHUNK: usize = 1024;
const BUFFER_PAGES: usize = 8;
const CHUNKS: usize = BUFFER_PAGES * PAGE_SIZE / CHUNK;
/// The control data stage's largest.
pub const CONTROL_DATA: usize = PAGE_SIZE;

/// What the driver knows of its controller, for the log.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    /// The revision it implements, in BCD (0x10 is 1.0).
    pub revision: u8,
    /// Its root ports.
    pub ports: u8,
    /// How the firmware let go of it.
    pub handoff: Handoff,
}

/// An ED and the empty TD at its tail.
#[derive(Clone, Copy)]
struct Queue {
    ed: Slot,
    tail: Slot,
}

struct Pipe {
    endpoint: Endpoint,
    queue: Queue,
    /// The TD of the transfer in flight, and the bytes it asked for.
    asked: Option<(Slot, usize)>,
    chunk: usize,
    node: usize,
}

#[derive(Clone, Copy)]
struct DefaultPipe {
    target: Target,
    max_packet: u16,
    queue: Queue,
}

/// The driver's records of its pipes, kept apart from it as the EHCI
/// driver's are ([`crate::ehci::Tables`]).
pub struct Tables {
    pipes: [Option<Pipe>; MAX_PIPES],
    defaults: [Option<DefaultPipe>; MAX_DEFAULT_PIPES],
}

impl Tables {
    /// No pipes yet.
    pub const EMPTY: Tables =
        Tables { pipes: [const { None }; MAX_PIPES], defaults: [None; MAX_DEFAULT_PIPES] };
}

/// An OHCI controller, taken from the firmware and running, with its
/// pipes' records in `T`.
pub struct Ohci<T: DerefMut<Target = Tables>> {
    regs: Mmio,
    info: Info,
    hcca: Dma,
    pool: Pool,
    /// The control list's head, which is skipped.
    head: Slot,
    tree: [Slot; TREE],
    load: [u8; TREE],
    control: Dma,
    buffers: Dma,
    chunks_used: u64,
    tables: T,
    failed: bool,
}

fn node(level: usize, phase: usize) -> usize {
    (1 << level) - 1 + phase
}

impl<T: DerefMut<Target = Tables>> Ohci<T> {
    /// Takes the controller whose registers are `regs` from the firmware,
    /// resets it and runs it, its ports powered. Memory comes from `alloc`
    /// (pages, below 4 GiB); `tables`, empty, keeps the pipes.
    pub fn new(
        regs: Mmio,
        waiter: &mut dyn Waiter,
        alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>,
        tables: T,
    ) -> Result<Ohci<T>, crate::Error> {
        if regs.len() < reg::RH_PORT_STATUS + 4 {
            return Err(crate::Error::Unsupported("its registers are too few"));
        }
        let revision = regs.read32(reg::REVISION);
        if revision == u32::MAX {
            return Err(crate::Error::Unsupported("it reads as absent"));
        }
        let ports = (regs.read32(reg::RH_DESCRIPTOR_A) & reg::NDP).min(15) as u8;
        if regs.len() < reg::RH_PORT_STATUS + 4 * usize::from(ports) || ports == 0 {
            return Err(crate::Error::Unsupported("its ports' registers are missing"));
        }
        let hcca = alloc(1, true).ok_or(crate::Error::NoMemory)?;
        let control = alloc(2, true).ok_or(crate::Error::NoMemory)?;
        let buffers = alloc(BUFFER_PAGES, true).ok_or(crate::Error::NoMemory)?;
        let mut pool = Pool::new(alloc).ok_or(crate::Error::NoMemory)?;
        let mut take = || pool.take().ok_or(crate::Error::NoMemory);
        let head = take()?;
        let mut tree = [head; TREE];
        for slot in &mut tree {
            *slot = take()?;
        }
        hcca.zero();
        let handoff = take_from_firmware(&regs, waiter);
        let interval = regs.read32(reg::FM_INTERVAL) & 0x3fff;
        let interval = if (11_000..13_000).contains(&interval) { interval } else { FRAME_INTERVAL };
        regs.write32(reg::INTERRUPT_DISABLE, u32::MAX);
        regs.write32(reg::CONTROL, reg::HCFS_RESET);
        waiter.wait(50_000);
        regs.write32(reg::COMMAND_STATUS, reg::HCR);
        if !wait_for(waiter, RESET_US, || regs.read32(reg::COMMAND_STATUS) & reg::HCR == 0) {
            return Err(crate::Error::Unresponsive("it did not reset"));
        }
        // Now suspended: operational within 2 ms (5.1.1.4), so the memory
        // was taken before.
        let mut ohci = Ohci {
            regs,
            info: Info { revision: revision as u8, ports, handoff },
            hcca,
            pool,
            head,
            tree,
            load: [0; TREE],
            control,
            buffers,
            chunks_used: 0,
            tables,
            failed: false,
        };
        ohci.build_lists();
        ohci.start(interval, waiter);
        Ok(ohci)
    }

    /// What the driver knows of the controller.
    pub fn info(&self) -> Info {
        self.info
    }

    fn build_lists(&mut self) {
        let pool = &self.pool;
        pool.write32(self.head, ed::INFO, ed::SKIP);
        for level in 0..LEVELS {
            for phase in 0..1 << level {
                let slot = self.tree[node(level, phase)];
                pool.write32(slot, ed::INFO, ed::SKIP);
                let next = if level == 0 {
                    0
                } else {
                    pool.phys(self.tree[node(level - 1, phase % (1 << (level - 1)))])
                };
                pool.write32(slot, ed::NEXT, next);
            }
        }
        for entry in 0..32 {
            let top = self.tree[node(LEVELS - 1, entry)];
            self.hcca.write32(4 * entry, self.pool.phys(top));
        }
    }

    fn start(&mut self, interval: u32, waiter: &mut dyn Waiter) {
        let regs = &self.regs;
        regs.write32(reg::HCCA, self.hcca.phys() as u32);
        regs.write32(reg::CONTROL_HEAD, self.pool.phys(self.head));
        regs.write32(reg::BULK_HEAD, 0);
        regs.write32(reg::INTERRUPT_STATUS, reg::ALL);
        // The largest packet a frame takes after its overhead (5.4).
        let largest = ((interval - 210) * 6 / 7) << 16;
        let toggle = !regs.read32(reg::FM_INTERVAL) & (1 << 31);
        regs.write32(reg::FM_INTERVAL, interval | largest | toggle);
        regs.write32(reg::PERIODIC_START, interval * 9 / 10);
        regs.write32(reg::LS_THRESHOLD, 0x628);
        device_barrier();
        regs.write32(reg::CONTROL, reg::HCFS_OPERATIONAL | reg::PLE | reg::CLE | reg::CBSR_4);
        // The ports' power: all of them at once, or each, as the root hub
        // switches it; then the time it takes to be good.
        let a = regs.read32(reg::RH_DESCRIPTOR_A);
        if a & reg::NPS == 0 {
            regs.write32(reg::RH_STATUS, reg::LPSC);
            for port in 1..=self.info.ports {
                regs.write32(Self::port_offset(port), port::SET_POWER);
            }
        }
        let good_ms = (a >> 24) * 2;
        waiter.wait(good_ms.max(20) * 1000);
    }

    fn port_offset(port: u8) -> usize {
        reg::RH_PORT_STATUS + 4 * usize::from(port - 1)
    }

    /// Waits until the controller has started another frame, after which
    /// it no longer holds an ED taken out of its lists.
    fn wait_frame(&mut self, waiter: &mut dyn Waiter) {
        let regs = &self.regs;
        let start = regs.read32(reg::FM_NUMBER) & 0xffff;
        let moved = || (regs.read32(reg::FM_NUMBER) & 0xffff) != start;
        if !wait_for(waiter, FRAME_US, moved) {
            self.failed = true;
        }
    }

    /// A new ED for `target`'s endpoint `number`, with an empty TD.
    fn new_queue(&mut self, target: Target, number: u8, max_packet: u16) -> Result<Queue, Error> {
        let ed = self.pool.take().ok_or(Error::NoMemory)?;
        let Some(tail) = self.pool.take() else {
            self.pool.give(ed);
            return Err(Error::NoMemory);
        };
        let mut info = u32::from(target.address & 0x7f)
            | u32::from(number & 0xf) << ed::ENDPOINT_SHIFT
            | u32::from(max_packet & 0x7ff) << ed::MAX_PACKET_SHIFT;
        if target.speed == Speed::Low {
            info |= ed::LOW_SPEED;
        }
        let pool = &self.pool;
        pool.write32(ed, ed::INFO, info);
        pool.write32(ed, ed::TAIL, pool.phys(tail));
        pool.write32(ed, ed::HEAD, pool.phys(tail));
        Ok(Queue { ed, tail })
    }

    /// The default pipe of `target`, made and linked in if new.
    fn default_pipe(
        &mut self,
        target: Target,
        max_packet: u16,
        waiter: &mut dyn Waiter,
    ) -> Result<Queue, Error> {
        if let Some(found) = self.tables.defaults.iter().flatten().find(|d| d.target.address == target.address)
        {
            if found.target == target && found.max_packet == max_packet {
                return Ok(found.queue);
            }
            self.forget(target.address, waiter);
        }
        let free = self.tables.defaults.iter().position(Option::is_none).ok_or(Error::NoMemory)?;
        let queue = self.new_queue(target, 0, max_packet)?;
        let pool = &self.pool;
        pool.write32(queue.ed, ed::NEXT, pool.read32(self.head, ed::NEXT));
        device_barrier();
        pool.write32(self.head, ed::NEXT, pool.phys(queue.ed));
        self.tables.defaults[free] = Some(DefaultPipe { target, max_packet, queue });
        Ok(queue)
    }

    /// Fills in the TD `slot`: `info`, the buffer from `phys`, `len` bytes
    /// (at most two pages), and the TD after it.
    fn fill_td(&self, slot: Slot, info: u32, phys: u64, len: usize, next: Slot) {
        let pool = &self.pool;
        pool.write32(slot, td::INFO, info | td::NOT_ACCESSED | td::NO_INTERRUPT);
        let (start, end) = if len == 0 { (0, 0) } else { (phys as u32, (phys + len as u64 - 1) as u32) };
        pool.write32(slot, td::BUFFER, start);
        pool.write32(slot, td::END, end);
        pool.write32(slot, td::NEXT, pool.phys(next));
    }

    /// Empties the halted or skipped queue `queue` of what is left of its
    /// TDs, `used`, and clears its halt, keeping its toggle carry unless
    /// `restart`.
    fn empty(&mut self, queue: Queue, used: &[Slot], restart: bool) {
        let pool = &self.pool;
        let head = pool.read32(queue.ed, ed::HEAD);
        let carry = if restart { 0 } else { head & ed::CARRY };
        pool.write32(queue.ed, ed::HEAD, pool.phys(queue.tail) | carry);
        for &slot in used {
            self.pool.give(slot);
        }
    }

    /// What a retired TD's condition code says.
    fn outcome(info: u32) -> Result<(), Error> {
        match info >> td::CC_SHIFT {
            cc::NO_ERROR | cc::DATA_UNDERRUN => Ok(()),
            cc::STALL => Err(Error::Stall),
            cc::DATA_OVERRUN => Err(Error::Babble),
            cc::BUFFER_OVERRUN | cc::BUFFER_UNDERRUN => Err(Error::Buffer),
            cc::NOT_RESPONDING => Err(Error::NoAnswer),
            0xe | 0xf => Err(Error::TimedOut),
            _ => Err(Error::NoAnswer),
        }
    }

    /// The bytes a retired TD for `asked` bytes from `phys` moved.
    fn moved(&self, slot: Slot, phys: u64, asked: usize) -> usize {
        // The controller holds 32-bit addresses.
        let current = self.pool.read32(slot, td::BUFFER);
        if current == 0 { asked } else { current.wrapping_sub(phys as u32) as usize }
    }

    fn take_chunk(&mut self) -> Option<usize> {
        let chunk = (0..CHUNKS).find(|&c| self.chunks_used & (1 << c) == 0)?;
        self.chunks_used |= 1 << chunk;
        Some(chunk)
    }

    fn pipe(&self, pipe: PipeId) -> Result<&Pipe, Error> {
        self.tables.pipes.get(usize::from(pipe)).and_then(Option::as_ref).ok_or(Error::BadRequest)
    }

    /// Takes pipe `index`'s ED out of the tree, and waits a frame.
    fn unhang(&mut self, index: usize, waiter: &mut dyn Waiter) {
        let Some(pipe) = &self.tables.pipes[index] else { return };
        let target = self.pool.phys(pipe.queue.ed);
        let mut at = self.tree[pipe.node];
        for _ in 0..MAX_PIPES + 1 {
            let next = self.pool.read32(at, ed::NEXT) & ed::ADDRESS;
            if next == target {
                self.pool.write32(at, ed::NEXT, self.pool.read32(pipe.queue.ed, ed::NEXT));
                break;
            }
            let Some(slot) = self.pool.slot_at(next) else { return };
            if next == 0 || self.tree.contains(&slot) {
                return;
            }
            at = slot;
        }
        device_barrier();
        self.wait_frame(waiter);
    }

    fn hang(&mut self, index: usize) {
        let Some(pipe) = &self.tables.pipes[index] else { return };
        let (ed, at) = (pipe.queue.ed, self.tree[pipe.node]);
        self.pool.write32(ed, ed::NEXT, self.pool.read32(at, ed::NEXT));
        device_barrier();
        self.pool.write32(at, ed::NEXT, self.pool.phys(ed));
    }
}

/// Takes the controller from the firmware (5.1.1.3): one that routes its
/// interrupts to SMIs is asked to let go; one whose firmware driver left
/// it running, without SMIs, is simply reset after.
fn take_from_firmware(regs: &Mmio, waiter: &mut dyn Waiter) -> Handoff {
    let control = regs.read32(reg::CONTROL);
    if control & reg::IR != 0 {
        regs.write32(reg::COMMAND_STATUS, reg::OCR);
        if wait_for(waiter, HANDOFF_US, || regs.read32(reg::CONTROL) & reg::IR == 0) {
            return Handoff::Given;
        }
        regs.write32(reg::CONTROL, regs.read32(reg::CONTROL) & !reg::IR);
        return Handoff::Taken;
    }
    if control & reg::HCFS_MASK != reg::HCFS_RESET {
        if control & reg::HCFS_MASK != reg::HCFS_OPERATIONAL {
            regs.write32(reg::CONTROL, control & !reg::HCFS_MASK | reg::HCFS_RESUME);
            waiter.wait(20_000);
        }
        return Handoff::Given;
    }
    Handoff::Free
}

impl<T: DerefMut<Target = Tables>> Hcd for Ohci<T> {
    fn kind(&self) -> &'static str {
        "OHCI"
    }

    fn ports(&self) -> u8 {
        self.info.ports
    }

    fn port(&mut self, port: u8) -> Port {
        let status = self.regs.read32(Self::port_offset(port));
        if status == u32::MAX {
            self.failed = true;
            return Port::default();
        }
        Port {
            connected: status & port::CONNECTED != 0,
            enabled: status & port::ENABLED != 0,
            connect_changed: status & port::CONNECT_CHANGE != 0,
            other_changed: status
                & (port::ENABLE_CHANGE
                    | port::SUSPEND_CHANGE
                    | port::OVER_CURRENT_CHANGE
                    | port::RESET_CHANGE)
                != 0,
            over_current: status & port::OVER_CURRENT != 0,
            companion: false,
        }
    }

    fn acknowledge(&mut self, port: u8) {
        let status = self.regs.read32(Self::port_offset(port));
        self.regs.write32(Self::port_offset(port), status & port::CHANGES);
    }

    fn reset(&mut self, port: u8, waiter: &mut dyn Waiter) -> Result<Option<Speed>, Error> {
        let offset = Self::port_offset(port);
        let regs = &self.regs;
        if regs.read32(offset) & port::CONNECTED == 0 {
            return Err(Error::Gone);
        }
        regs.write32(offset, port::SET_RESET);
        let done = || regs.read32(offset) & (port::RESET_CHANGE | port::RESETTING) == port::RESET_CHANGE;
        if !wait_for(waiter, PORT_RESET_US, done) {
            return Err(Error::TimedOut);
        }
        regs.write32(offset, port::RESET_CHANGE);
        let status = regs.read32(offset);
        if status & port::CONNECTED == 0 {
            return Err(Error::Gone);
        }
        if status & port::ENABLED == 0 {
            return Err(Error::NoAnswer);
        }
        Ok(Some(if status & port::LOW_SPEED != 0 { Speed::Low } else { Speed::Full }))
    }

    fn disable(&mut self, port: u8) {
        self.regs.write32(Self::port_offset(port), port::CLEAR_ENABLE);
    }

    fn control(
        &mut self,
        target: Target,
        max_packet: u16,
        setup: Setup,
        data: &mut [u8],
        timeout_ms: u32,
        waiter: &mut dyn Waiter,
    ) -> Result<usize, Error> {
        let length = usize::from(setup.length).min(data.len());
        if self.failed {
            return Err(Error::Halted);
        }
        if length > CONTROL_DATA {
            return Err(Error::BadRequest);
        }
        let queue = self.default_pipe(target, max_packet, waiter)?;
        let into_host = setup.device_to_host();
        self.control.write(0, &setup.to_u64().to_le_bytes());
        if !into_host && length > 0 {
            self.control.write(PAGE_SIZE, &data[..length]);
        }
        // The empty TD at the tail takes the SETUP stage; two more and a
        // new empty one follow.
        let first = queue.tail;
        let (middle, last, tail) = match (self.pool.take(), self.pool.take(), self.pool.take()) {
            (Some(middle), Some(last), Some(tail)) => (middle, last, tail),
            taken => {
                for slot in [taken.0, taken.1, taken.2].into_iter().flatten() {
                    self.pool.give(slot);
                }
                return Err(Error::NoMemory);
            }
        };
        let data_phys = self.control.phys() + PAGE_SIZE as u64;
        let after_setup = if length > 0 { middle } else { last };
        self.fill_td(first, td::SETUP | td::DATA0, self.control.phys(), 8, after_setup);
        let pid = if into_host { td::IN } else { td::OUT };
        if length > 0 {
            self.fill_td(middle, pid | td::DATA1 | td::ROUNDING, data_phys, length, last);
        }
        let status_pid = if into_host && length > 0 { td::OUT } else { td::IN };
        self.fill_td(last, status_pid | td::DATA1, 0, 0, tail);
        let queue = Queue { ed: queue.ed, tail };
        if let Some(d) = self.tables.defaults.iter_mut().flatten().find(|d| d.target.address == target.address)
        {
            d.queue = queue;
        }
        device_barrier();
        self.pool.write32(queue.ed, ed::TAIL, self.pool.phys(tail));
        self.regs.write32(reg::COMMAND_STATUS, reg::CLF);
        let (pool, tail_phys) = (&self.pool, self.pool.phys(tail));
        let done = || {
            let head = pool.read32(queue.ed, ed::HEAD);
            head & ed::HALTED != 0 || head & ed::ADDRESS == tail_phys
        };
        let finished = wait_for(waiter, u64::from(timeout_ms) * 1000, done);
        device_barrier();
        self.regs.write32(reg::INTERRUPT_STATUS, reg::WDH);
        let used: &[Slot] = if length > 0 { &[first, middle, last] } else { &[first, last] };
        if length == 0 {
            self.pool.give(middle);
        }
        if !finished {
            self.forget(target.address, waiter);
            for &slot in used {
                self.pool.give(slot);
            }
            return Err(Error::TimedOut);
        }
        let halted = self.pool.read32(queue.ed, ed::HEAD) & ed::HALTED != 0;
        let mut result = Ok(());
        for &slot in used {
            let info = self.pool.read32(slot, td::INFO);
            if let Err(err) = Self::outcome(info) {
                result = Err(err);
                break;
            }
        }
        let came = if length > 0 { self.moved(middle, data_phys, length) } else { 0 };
        if halted || result.is_err() {
            self.empty(queue, used, true);
            result?;
            return Err(Error::Stall);
        }
        for &slot in used {
            self.pool.give(slot);
        }
        if into_host {
            self.control.read(PAGE_SIZE, &mut data[..came.min(length)]);
        }
        Ok(came.min(length))
    }

    fn open(&mut self, endpoint: Endpoint, buffer: usize) -> Result<PipeId, Error> {
        if endpoint.kind != TransferType::Interrupt || buffer > CHUNK {
            return Err(Error::BadRequest);
        }
        let index = self.tables.pipes.iter().position(Option::is_none).ok_or(Error::NoMemory)?;
        let chunk = self.take_chunk().ok_or(Error::NoMemory)?;
        let queue = match self.new_queue(endpoint.target, endpoint.number, endpoint.max_packet) {
            Ok(queue) => queue,
            Err(err) => {
                self.chunks_used &= !(1 << chunk);
                return Err(err);
            }
        };
        let level = ((endpoint.period_microframes() / 8).max(1).trailing_zeros() as usize).min(LEVELS - 1);
        let phase = (0..1 << level).min_by_key(|&p| self.load[node(level, p)]).unwrap_or(0);
        let node = node(level, phase);
        self.load[node] = self.load[node].saturating_add(1);
        self.tables.pipes[index] = Some(Pipe { endpoint, queue, asked: None, chunk, node });
        self.hang(index);
        Ok(index as PipeId)
    }

    fn start(&mut self, pipe: PipeId, data: Data<'_>) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Halted);
        }
        let (queue, chunk, into_host) = {
            let p = self.pipe(pipe)?;
            if p.asked.is_some() {
                return Err(Error::BadRequest);
            }
            (p.queue, p.chunk, p.endpoint.into_host)
        };
        let length = match data {
            Data::In(len) if into_host && len <= CHUNK => len,
            Data::Out(bytes) if !into_host && bytes.len() <= CHUNK => {
                self.buffers.write(chunk * CHUNK, bytes);
                bytes.len()
            }
            _ => return Err(Error::BadRequest),
        };
        let tail = self.pool.take().ok_or(Error::NoMemory)?;
        let phys = self.buffers.phys() + (chunk * CHUNK) as u64;
        let pid = if into_host { td::IN | td::ROUNDING } else { td::OUT };
        // The toggle comes from the ED's carry.
        self.fill_td(queue.tail, pid, phys, length, tail);
        device_barrier();
        self.pool.write32(queue.ed, ed::TAIL, self.pool.phys(tail));
        if let Some(Some(p)) = self.tables.pipes.get_mut(usize::from(pipe)) {
            p.asked = Some((queue.tail, length));
            p.queue.tail = tail;
        }
        Ok(())
    }

    fn poll(&mut self, pipe: PipeId) -> Option<Result<usize, Error>> {
        let p = self.tables.pipes.get(usize::from(pipe))?.as_ref()?;
        let (slot, asked) = p.asked?;
        let queue = p.queue;
        let head = self.pool.read32(queue.ed, ed::HEAD);
        let halted = head & ed::HALTED != 0;
        if !halted && head & ed::ADDRESS != self.pool.phys(queue.tail) && !self.failed {
            return None;
        }
        device_barrier();
        self.regs.write32(reg::INTERRUPT_STATUS, reg::WDH);
        let chunk = p.chunk;
        if let Some(Some(p)) = self.tables.pipes.get_mut(usize::from(pipe)) {
            p.asked = None;
        }
        if self.failed {
            return Some(Err(Error::Halted));
        }
        let info = self.pool.read32(slot, td::INFO);
        let phys = self.buffers.phys() + (chunk * CHUNK) as u64;
        let result = Self::outcome(info).map(|()| self.moved(slot, phys, asked).min(asked));
        if halted {
            // The halt stays until the pipe is reset.
            self.pool.give(slot);
            return Some(result.and(Err(Error::Stall)));
        }
        self.pool.give(slot);
        Some(result)
    }

    fn received(&self, pipe: PipeId, out: &mut [u8]) {
        if let Ok(p) = self.pipe(pipe) {
            let len = out.len().min(CHUNK);
            self.buffers.read(p.chunk * CHUNK, &mut out[..len]);
        }
    }

    fn cancel(&mut self, pipe: PipeId, waiter: &mut dyn Waiter) {
        let index = usize::from(pipe);
        let Ok(p) = self.pipe(pipe) else { return };
        let Some((slot, _)) = p.asked else { return };
        let queue = p.queue;
        let info = self.pool.read32(queue.ed, ed::INFO);
        self.pool.write32(queue.ed, ed::INFO, info | ed::SKIP);
        self.wait_frame(waiter);
        self.empty(queue, &[slot], false);
        self.pool.write32(queue.ed, ed::INFO, info);
        if let Some(Some(p)) = self.tables.pipes.get_mut(index) {
            p.asked = None;
        }
    }

    fn reset_pipe(&mut self, pipe: PipeId) {
        let Ok(p) = self.pipe(pipe) else { return };
        if p.asked.is_some() {
            return;
        }
        let queue = p.queue;
        // Halted or idle, the controller leaves it: DATA0 again.
        self.pool.write32(queue.ed, ed::HEAD, self.pool.phys(queue.tail));
    }

    fn close(&mut self, pipe: PipeId, waiter: &mut dyn Waiter) {
        let index = usize::from(pipe);
        if self.pipe(pipe).is_err() {
            return;
        }
        self.unhang(index, waiter);
        if let Some(p) = self.tables.pipes[index].take() {
            self.load[p.node] = self.load[p.node].saturating_sub(1);
            self.chunks_used &= !(1 << p.chunk);
            if let Some((slot, _)) = p.asked {
                self.pool.give(slot);
            }
            self.pool.give(p.queue.tail);
            self.pool.give(p.queue.ed);
        }
    }

    fn forget(&mut self, address: u8, waiter: &mut dyn Waiter) {
        let Some(at) =
            self.tables.defaults.iter().position(|d| d.is_some_and(|d| d.target.address == address))
        else {
            return;
        };
        let Some(pipe) = self.tables.defaults[at].take() else { return };
        // The control list off for a frame while its ED is taken out
        // (5.2.7.1.2).
        let control = self.regs.read32(reg::CONTROL);
        self.regs.write32(reg::CONTROL, control & !reg::CLE);
        self.wait_frame(waiter);
        let target = self.pool.phys(pipe.queue.ed);
        let mut at = self.head;
        for _ in 0..MAX_DEFAULT_PIPES + 1 {
            let next = self.pool.read32(at, ed::NEXT) & ed::ADDRESS;
            if next == target {
                self.pool.write32(at, ed::NEXT, self.pool.read32(pipe.queue.ed, ed::NEXT));
                break;
            }
            match self.pool.slot_at(next) {
                Some(slot) if next != 0 => at = slot,
                _ => break,
            }
        }
        self.regs.write32(reg::CONTROL_CURRENT, 0);
        self.regs.write32(reg::CONTROL, control);
        self.pool.give(pipe.queue.tail);
        self.pool.give(pipe.queue.ed);
    }

    fn failed(&mut self) -> bool {
        if !self.failed {
            let status = self.regs.read32(reg::INTERRUPT_STATUS);
            let state = self.regs.read32(reg::CONTROL) & reg::HCFS_MASK;
            if status == u32::MAX || status & reg::UE != 0 || state != reg::HCFS_OPERATIONAL {
                self.failed = true;
            }
        }
        self.failed
    }

    fn halt(&mut self, _waiter: &mut dyn Waiter) {
        stop(&self.regs);
        self.failed = true;
    }
}

/// Puts the controller whose registers are `regs` in its reset state, in
/// which it reads and writes no memory: before the machine is powered off
/// or restarted.
pub fn stop(regs: &Mmio) -> bool {
    regs.write32(reg::INTERRUPT_DISABLE, u32::MAX);
    regs.write32(reg::CONTROL, reg::HCFS_RESET);
    regs.read32(reg::CONTROL) & reg::HCFS_MASK == reg::HCFS_RESET
}
