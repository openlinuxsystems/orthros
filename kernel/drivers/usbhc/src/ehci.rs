// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! EHCI, USB 2.0's high-speed host controller (Intel's *Enhanced Host
//! Controller Interface Specification for Universal Serial Bus*, revision
//! 1.0; ADR 0243).
//!
//! An EHCI controller is one PCI function with its registers in BAR 0:
//! capability registers, then operational ones with a status and control
//! register for each root port. It reads its work from two schedules in
//! memory:
//!
//! - the *asynchronous schedule*, a ring of queue heads it visits over and
//!   over, for control and bulk transfers: here a head that does nothing
//!   (it has the H bit), and a queue head for each device's default pipe,
//!   kept while the device is there;
//! - the *periodic schedule*, a list of 1024 frames, each pointing at the
//!   queue heads of the interrupt pipes to poll in that millisecond. The
//!   frames lead into a tree of 63 empty queue heads, one for each phase of
//!   each period of 1, 2, 4, 8, 16 and 32 ms (FreeBSD's layout): frame `f`
//!   starts at the 32 ms one of phase `f % 32`, which leads to the 16 ms
//!   one of phase `f % 16`, and so on to the 1 ms one. A pipe polled every
//!   `p` ms hangs after the `p` ms head of the phase that has the fewest.
//!
//! A queue head says where the endpoint is (its device's address and
//! speed, its packet size, and for a low- or full-speed device behind a
//! high-speed hub, the hub and port whose Transaction Translator reaches
//! it, which the controller talks to in split transactions), and holds the
//! transfer descriptor (qTD) it works on. A qTD moves up to 20 KiB in one
//! direction; a control transfer is a SETUP qTD, a data one, and a status
//! one the other way.
//!
//! The driver is polled: the kernel turns off the function's INTx
//! interrupt, and its thread looks at the ports and transfers every few
//! milliseconds. A device that is not high speed shows itself before or
//! after its port's reset, and is handed to the companion controller
//! (Port Owner), if the controller has one; Intel's chipsets from the 6
//! Series on have none, and a hub inside, on each port, which talks to
//! the slower devices.

use radix_hw::pci::Function;
use radix_hw::{Dma, Mmio, PAGE_SIZE, device_barrier};
use radix_usb::host::{Data, Endpoint, Error, Hcd, PipeId, Port, Target, Waiter};
use radix_usb::{Setup, Speed, TransferType};

use crate::{Handoff, Pool, Slot, wait_for};

/// The PCI class, subclass and programming interface of an EHCI
/// controller: serial bus, USB, EHCI.
pub const PCI_CLASS: (u8, u8, u8) = (0x0c, 0x03, 0x20);
/// The BAR that holds its registers.
pub const BAR: usize = 0;

/// The capability registers (2.2).
mod cap {
    /// CAPLENGTH in bits 0 to 7, HCIVERSION in 16 to 31.
    pub const LENGTH_VERSION: usize = 0x00;
    pub const HCSPARAMS: usize = 0x04;
    pub const HCCPARAMS: usize = 0x08;

    /// HCSPARAMS: the ports, port power control, the companions.
    pub const N_PORTS: u32 = 0xf;
    pub const PPC: u32 = 1 << 4;
    pub const N_CC_SHIFT: u32 = 12;
    /// HCCPARAMS: 64-bit structures; the extended capabilities' place in
    /// configuration space, bits 8 to 15.
    pub const AC64: u32 = 1 << 0;
    pub const EECP_SHIFT: u32 = 8;
}

/// The operational registers (2.3), from CAPLENGTH.
mod op {
    pub const USBCMD: usize = 0x00;
    pub const USBSTS: usize = 0x04;
    pub const USBINTR: usize = 0x08;
    pub const FRINDEX: usize = 0x0c;
    pub const CTRLDSSEGMENT: usize = 0x10;
    pub const PERIODICLISTBASE: usize = 0x14;
    pub const ASYNCLISTADDR: usize = 0x18;
    pub const CONFIGFLAG: usize = 0x40;
    pub const PORTSC: usize = 0x44;

    /// USBCMD: run, reset, the schedules on, the doorbell that asks the
    /// controller to say when it has left the asynchronous schedule, and
    /// the interrupt threshold of 8 microframes.
    pub const RUN: u32 = 1 << 0;
    pub const HCRESET: u32 = 1 << 1;
    pub const PSE: u32 = 1 << 4;
    pub const ASE: u32 = 1 << 5;
    pub const IAAD: u32 = 1 << 6;
    pub const ITC_8: u32 = 8 << 16;
    /// USBSTS: a host system error, the doorbell answered, halted, the
    /// schedules running.
    pub const HSE: u32 = 1 << 4;
    pub const IAA: u32 = 1 << 5;
    pub const HALTED: u32 = 1 << 12;
    pub const PSS: u32 = 1 << 14;
    pub const ASS: u32 = 1 << 15;
    /// What a write of 1 clears in USBSTS.
    pub const STS_CLEAR: u32 = 0x3f;
}

/// A root port's status and control register (2.3.9).
mod portsc {
    pub const CONNECTED: u32 = 1 << 0;
    pub const CONNECT_CHANGE: u32 = 1 << 1;
    pub const ENABLED: u32 = 1 << 2;
    pub const ENABLE_CHANGE: u32 = 1 << 3;
    pub const OVER_CURRENT: u32 = 1 << 4;
    pub const OVER_CURRENT_CHANGE: u32 = 1 << 5;
    pub const RESET: u32 = 1 << 8;
    /// Line status, bits 10 and 11: K-state (01) is a low-speed device.
    pub const LINE_SHIFT: u32 = 10;
    pub const LINE_K: u32 = 0b01;
    pub const POWER: u32 = 1 << 12;
    pub const OWNER: u32 = 1 << 13;
    /// The bits a write of 1 clears.
    pub const CHANGES: u32 = CONNECT_CHANGE | ENABLE_CHANGE | OVER_CURRENT_CHANGE;
}

/// The legacy support capability in configuration space (2.1.7, 2.1.8).
mod legacy {
    pub const ID: u8 = 1;
    /// In USBLEGSUP: the firmware's semaphore (byte 2) and the system's
    /// (byte 3).
    pub const BIOS_OWNED: usize = 2;
    pub const OS_OWNED: usize = 3;
    /// USBLEGCTLSTS, after USBLEGSUP: its SMI status bits, which a write
    /// of 1 clears, and enables, written 0.
    pub const CTLSTS: usize = 4;
    pub const SMI_STATUS: u32 = 0xe000_0000;
}

/// Link pointers (3.1): an address, a type, and T, which ends the list.
mod link {
    pub const TERMINATE: u32 = 1;
    pub const QH: u32 = 1 << 1;
    pub const ADDRESS: u32 = !0x1f;
}

/// A queue head's words (3.6), by offset.
mod qh {
    pub const LINK: usize = 0x00;
    /// Endpoint characteristics.
    pub const CHARS: usize = 0x04;
    /// Endpoint capabilities.
    pub const CAPS: usize = 0x08;
    /// The overlay: the qTD the controller works on, copied in.
    pub const NEXT: usize = 0x10;
    pub const ALT_NEXT: usize = 0x14;
    pub const TOKEN: usize = 0x18;

    /// CHARS: the address (bits 0 to 6), the endpoint (8 to 11), its speed
    /// (12 and 13: full, low, high), the data toggle from the qTD, the head
    /// of the asynchronous schedule, the packet size (16 to 26), and a
    /// control endpoint that is not high speed.
    pub const ENDPOINT_SHIFT: u32 = 8;
    pub const SPEED_SHIFT: u32 = 12;
    pub const SPEED_FULL: u32 = 0;
    pub const SPEED_LOW: u32 = 1;
    pub const SPEED_HIGH: u32 = 2;
    pub const DTC: u32 = 1 << 14;
    pub const HEAD: u32 = 1 << 15;
    pub const MAX_PACKET_SHIFT: u32 = 16;
    pub const CONTROL: u32 = 1 << 27;
    /// CAPS: the microframes a periodic transfer starts in (bits 0 to 7)
    /// and its split transactions complete in (8 to 15), the hub and its
    /// port (16 to 22, 23 to 29), and the transactions in a microframe
    /// (30 and 31).
    pub const C_MASK_SHIFT: u32 = 8;
    pub const HUB_SHIFT: u32 = 16;
    pub const PORT_SHIFT: u32 = 23;
    pub const MULT_SHIFT: u32 = 30;
}

/// A qTD's words (3.5), by offset.
mod qtd {
    pub const NEXT: usize = 0x00;
    pub const ALT_NEXT: usize = 0x04;
    pub const TOKEN: usize = 0x08;
    /// Five buffer pointers, then (64-bit structures) their high words.
    pub const BUFFER: usize = 0x0c;
    pub const BUFFER_HIGH: usize = 0x20;

    /// TOKEN: status bits.
    pub const ACTIVE: u32 = 1 << 7;
    pub const HALTED: u32 = 1 << 6;
    pub const BUFFER_ERROR: u32 = 1 << 5;
    pub const BABBLE: u32 = 1 << 4;
    pub const TRANSACTION_ERROR: u32 = 1 << 3;
    pub const MISSED_MICROFRAME: u32 = 1 << 2;
    /// TOKEN: the PID (bits 8 and 9), three tries (10 and 11), interrupt
    /// on completion, the bytes (16 to 30), the data toggle.
    pub const PID_OUT: u32 = 0 << 8;
    pub const PID_IN: u32 = 1 << 8;
    pub const PID_SETUP: u32 = 2 << 8;
    pub const CERR_3: u32 = 3 << 10;
    pub const IOC: u32 = 1 << 15;
    pub const BYTES_SHIFT: u32 = 16;
    pub const BYTES_MASK: u32 = 0x7fff;
    pub const TOGGLE: u32 = 1 << 31;
}

/// How long a halt, a reset and a root port's reset take (2.3.1; USB 2.0
/// 7.1.7.5 asks 50 ms of a root port's reset).
const HALT_US: u64 = 20_000;
const RESET_US: u64 = 250_000;
const PORT_RESET_US: u32 = 50_000;
const PORT_RESET_END_US: u64 = 10_000;
/// How long the firmware is given to let go (Linux gives it a second).
const HANDOFF_US: u64 = 1_000_000;
/// How long the controller is given to leave the asynchronous schedule,
/// or a frame to pass.
const DOORBELL_US: u64 = 10_000;
/// The periodic tree's levels: 1 to 32 ms.
const LEVELS: usize = 6;
const TREE: usize = (1 << LEVELS) - 1;
/// The pipes and default pipes kept, at most.
const MAX_PIPES: usize = 32;
const MAX_DEFAULT_PIPES: usize = 32;
/// The buffers of interrupt pipes: 1 KiB each, in [`BUFFER_PAGES`] pages.
const CHUNK: usize = 1024;
const BUFFER_PAGES: usize = 8;
const CHUNKS: usize = BUFFER_PAGES * PAGE_SIZE / CHUNK;
/// The control data stage's largest.
pub const CONTROL_DATA: usize = PAGE_SIZE;

/// What the driver knows of its controller, for the log.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    /// HCIVERSION, in BCD (0x0100 is 1.0).
    pub version: u16,
    /// Its root ports.
    pub ports: u8,
    /// Its companion controllers, which take its slower devices.
    pub companions: u8,
    /// Whether it reads 64-bit structures.
    pub wide: bool,
    /// How the firmware let go of it.
    pub handoff: Handoff,
}

/// An interrupt pipe.
struct Pipe {
    endpoint: Endpoint,
    qh: Slot,
    qtd: Slot,
    /// Its buffer, a chunk of [`Ehci::buffers`].
    chunk: usize,
    /// Its place in the periodic tree.
    node: usize,
    /// The bytes the transfer in flight asked for, if one is.
    asked: Option<usize>,
}

/// A device's default pipe: its queue head in the asynchronous schedule.
#[derive(Clone, Copy)]
struct DefaultPipe {
    target: Target,
    max_packet: u16,
    qh: Slot,
}

/// An EHCI controller, taken from the firmware and started.
pub struct Ehci {
    op: Mmio,
    info: Info,
    port_power: bool,
    frames: Dma,
    pool: Pool,
    /// The asynchronous schedule's head.
    head: Slot,
    /// The periodic tree, its 1 ms head first, then the 2 ms ones, and so
    /// on, and how many pipes hang after each.
    tree: [Slot; TREE],
    load: [u8; TREE],
    /// The SETUP, data and status qTDs of the control transfer under way.
    control_qtds: [Slot; 3],
    /// The control transfer's setup bytes (at 0) and data (a page on).
    control: Dma,
    buffers: Dma,
    chunks_used: u64,
    pipes: [Option<Pipe>; MAX_PIPES],
    defaults: [Option<DefaultPipe>; MAX_DEFAULT_PIPES],
    failed: bool,
}

/// Where the tree's head of period `1 << level` ms and phase `phase` is in
/// [`Ehci::tree`].
fn node(level: usize, phase: usize) -> usize {
    (1 << level) - 1 + phase
}

impl Ehci {
    /// Takes the controller of `function`, whose registers are `regs`,
    /// from the firmware, resets it, and starts it with both schedules on
    /// and every port routed to it. Memory comes from `alloc` (pages,
    /// below 4 GiB).
    pub fn new(
        function: &Function,
        regs: Mmio,
        waiter: &mut dyn Waiter,
        alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>,
    ) -> Result<Ehci, crate::Error> {
        if regs.len() < 0x20 {
            return Err(crate::Error::Unsupported("its registers are too few"));
        }
        let length_version = regs.read32(cap::LENGTH_VERSION);
        let caplength = (length_version & 0xff) as usize;
        let hcsparams = regs.read32(cap::HCSPARAMS);
        let hccparams = regs.read32(cap::HCCPARAMS);
        let ports = (hcsparams & cap::N_PORTS) as u8;
        if length_version == u32::MAX || ports == 0 {
            return Err(crate::Error::Unsupported("it reads as absent"));
        }
        let op = regs
            .window(caplength, op::PORTSC + 4 * usize::from(ports))
            .ok_or(crate::Error::Unsupported("its registers are too few"))?;
        let eecp = ((hccparams >> cap::EECP_SHIFT) & 0xff) as usize;
        let handoff = take_from_firmware(function, eecp, waiter);
        let info = Info {
            version: (length_version >> 16) as u16,
            ports,
            companions: ((hcsparams >> cap::N_CC_SHIFT) & 0xf) as u8,
            wide: hccparams & cap::AC64 != 0,
            handoff,
        };
        // Halted, then reset.
        op.write32(op::USBCMD, op.read32(op::USBCMD) & !op::RUN);
        if !wait_for(waiter, HALT_US, || op.read32(op::USBSTS) & op::HALTED != 0) {
            return Err(crate::Error::Unresponsive("it did not halt"));
        }
        op.write32(op::USBCMD, op::HCRESET);
        if !wait_for(waiter, RESET_US, || op.read32(op::USBCMD) & op::HCRESET == 0) {
            return Err(crate::Error::Unresponsive("it did not reset"));
        }
        let frames = alloc(1, true).ok_or(crate::Error::NoMemory)?;
        let control = alloc(2, true).ok_or(crate::Error::NoMemory)?;
        let buffers = alloc(BUFFER_PAGES, true).ok_or(crate::Error::NoMemory)?;
        let mut pool = Pool::new(alloc).ok_or(crate::Error::NoMemory)?;
        let mut take = || pool.take().ok_or(crate::Error::NoMemory);
        let head = take()?;
        let mut tree = [head; TREE];
        for slot in &mut tree {
            *slot = take()?;
        }
        let control_qtds = [take()?, take()?, take()?];
        let mut ehci = Ehci {
            op,
            info,
            port_power: hcsparams & cap::PPC != 0,
            frames,
            pool,
            head,
            tree,
            load: [0; TREE],
            control_qtds,
            control,
            buffers,
            chunks_used: 0,
            pipes: [const { None }; MAX_PIPES],
            defaults: [None; MAX_DEFAULT_PIPES],
            failed: false,
        };
        ehci.build_schedules();
        ehci.start(waiter)?;
        Ok(ehci)
    }

    /// What the driver knows of the controller.
    pub fn info(&self) -> Info {
        self.info
    }

    /// Makes the asynchronous schedule's head, the periodic tree and the
    /// frame list.
    fn build_schedules(&mut self) {
        // Queue heads that do nothing: high speed (so no split state),
        // one transaction, no microframes, and a halted overlay.
        let idle = |pool: &Pool, slot: Slot, chars: u32| {
            pool.write32(slot, qh::CHARS, chars | qh::SPEED_HIGH << qh::SPEED_SHIFT);
            pool.write32(slot, qh::CAPS, 1 << qh::MULT_SHIFT);
            pool.write32(slot, qh::NEXT, link::TERMINATE);
            pool.write32(slot, qh::ALT_NEXT, link::TERMINATE);
            pool.write32(slot, qh::TOKEN, qtd::HALTED);
        };
        idle(&self.pool, self.head, qh::HEAD);
        let head = self.pool.phys(self.head);
        self.pool.write32(self.head, qh::LINK, head | link::QH);
        for level in 0..LEVELS {
            for phase in 0..1 << level {
                let slot = self.tree[node(level, phase)];
                idle(&self.pool, slot, 0);
                let next = if level == 0 {
                    link::TERMINATE
                } else {
                    let parent = self.tree[node(level - 1, phase % (1 << (level - 1)))];
                    self.pool.phys(parent) | link::QH
                };
                self.pool.write32(slot, qh::LINK, next);
            }
        }
        for frame in 0..1024 {
            let top = self.tree[node(LEVELS - 1, frame % (1 << (LEVELS - 1)))];
            self.frames.write32(4 * frame, self.pool.phys(top) | link::QH);
        }
    }

    /// Starts the controller with both schedules on, routes every port to
    /// it and powers them.
    fn start(&mut self, waiter: &mut dyn Waiter) -> Result<(), crate::Error> {
        let op = &self.op;
        if self.info.wide {
            op.write32(op::CTRLDSSEGMENT, 0);
        }
        op.write32(op::USBINTR, 0);
        op.write32(op::USBSTS, op::STS_CLEAR);
        op.write32(op::PERIODICLISTBASE, self.frames.phys() as u32);
        op.write32(op::ASYNCLISTADDR, self.pool.phys(self.head));
        device_barrier();
        op.write32(op::USBCMD, op::ITC_8 | op::RUN);
        if !wait_for(waiter, HALT_US, || op.read32(op::USBSTS) & op::HALTED == 0) {
            return Err(crate::Error::Unresponsive("it did not start"));
        }
        op.write32(op::USBCMD, op::ITC_8 | op::RUN | op::PSE | op::ASE);
        let running = op::PSS | op::ASS;
        if !wait_for(waiter, HALT_US, || op.read32(op::USBSTS) & running == running) {
            return Err(crate::Error::Unresponsive("its schedules did not start"));
        }
        op.write32(op::CONFIGFLAG, 1);
        // The ports' power, if the driver switches it, then the time it
        // takes to be good (Linux waits 20 ms).
        if self.port_power {
            for port in 1..=self.info.ports {
                let status = self.read_port(port);
                self.write_port(port, status | portsc::POWER);
            }
        }
        waiter.wait(20_000);
        Ok(())
    }

    fn port_offset(port: u8) -> usize {
        op::PORTSC + 4 * usize::from(port - 1)
    }

    fn read_port(&self, port: u8) -> u32 {
        self.op.read32(Self::port_offset(port))
    }

    /// Writes `value` to port `port`'s register, clearing none of its
    /// changes.
    fn write_port(&self, port: u8, value: u32) {
        self.op.write32(Self::port_offset(port), value & !portsc::CHANGES);
    }

    /// Hands the device on `port` to the companion controller, if there
    /// is one.
    fn to_companion(&mut self, port: u8) -> Result<Option<Speed>, Error> {
        if self.info.companions == 0 {
            return Err(Error::NoCompanion);
        }
        let status = self.read_port(port);
        self.write_port(port, status | portsc::OWNER);
        Ok(None)
    }

    /// Unlinks the queue head `slot` from the asynchronous schedule, and
    /// waits until the controller has left it.
    fn unlink_async(&mut self, slot: Slot, waiter: &mut dyn Waiter) {
        let target = self.pool.phys(slot);
        let mut at = self.head;
        // At most every slot in the ring.
        for _ in 0..crate::POOL_WORDS * 64 {
            let next = self.pool.read32(at, qh::LINK);
            let Some(next_slot) = self.pool.slot_at(next & link::ADDRESS) else { return };
            if next & link::ADDRESS == target {
                self.pool.write32(at, qh::LINK, self.pool.read32(slot, qh::LINK));
                break;
            }
            if next_slot == self.head {
                return;
            }
            at = next_slot;
        }
        device_barrier();
        let op = &self.op;
        if op.read32(op::USBSTS) & op::ASS == 0 {
            return;
        }
        op.write32(op::USBSTS, op::IAA);
        op.write32(op::USBCMD, op.read32(op::USBCMD) | op::IAAD);
        if !wait_for(waiter, DOORBELL_US, || op.read32(op::USBSTS) & op::IAA != 0) {
            self.failed = true;
        }
        op.write32(op::USBSTS, op::IAA);
    }

    /// Links the queue head `slot` into the asynchronous schedule, after
    /// its head.
    fn link_async(&mut self, slot: Slot) {
        self.pool.write32(slot, qh::LINK, self.pool.read32(self.head, qh::LINK));
        device_barrier();
        self.pool.write32(self.head, qh::LINK, self.pool.phys(slot) | link::QH);
    }

    /// Waits for the periodic schedule to move on two frames, after which
    /// the controller no longer holds a queue head unlinked from it.
    fn wait_frames(&mut self, waiter: &mut dyn Waiter) {
        let op = &self.op;
        if op.read32(op::USBSTS) & op::PSS == 0 {
            return;
        }
        let start = op.read32(op::FRINDEX);
        let moved = || (op.read32(op::FRINDEX).wrapping_sub(start) & 0x3fff) >= 16;
        if !wait_for(waiter, DOORBELL_US, moved) {
            self.failed = true;
        }
    }

    /// Endpoint characteristics and capabilities for a queue head.
    fn words(target: Target, number: u8, max_packet: u16, control: bool) -> (u32, u32) {
        let speed = match target.speed {
            Speed::Low => qh::SPEED_LOW,
            Speed::Full => qh::SPEED_FULL,
            _ => qh::SPEED_HIGH,
        };
        let mut chars = u32::from(target.address & 0x7f)
            | u32::from(number & 0xf) << qh::ENDPOINT_SHIFT
            | speed << qh::SPEED_SHIFT
            | u32::from(max_packet & 0x7ff) << qh::MAX_PACKET_SHIFT;
        if control {
            chars |= qh::DTC;
            if target.speed != Speed::High {
                chars |= qh::CONTROL;
            }
        }
        let mut caps = 1 << qh::MULT_SHIFT;
        if let Some(tt) = target.tt.filter(|_| target.speed != Speed::High) {
            caps |=
                u32::from(tt.hub & 0x7f) << qh::HUB_SHIFT | u32::from(tt.port & 0x7f) << qh::PORT_SHIFT;
        }
        (chars, caps)
    }

    /// The default pipe of `target`, made and linked in if it is new;
    /// one kept for the same address but another speed or packet size is
    /// made again.
    fn default_pipe(
        &mut self,
        target: Target,
        max_packet: u16,
        waiter: &mut dyn Waiter,
    ) -> Result<Slot, Error> {
        if let Some(found) = self.defaults.iter().flatten().find(|d| d.target.address == target.address)
        {
            if found.target == target && found.max_packet == max_packet {
                return Ok(found.qh);
            }
            self.forget(target.address, waiter);
        }
        let free = self.defaults.iter().position(Option::is_none).ok_or(Error::NoMemory)?;
        let slot = self.pool.take().ok_or(Error::NoMemory)?;
        let (chars, caps) = Self::words(target, 0, max_packet, true);
        self.pool.write32(slot, qh::CHARS, chars);
        self.pool.write32(slot, qh::CAPS, caps);
        self.pool.write32(slot, qh::NEXT, link::TERMINATE);
        self.pool.write32(slot, qh::ALT_NEXT, link::TERMINATE);
        self.link_async(slot);
        self.defaults[free] = Some(DefaultPipe { target, max_packet, qh: slot });
        Ok(slot)
    }

    /// Fills in qTD `slot`: `token`, and the buffer from physical address
    /// `phys`, `len` bytes, which may cross into the following pages.
    fn fill_qtd(&self, slot: Slot, next: u32, token: u32, phys: u64, len: usize) {
        let pool = &self.pool;
        pool.write32(slot, qtd::NEXT, next);
        pool.write32(slot, qtd::ALT_NEXT, link::TERMINATE);
        for page in 0..5 {
            let address = if page == 0 {
                phys
            } else {
                (phys & !(PAGE_SIZE as u64 - 1)) + (page * PAGE_SIZE) as u64
            };
            let used = page == 0 || (phys as usize % PAGE_SIZE) + len > page * PAGE_SIZE;
            let address = if used { address } else { 0 };
            pool.write32(slot, qtd::BUFFER + 4 * page, address as u32);
            if self.info.wide {
                pool.write32(slot, qtd::BUFFER_HIGH + 4 * page, (address >> 32) as u32);
            }
        }
        pool.write32(slot, qtd::TOKEN, token | ((len as u32) << qtd::BYTES_SHIFT));
    }

    /// What a finished qTD's status says.
    fn outcome(token: u32) -> Result<(), Error> {
        if token & qtd::HALTED == 0 {
            return Ok(());
        }
        Err(if token & qtd::BABBLE != 0 {
            Error::Babble
        } else if token & qtd::BUFFER_ERROR != 0 {
            Error::Buffer
        } else if token & (qtd::TRANSACTION_ERROR | qtd::MISSED_MICROFRAME) != 0 {
            Error::NoAnswer
        } else {
            Error::Stall
        })
    }

    /// The bytes a qTD for `asked` moved.
    fn moved(token: u32, asked: usize) -> usize {
        asked.saturating_sub(((token >> qtd::BYTES_SHIFT) & qtd::BYTES_MASK) as usize)
    }

    /// A free chunk of the interrupt buffers.
    fn take_chunk(&mut self) -> Option<usize> {
        let chunk = (0..CHUNKS).find(|&c| self.chunks_used & (1 << c) == 0)?;
        self.chunks_used |= 1 << chunk;
        Some(chunk)
    }

    /// Hangs pipe `index`'s queue head in the periodic tree.
    fn hang(&mut self, index: usize) {
        let Some(pipe) = &self.pipes[index] else { return };
        let (qh, at) = (pipe.qh, self.tree[pipe.node]);
        self.pool.write32(qh, qh::LINK, self.pool.read32(at, qh::LINK));
        device_barrier();
        self.pool.write32(at, qh::LINK, self.pool.phys(qh) | link::QH);
    }

    /// Takes pipe `index`'s queue head out of the periodic tree, and waits
    /// until the controller has left it.
    fn unhang(&mut self, index: usize, waiter: &mut dyn Waiter) {
        let Some(pipe) = &self.pipes[index] else { return };
        let target = self.pool.phys(pipe.qh);
        let mut at = self.tree[pipe.node];
        for _ in 0..MAX_PIPES + 1 {
            let next = self.pool.read32(at, qh::LINK);
            if next & link::TERMINATE != 0 {
                return;
            }
            if next & link::ADDRESS == target {
                self.pool.write32(at, qh::LINK, self.pool.read32(pipe.qh, qh::LINK));
                break;
            }
            let Some(slot) = self.pool.slot_at(next & link::ADDRESS) else { return };
            // Past this phase's pipes: the next head of the tree.
            if self.tree.contains(&slot) {
                return;
            }
            at = slot;
        }
        device_barrier();
        self.wait_frames(waiter);
    }

    /// The pipe `pipe` names, if it is open.
    fn pipe(&self, pipe: PipeId) -> Result<&Pipe, Error> {
        self.pipes.get(usize::from(pipe)).and_then(Option::as_ref).ok_or(Error::BadRequest)
    }
}

/// Takes the controller from the firmware through its legacy support
/// capability, at `eecp` in configuration space (5.1): the system's
/// semaphore is set and the firmware's waited for; a firmware that does
/// not let go in time is overruled. Its SMIs are turned off either way.
fn take_from_firmware(function: &Function, eecp: usize, waiter: &mut dyn Waiter) -> Handoff {
    let mut at = eecp;
    // A list that loops ends after the configuration space's 64 words.
    for _ in 0..48 {
        if !(0x40..=0xf8).contains(&at) || !at.is_multiple_of(4) {
            return Handoff::None;
        }
        let word = function.read32(at);
        if word as u8 == legacy::ID {
            let handoff = if function.read8(at + legacy::BIOS_OWNED) & 1 == 0 {
                function.write8(at + legacy::OS_OWNED, 1);
                Handoff::Free
            } else {
                function.write8(at + legacy::OS_OWNED, 1);
                if wait_for(waiter, HANDOFF_US, || function.read8(at + legacy::BIOS_OWNED) & 1 == 0)
                {
                    Handoff::Given
                } else {
                    function.write8(at + legacy::BIOS_OWNED, 0);
                    Handoff::Taken
                }
            };
            function.write32(at + legacy::CTLSTS, legacy::SMI_STATUS);
            return handoff;
        }
        at = ((word >> 8) & 0xff) as usize;
    }
    Handoff::None
}

impl Hcd for Ehci {
    fn kind(&self) -> &'static str {
        "EHCI"
    }

    fn ports(&self) -> u8 {
        self.info.ports
    }

    fn port(&mut self, port: u8) -> Port {
        let status = self.read_port(port);
        if status == u32::MAX {
            self.failed = true;
            return Port::default();
        }
        Port {
            connected: status & portsc::CONNECTED != 0,
            enabled: status & portsc::ENABLED != 0,
            connect_changed: status & portsc::CONNECT_CHANGE != 0,
            other_changed: status & (portsc::ENABLE_CHANGE | portsc::OVER_CURRENT_CHANGE) != 0,
            over_current: status & portsc::OVER_CURRENT != 0,
            companion: status & portsc::OWNER != 0,
        }
    }

    fn acknowledge(&mut self, port: u8) {
        let status = self.read_port(port);
        // The changes written back as they were read clear them.
        self.op.write32(Self::port_offset(port), status);
    }

    fn reset(&mut self, port: u8, waiter: &mut dyn Waiter) -> Result<Option<Speed>, Error> {
        let status = self.read_port(port);
        if status & portsc::CONNECTED == 0 {
            return Err(Error::Gone);
        }
        // A low-speed device pulls its lines to the K state (4.2.2).
        if (status >> portsc::LINE_SHIFT) & 0b11 == portsc::LINE_K {
            return self.to_companion(port);
        }
        // Reset, with the port disabled, for 50 ms; then the reset ends.
        self.write_port(port, (status | portsc::RESET) & !portsc::ENABLED);
        waiter.wait(PORT_RESET_US);
        let status = self.read_port(port);
        self.write_port(port, status & !portsc::RESET);
        let op = &self.op;
        let offset = Self::port_offset(port);
        if !wait_for(waiter, PORT_RESET_END_US, || op.read32(offset) & portsc::RESET == 0) {
            return Err(Error::TimedOut);
        }
        let status = self.read_port(port);
        if status & portsc::CONNECTED == 0 {
            return Err(Error::Gone);
        }
        // A high-speed device enables its port; any other is full speed.
        if status & portsc::ENABLED != 0 {
            return Ok(Some(Speed::High));
        }
        self.to_companion(port)
    }

    fn disable(&mut self, port: u8) {
        let status = self.read_port(port);
        self.write_port(port, status & !portsc::ENABLED);
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
        if length > CONTROL_DATA || self.failed {
            return Err(if self.failed { Error::Halted } else { Error::BadRequest });
        }
        let qh = self.default_pipe(target, max_packet, waiter)?;
        let into_host = setup.device_to_host();
        let bytes = setup.to_u64().to_le_bytes();
        self.control.write(0, &bytes);
        if !into_host && length > 0 {
            self.control.write(PAGE_SIZE, &data[..length]);
        }
        let [first, middle, last] = self.control_qtds;
        let phys = |slot| self.pool.phys(slot);
        let base = qtd::ACTIVE | qtd::CERR_3;
        let after_setup = if length > 0 { phys(middle) } else { phys(last) };
        self.fill_qtd(first, after_setup, base | qtd::PID_SETUP, self.control.phys(), 8);
        let data_pid = if into_host { qtd::PID_IN } else { qtd::PID_OUT };
        if length > 0 {
            let token = base | data_pid | qtd::TOGGLE;
            self.fill_qtd(middle, phys(last), token, self.control.phys() + PAGE_SIZE as u64, length);
        }
        // The status stage goes the other way, or in without data.
        let status_pid = if into_host && length > 0 { qtd::PID_OUT } else { qtd::PID_IN };
        self.fill_qtd(last, link::TERMINATE, base | status_pid | qtd::TOGGLE | qtd::IOC, 0, 0);
        device_barrier();
        // The overlay is idle: a halt from before is cleared, and the
        // controller fetches the first qTD at its next visit.
        self.pool.write32(qh, qh::TOKEN, 0);
        self.pool.write32(qh, qh::ALT_NEXT, link::TERMINATE);
        self.pool.write32(qh, qh::NEXT, phys(first));
        let used: &[Slot] = if length > 0 { &[first, middle, last] } else { &[first, last] };
        let pool = &self.pool;
        let done = || {
            used.iter().any(|&s| pool.read32(s, qtd::TOKEN) & qtd::HALTED != 0)
                || pool.read32(last, qtd::TOKEN) & qtd::ACTIVE == 0
        };
        let finished = wait_for(waiter, u64::from(timeout_ms) * 1000, done);
        device_barrier();
        if !finished {
            // Taken out of the schedule while its qTDs are put right.
            self.forget(target.address, waiter);
            return Err(Error::TimedOut);
        }
        for &slot in used {
            Self::outcome(self.pool.read32(slot, qtd::TOKEN))?;
        }
        let came = if length > 0 { Self::moved(self.pool.read32(middle, qtd::TOKEN), length) } else { 0 };
        if into_host {
            self.control.read(PAGE_SIZE, &mut data[..came]);
        }
        Ok(came)
    }

    fn open(&mut self, endpoint: Endpoint, buffer: usize) -> Result<PipeId, Error> {
        if endpoint.kind != TransferType::Interrupt || buffer > CHUNK {
            return Err(Error::BadRequest);
        }
        let index = self.pipes.iter().position(Option::is_none).ok_or(Error::NoMemory)?;
        let chunk = self.take_chunk().ok_or(Error::NoMemory)?;
        let (Some(qh), Some(qtd)) = (self.pool.take(), self.pool.take()) else {
            self.chunks_used &= !(1 << chunk);
            return Err(Error::NoMemory);
        };
        let period = endpoint.period_microframes();
        let high = endpoint.target.speed == Speed::High;
        // Below a millisecond, every frame, in the microframes asked for;
        // a split transaction starts in microframe 0 and completes in 2 to
        // 4 (4.12.2).
        let (level, s_mask, c_mask) = if period < 8 {
            let mask = match period {
                1 => 0xff,
                2 => 0x55,
                _ => 0x11,
            };
            (0, mask, 0)
        } else {
            let level = (period / 8).trailing_zeros() as usize;
            if high { (level, 0x01, 0) } else { (level, 0x01, 0x1c) }
        };
        let level = level.min(LEVELS - 1);
        let phase =
            (0..1 << level).min_by_key(|&phase| self.load[node(level, phase)]).unwrap_or(0);
        let node = node(level, phase);
        self.load[node] = self.load[node].saturating_add(1);
        let (chars, mut caps) = Self::words(endpoint.target, endpoint.number, endpoint.max_packet, false);
        if high {
            caps = caps & !(3 << qh::MULT_SHIFT)
                | u32::from(endpoint.transactions.clamp(1, 3)) << qh::MULT_SHIFT;
        }
        caps |= s_mask | c_mask << qh::C_MASK_SHIFT;
        self.pool.write32(qh, qh::CHARS, chars);
        self.pool.write32(qh, qh::CAPS, caps);
        self.pool.write32(qh, qh::NEXT, link::TERMINATE);
        self.pool.write32(qh, qh::ALT_NEXT, link::TERMINATE);
        self.pipes[index] = Some(Pipe { endpoint, qh, qtd, chunk, node, asked: None });
        self.hang(index);
        Ok(index as PipeId)
    }

    fn start(&mut self, pipe: PipeId, data: Data<'_>) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Halted);
        }
        let (chunk, qh, slot, into_host) = {
            let p = self.pipe(pipe)?;
            if p.asked.is_some() {
                return Err(Error::BadRequest);
            }
            (p.chunk, p.qh, p.qtd, p.endpoint.into_host)
        };
        let length = match data {
            Data::In(len) if into_host && len <= CHUNK => len,
            Data::Out(bytes) if !into_host && bytes.len() <= CHUNK => {
                self.buffers.write(chunk * CHUNK, bytes);
                bytes.len()
            }
            _ => return Err(Error::BadRequest),
        };
        let pid = if into_host { qtd::PID_IN } else { qtd::PID_OUT };
        let phys = self.buffers.phys() + (chunk * CHUNK) as u64;
        let token = qtd::ACTIVE | qtd::CERR_3 | pid | qtd::IOC;
        self.fill_qtd(slot, link::TERMINATE, token, phys, length);
        device_barrier();
        self.pool.write32(qh, qh::NEXT, self.pool.phys(slot));
        if let Some(Some(p)) = self.pipes.get_mut(usize::from(pipe)) {
            p.asked = Some(length);
        }
        Ok(())
    }

    fn poll(&mut self, pipe: PipeId) -> Option<Result<usize, Error>> {
        let p = self.pipes.get(usize::from(pipe))?.as_ref()?;
        let asked = p.asked?;
        let token = self.pool.read32(p.qtd, qtd::TOKEN);
        if token & qtd::ACTIVE != 0 && !self.failed {
            return None;
        }
        device_barrier();
        if let Some(Some(p)) = self.pipes.get_mut(usize::from(pipe)) {
            p.asked = None;
        }
        if self.failed {
            return Some(Err(Error::Halted));
        }
        Some(Self::outcome(token).map(|()| Self::moved(token, asked)))
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
        if p.asked.is_none() {
            return;
        }
        let qh = p.qh;
        self.unhang(index, waiter);
        // Out of the schedule: the overlay and the qTD can be put right,
        // keeping the data toggle.
        let toggle = self.pool.read32(qh, qh::TOKEN) & qtd::TOGGLE;
        self.pool.write32(qh, qh::NEXT, link::TERMINATE);
        self.pool.write32(qh, qh::TOKEN, toggle);
        if let Some(Some(p)) = self.pipes.get_mut(index) {
            self.pool.write32(p.qtd, qtd::TOKEN, 0);
            p.asked = None;
        }
        self.hang(index);
    }

    fn reset_pipe(&mut self, pipe: PipeId) {
        let Ok(p) = self.pipe(pipe) else { return };
        if p.asked.is_some() {
            return;
        }
        // An idle overlay, halted or not, starts over at DATA0.
        self.pool.write32(p.qh, qh::TOKEN, 0);
    }

    fn close(&mut self, pipe: PipeId, waiter: &mut dyn Waiter) {
        let index = usize::from(pipe);
        if self.pipe(pipe).is_err() {
            return;
        }
        self.unhang(index, waiter);
        if let Some(p) = self.pipes[index].take() {
            self.load[p.node] = self.load[p.node].saturating_sub(1);
            self.chunks_used &= !(1 << p.chunk);
            self.pool.give(p.qh);
            self.pool.give(p.qtd);
        }
    }

    fn forget(&mut self, address: u8, waiter: &mut dyn Waiter) {
        let Some(at) =
            self.defaults.iter().position(|d| d.is_some_and(|d| d.target.address == address))
        else {
            return;
        };
        let Some(pipe) = self.defaults[at].take() else { return };
        self.unlink_async(pipe.qh, waiter);
        self.pool.give(pipe.qh);
    }

    fn failed(&mut self) -> bool {
        if !self.failed {
            let status = self.op.read32(op::USBSTS);
            if status == u32::MAX || status & (op::HSE | op::HALTED) != 0 {
                self.failed = true;
            }
        }
        self.failed
    }

    fn halt(&mut self, waiter: &mut dyn Waiter) {
        stop(&self.op, &mut |us| waiter.wait(us));
        self.failed = true;
    }
}

/// Halts the controller whose operational registers are `op` and gives
/// its ports back to the companions, as before the driver took it: before
/// the machine is powered off or restarted. `delay` waits that many
/// microseconds. Returns whether it halted.
pub fn stop(op: &Mmio, delay: &mut dyn FnMut(u32)) -> bool {
    op.write32(op::USBCMD, op.read32(op::USBCMD) & !(op::RUN | op::PSE | op::ASE));
    let mut halted = false;
    for _ in 0..20 {
        if op.read32(op::USBSTS) & op::HALTED != 0 {
            halted = true;
            break;
        }
        delay(1000);
    }
    op.write32(op::CONFIGFLAG, 0);
    halted
}

/// The operational registers of the controller whose registers are
/// `regs`, for [`stop`].
pub fn operational(regs: &Mmio) -> Option<Mmio> {
    let caplength = (regs.read32(cap::LENGTH_VERSION) & 0xff) as usize;
    regs.window(caplength, op::CONFIGFLAG + 4)
}
