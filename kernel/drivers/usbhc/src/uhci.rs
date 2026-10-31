// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! UHCI, Intel's USB 1.1 host controller for low and full speed (the
//! *Universal Host Controller Interface Design Guide*, revision 1.1; ADR
//! 0243): the companions of Intel's and VIA's EHCI controllers, and the
//! only USB controller of their older chipsets.
//!
//! A UHCI controller is one PCI function with its registers in I/O port
//! space, BAR 4, which the driver reaches through [`Ports`]. Each
//! millisecond it reads an entry of a 1024-entry frame list, which leads
//! to queue heads (QHs) and the transfer descriptors (TDs) hung from
//! them, one packet each:
//!
//! - as on OHCI, the entries lead into a tree of empty QHs for periods of
//!   1 to 32 ms, and an interrupt pipe's QH hangs after the one of its
//!   period and phase;
//! - the 1 ms head leads to the head of the control QHs, one for each
//!   device's default pipe, which every frame then reaches.
//!
//! A QH's element pointer names the TD it works on; the controller moves
//! it past each TD it finishes, and leaves it on one that fails, or that
//! brought a short packet in. The driver keeps each pipe's data toggle
//! itself, and writes it into each TD.
//!
//! The driver is polled, as the others are. The firmware may drive the
//! controller through SMIs, which the legacy support register in
//! configuration space turns off.

use core::ops::DerefMut;

use radix_hw::pci::Function;
use radix_hw::{Dma, PAGE_SIZE, device_barrier};
use radix_usb::host::{Data, Endpoint, Error, Hcd, PipeId, Port, Target, Waiter};
use radix_usb::{Setup, Speed, TransferType};

use crate::{Handoff, Pool, Slot, wait_for};

/// The PCI class, subclass and programming interface of a UHCI
/// controller: serial bus, USB, UHCI.
pub const PCI_CLASS: (u8, u8, u8) = (0x0c, 0x03, 0x00);
/// The BAR that holds its registers, in I/O space.
pub const BAR: usize = 4;

/// A controller's registers in I/O port space, as UHCI's are: offsets
/// into its window, which the kernel keeps accesses within.
pub trait Ports: Send {
    /// The window's length.
    fn len(&self) -> u16;
    /// Reads 16 bits at `offset`.
    fn read16(&self, offset: u16) -> u16;
    /// Writes 16 bits at `offset`.
    fn write16(&self, offset: u16, value: u16);
    /// Reads 32 bits at `offset`.
    fn read32(&self, offset: u16) -> u32;
    /// Writes 32 bits at `offset`.
    fn write32(&self, offset: u16, value: u32);
}

/// The registers (2.1).
mod reg {
    pub const USBCMD: u16 = 0x00;
    pub const USBSTS: u16 = 0x02;
    pub const USBINTR: u16 = 0x04;
    pub const FRNUM: u16 = 0x06;
    pub const FRBASEADD: u16 = 0x08;
    pub const PORTSC: u16 = 0x10;

    /// USBCMD: run, reset, configured, 64-byte packets for bandwidth
    /// reclamation.
    pub const RUN: u16 = 1 << 0;
    pub const HCRESET: u16 = 1 << 1;
    pub const CF: u16 = 1 << 6;
    pub const MAXP: u16 = 1 << 7;
    /// USBSTS: a host system error, a process error, halted; every bit.
    pub const HSE: u16 = 1 << 3;
    pub const HCPE: u16 = 1 << 4;
    pub const HALTED: u16 = 1 << 5;
    pub const ALL: u16 = 0x3f;
}

/// A port's status and control (2.1.7).
mod port {
    pub const CONNECTED: u16 = 1 << 0;
    pub const CONNECT_CHANGE: u16 = 1 << 1;
    pub const ENABLED: u16 = 1 << 2;
    pub const ENABLE_CHANGE: u16 = 1 << 3;
    /// Always reads 1: a port is there.
    pub const PRESENT: u16 = 1 << 7;
    pub const LOW_SPEED: u16 = 1 << 8;
    pub const RESET: u16 = 1 << 9;
    /// The bits a write of 1 clears.
    pub const CHANGES: u16 = CONNECT_CHANGE | ENABLE_CHANGE;
}

/// The legacy support register in configuration space (5.2.1): written,
/// its status bits clear and its SMIs and PIRQ go off.
const LEGSUP: usize = 0xc0;
const LEGSUP_CLEAR: u16 = 0x8f00;
const LEGSUP_SMIS: u16 = 0x3f;

/// A link pointer's bits (3.1, 3.2.1, 3.3.1).
mod link {
    pub const TERMINATE: u32 = 1 << 0;
    pub const QH: u32 = 1 << 1;
    /// A TD's: depth first, to the next TD of the same QH.
    pub const DEPTH: u32 = 1 << 2;
}

/// A TD's words (3.2), by offset, and their bits.
mod td {
    pub const LINK: usize = 0x00;
    pub const STATUS: usize = 0x04;
    pub const TOKEN: usize = 0x08;
    pub const BUFFER: usize = 0x0c;
    pub const SIZE: usize = 32;

    /// STATUS: the actual length (n - 1), and the status bits.
    pub const ACTUAL: u32 = 0x7ff;
    pub const BITSTUFF: u32 = 1 << 17;
    pub const CRC_TIMEOUT: u32 = 1 << 18;
    pub const BABBLE: u32 = 1 << 20;
    pub const BUFFER_ERROR: u32 = 1 << 21;
    pub const STALLED: u32 = 1 << 22;
    pub const ACTIVE: u32 = 1 << 23;
    pub const LOW_SPEED: u32 = 1 << 26;
    /// Three tries.
    pub const ERRORS_3: u32 = 3 << 27;
    pub const SHORT_PACKET: u32 = 1 << 29;
    /// TOKEN: the PIDs, the toggle, the most (n - 1) at bit 21.
    pub const PID_SETUP: u32 = 0x2d;
    pub const PID_IN: u32 = 0x69;
    pub const PID_OUT: u32 = 0xe1;
    pub const ADDRESS_SHIFT: u32 = 8;
    pub const ENDPOINT_SHIFT: u32 = 15;
    pub const TOGGLE: u32 = 1 << 19;
    pub const MAX_SHIFT: u32 = 21;
}

/// A QH's words: the next QH, and the TD it works on.
mod qh {
    pub const HEAD: usize = 0x00;
    pub const ELEMENT: usize = 0x04;
}

/// How long a reset takes, and a root port's (50 ms, 7.1.7.5 of USB 2.0),
/// and how long the port is waited for after.
const RESET_US: u64 = 10_000;
const PORT_RESET_US: u32 = 50_000;
const PORT_ENABLE_US: u64 = 10_000;
const HALT_US: u64 = 10_000;
/// How long a frame is waited for.
const FRAME_US: u64 = 10_000;
const LEVELS: usize = 6;
const TREE: usize = (1 << LEVELS) - 1;
const MAX_PIPES: usize = 32;
const MAX_DEFAULT_PIPES: usize = 32;
/// An interrupt pipe's slot holds its QH and this many TDs after it.
const PIPE_TDS: usize = 3;
/// Interrupt pipes' buffers: 1 KiB each.
const CHUNK: usize = 1024;
const BUFFER_PAGES: usize = 8;
const CHUNKS: usize = BUFFER_PAGES * PAGE_SIZE / CHUNK;
/// The control data stage's largest, and the TDs for it at 8 bytes a
/// packet, with the SETUP and status TDs.
pub const CONTROL_DATA: usize = PAGE_SIZE;
const CONTROL_TDS: usize = 2 + CONTROL_DATA / 8;
const CONTROL_TD_PAGES: usize = (CONTROL_TDS * td::SIZE).div_ceil(PAGE_SIZE);

/// What the driver knows of its controller, for the log.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    /// Its root ports.
    pub ports: u8,
    /// How the firmware let go of it.
    pub handoff: Handoff,
}

/// Where a control transfer is.
enum Control {
    Running,
    /// A short packet came, after these bytes: the status stage is next.
    Short(usize),
    Done(Result<usize, Error>),
}

struct Pipe {
    endpoint: Endpoint,
    /// Its QH, with its TDs after it in the slot.
    slot: Slot,
    toggle: bool,
    /// The TDs and bytes of the transfer in flight.
    asked: Option<(u8, usize)>,
    chunk: u8,
    node: u8,
}

#[derive(Clone, Copy)]
struct DefaultPipe {
    target: Target,
    max_packet: u16,
    qh: Slot,
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

/// A UHCI controller, taken from the firmware and running, reached
/// through `P`, with its pipes' records in `T`.
pub struct Uhci<P: Ports, T: DerefMut<Target = Tables>> {
    io: P,
    info: Info,
    frames: Dma,
    pool: Pool,
    /// The head of the control QHs.
    head: Slot,
    tree: [Slot; TREE],
    load: [u8; TREE],
    /// The control transfer's setup bytes (at 0) and data (a page on),
    /// and its TDs.
    control: Dma,
    control_tds: Dma,
    buffers: Dma,
    chunks_used: u64,
    tables: T,
    failed: bool,
}

fn node(level: usize, phase: usize) -> usize {
    (1 << level) - 1 + phase
}

/// What a finished TD's status says.
fn outcome(status: u32) -> Result<(), Error> {
    if status & td::BABBLE != 0 {
        Err(Error::Babble)
    } else if status & td::BUFFER_ERROR != 0 {
        Err(Error::Buffer)
    } else if status & (td::CRC_TIMEOUT | td::BITSTUFF) != 0 {
        Err(Error::NoAnswer)
    } else if status & td::STALLED != 0 {
        Err(Error::Stall)
    } else {
        Ok(())
    }
}

/// The bytes a finished TD moved.
fn actual(status: u32) -> usize {
    ((status & td::ACTUAL) + 1) as usize & td::ACTUAL as usize
}

/// A TD's token: `pid` to `target`'s `endpoint`, `toggle`, `len` bytes.
fn token(pid: u32, target: Target, endpoint: u8, toggle: bool, len: usize) -> u32 {
    let max = (len as u32).wrapping_sub(1) & 0x7ff;
    pid | u32::from(target.address & 0x7f) << td::ADDRESS_SHIFT
        | u32::from(endpoint & 0xf) << td::ENDPOINT_SHIFT
        | if toggle { td::TOGGLE } else { 0 }
        | max << td::MAX_SHIFT
}

/// The status a TD for `target` starts with.
fn fresh(target: Target, into_host: bool) -> u32 {
    let mut status = td::ACTIVE | td::ERRORS_3;
    if target.speed == Speed::Low {
        status |= td::LOW_SPEED;
    }
    if into_host {
        status |= td::SHORT_PACKET;
    }
    status
}

impl<P: Ports, T: DerefMut<Target = Tables>> Uhci<P, T> {
    /// Takes the controller of `function`, whose registers `io` reaches,
    /// from the firmware, resets it and runs it. Memory comes from `alloc`
    /// (pages, below 4 GiB); `tables`, empty, keeps the pipes.
    pub fn new(
        function: &Function,
        io: P,
        waiter: &mut dyn Waiter,
        alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>,
        tables: T,
    ) -> Result<Uhci<P, T>, crate::Error> {
        if io.len() < reg::PORTSC + 4 {
            return Err(crate::Error::Unsupported("its registers are too few"));
        }
        if io.read16(reg::USBSTS) == u16::MAX {
            return Err(crate::Error::Unsupported("it reads as absent"));
        }
        // SMIs off; then stopped and reset.
        let legacy = function.read16(LEGSUP);
        function.write16(LEGSUP, LEGSUP_CLEAR);
        let handoff = if legacy & LEGSUP_SMIS != 0 { Handoff::Given } else { Handoff::Free };
        io.write16(reg::USBINTR, 0);
        io.write16(reg::USBCMD, 0);
        if !wait_for(waiter, HALT_US, || io.read16(reg::USBSTS) & reg::HALTED != 0) {
            return Err(crate::Error::Unresponsive("it did not halt"));
        }
        io.write16(reg::USBCMD, reg::HCRESET);
        if !wait_for(waiter, RESET_US, || io.read16(reg::USBCMD) & reg::HCRESET == 0) {
            return Err(crate::Error::Unresponsive("it did not reset"));
        }
        // Its ports: as many as read as there, up to 7, at least 2.
        let mut ports = 0u16;
        while ports < 7 && reg::PORTSC + 2 * (ports + 1) <= io.len() {
            let status = io.read16(reg::PORTSC + 2 * ports);
            if status & port::PRESENT == 0 || status == u16::MAX {
                break;
            }
            ports += 1;
        }
        let ports = ports.max(2) as u8;
        let frames = alloc(1, true).ok_or(crate::Error::NoMemory)?;
        let control = alloc(2, true).ok_or(crate::Error::NoMemory)?;
        let control_tds = alloc(CONTROL_TD_PAGES, true).ok_or(crate::Error::NoMemory)?;
        let buffers = alloc(BUFFER_PAGES, true).ok_or(crate::Error::NoMemory)?;
        let mut pool = Pool::new(alloc).ok_or(crate::Error::NoMemory)?;
        let mut take = || pool.take().ok_or(crate::Error::NoMemory);
        let head = take()?;
        let mut tree = [head; TREE];
        for slot in &mut tree {
            *slot = take()?;
        }
        let mut uhci = Uhci {
            io,
            info: Info { ports, handoff },
            frames,
            pool,
            head,
            tree,
            load: [0; TREE],
            control,
            control_tds,
            buffers,
            chunks_used: 0,
            tables,
            failed: false,
        };
        uhci.build_schedule();
        uhci.start(waiter)?;
        Ok(uhci)
    }

    /// What the driver knows of the controller.
    pub fn info(&self) -> Info {
        self.info
    }

    fn build_schedule(&mut self) {
        let pool = &self.pool;
        pool.write32(self.head, qh::HEAD, link::TERMINATE);
        pool.write32(self.head, qh::ELEMENT, link::TERMINATE);
        for level in 0..LEVELS {
            for phase in 0..1 << level {
                let slot = self.tree[node(level, phase)];
                pool.write32(slot, qh::ELEMENT, link::TERMINATE);
                let next = if level == 0 {
                    self.head
                } else {
                    self.tree[node(level - 1, phase % (1 << (level - 1)))]
                };
                pool.write32(slot, qh::HEAD, pool.phys(next) | link::QH);
            }
        }
        for entry in 0..1024 {
            let top = self.tree[node(LEVELS - 1, entry % 32)];
            self.frames.write32(4 * entry, self.pool.phys(top) | link::QH);
        }
    }

    fn start(&mut self, waiter: &mut dyn Waiter) -> Result<(), crate::Error> {
        let io = &self.io;
        io.write16(reg::USBSTS, reg::ALL);
        io.write16(reg::FRNUM, 0);
        io.write32(reg::FRBASEADD, self.frames.phys() as u32);
        device_barrier();
        io.write16(reg::USBCMD, reg::RUN | reg::CF | reg::MAXP);
        if !wait_for(waiter, HALT_US, || io.read16(reg::USBSTS) & reg::HALTED == 0) {
            return Err(crate::Error::Unresponsive("it did not start"));
        }
        Ok(())
    }

    fn port_offset(port: u8) -> u16 {
        reg::PORTSC + 2 * u16::from(port - 1)
    }

    /// Writes `set` into port `port`'s register, clearing `clear`, without
    /// clearing a change by chance.
    fn port_write(&self, port: u8, set: u16, clear: u16) {
        let offset = Self::port_offset(port);
        let status = self.io.read16(offset) & !port::CHANGES;
        self.io.write16(offset, (status | set) & !clear);
    }

    /// Waits until the controller has started another frame, after which
    /// it no longer holds a QH taken out of the schedule.
    fn wait_frame(&mut self, waiter: &mut dyn Waiter) {
        let io = &self.io;
        let start = io.read16(reg::FRNUM) & 0x7ff;
        if !wait_for(waiter, FRAME_US, || io.read16(reg::FRNUM) & 0x7ff != start) {
            self.failed = true;
        }
    }

    fn default_qh(
        &mut self,
        target: Target,
        max_packet: u16,
        waiter: &mut dyn Waiter,
    ) -> Result<Slot, Error> {
        if let Some(found) =
            self.tables.defaults.iter().flatten().find(|d| d.target.address == target.address)
        {
            if found.target == target && found.max_packet == max_packet {
                return Ok(found.qh);
            }
            self.forget(target.address, waiter);
        }
        let free = self.tables.defaults.iter().position(Option::is_none).ok_or(Error::NoMemory)?;
        let slot = self.pool.take().ok_or(Error::NoMemory)?;
        let pool = &self.pool;
        pool.write32(slot, qh::ELEMENT, link::TERMINATE);
        pool.write32(slot, qh::HEAD, pool.read32(self.head, qh::HEAD));
        device_barrier();
        pool.write32(self.head, qh::HEAD, pool.phys(slot) | link::QH);
        self.tables.defaults[free] = Some(DefaultPipe { target, max_packet, qh: slot });
        Ok(slot)
    }

    fn control_td_phys(&self, index: usize) -> u32 {
        (self.control_tds.phys() + (index * td::SIZE) as u64) as u32
    }

    /// Writes control TD `index`, linked to the one after it unless
    /// `last`.
    fn write_control_td(&self, index: usize, status: u32, token: u32, buffer: u32, last: bool) {
        let at = index * td::SIZE;
        let link = if last { link::TERMINATE } else { self.control_td_phys(index + 1) | link::DEPTH };
        self.control_tds.write32(at + td::LINK, link);
        self.control_tds.write32(at + td::STATUS, status);
        self.control_tds.write32(at + td::TOKEN, token);
        self.control_tds.write32(at + td::BUFFER, buffer);
    }

    fn control_status(&self, index: usize) -> u32 {
        self.control_tds.read32(index * td::SIZE + td::STATUS)
    }

    /// Where the transfer of `count` control TDs is.
    fn control_done(&self, count: usize) -> Control {
        let mut moved = 0;
        for index in 0..count {
            let status = self.control_status(index);
            if status & td::ACTIVE != 0 {
                return Control::Running;
            }
            if let Err(err) = outcome(status) {
                return Control::Done(Err(err));
            }
            if index != 0 && index != count - 1 {
                let came = actual(status);
                moved += came;
                let token = self.control_tds.read32(index * td::SIZE + td::TOKEN);
                if came < actual(token >> td::MAX_SHIFT) {
                    return Control::Short(moved);
                }
            }
        }
        Control::Done(Ok(moved))
    }

    fn take_chunk(&mut self) -> Option<u8> {
        let chunk = (0..CHUNKS).find(|&c| self.chunks_used & (1 << c) == 0)?;
        self.chunks_used |= 1 << chunk;
        Some(chunk as u8)
    }

    fn chunk_phys(&self, chunk: u8) -> u32 {
        (self.buffers.phys() + (usize::from(chunk) * CHUNK) as u64) as u32
    }

    fn pipe(&self, pipe: PipeId) -> Result<&Pipe, Error> {
        self.tables.pipes.get(usize::from(pipe)).and_then(Option::as_ref).ok_or(Error::BadRequest)
    }

    /// The TDs of pipe `index`'s transfer: how many finished well, what
    /// they moved, and whether it is over (and how).
    fn pipe_progress(&self, slot: Slot, tds: u8) -> (u8, usize, Option<Result<(), Error>>) {
        let (mut good, mut moved) = (0, 0);
        for i in 0..usize::from(tds) {
            let offset = 32 * (i + 1);
            let status = self.pool.read32(slot, offset + td::STATUS);
            if status & td::ACTIVE != 0 {
                return (good, moved, None);
            }
            if let Err(err) = outcome(status) {
                return (good, moved, Some(Err(err)));
            }
            good += 1;
            let came = actual(status);
            moved += came;
            let asked = actual(self.pool.read32(slot, offset + td::TOKEN) >> td::MAX_SHIFT);
            if came < asked {
                return (good, moved, Some(Ok(())));
            }
        }
        (good, moved, Some(Ok(())))
    }

    /// Takes pipe `index`'s QH out of the tree, and waits a frame.
    fn unhang(&mut self, index: usize, waiter: &mut dyn Waiter) {
        let Some(pipe) = &self.tables.pipes[index] else { return };
        let target = self.pool.phys(pipe.slot);
        let mut at = self.tree[usize::from(pipe.node)];
        for _ in 0..MAX_PIPES + 1 {
            let next = self.pool.read32(at, qh::HEAD) & !0xf;
            if next == target {
                self.pool.write32(at, qh::HEAD, self.pool.read32(pipe.slot, qh::HEAD));
                break;
            }
            match self.pool.slot_at(next) {
                Some(slot) if !self.tree.contains(&slot) && slot != self.head => at = slot,
                _ => return,
            }
        }
        device_barrier();
        self.wait_frame(waiter);
    }
}

impl<P: Ports, T: DerefMut<Target = Tables>> Hcd for Uhci<P, T> {
    fn kind(&self) -> &'static str {
        "UHCI"
    }

    fn ports(&self) -> u8 {
        self.info.ports
    }

    fn port(&mut self, port: u8) -> Port {
        let status = self.io.read16(Self::port_offset(port));
        if status == u16::MAX {
            self.failed = true;
            return Port::default();
        }
        Port {
            connected: status & port::CONNECTED != 0,
            enabled: status & port::ENABLED != 0,
            connect_changed: status & port::CONNECT_CHANGE != 0,
            other_changed: status & port::ENABLE_CHANGE != 0,
            over_current: false,
            companion: false,
        }
    }

    fn acknowledge(&mut self, port: u8) {
        let offset = Self::port_offset(port);
        let status = self.io.read16(offset);
        self.io.write16(offset, status);
    }

    fn reset(&mut self, port: u8, waiter: &mut dyn Waiter) -> Result<Option<Speed>, Error> {
        let offset = Self::port_offset(port);
        if self.io.read16(offset) & port::CONNECTED == 0 {
            return Err(Error::Gone);
        }
        self.port_write(port, port::RESET, 0);
        waiter.wait(PORT_RESET_US);
        self.port_write(port, 0, port::RESET);
        waiter.wait(10);
        // The reset leaves the port off: it is turned on until it holds.
        let io = &self.io;
        let enabled = || {
            let status = io.read16(offset) & !port::CHANGES;
            if status & port::ENABLED == 0 {
                io.write16(offset, status | port::ENABLED);
            }
            io.read16(offset) & (port::ENABLED | port::CONNECTED)
                == port::ENABLED | port::CONNECTED
        };
        if !wait_for(waiter, PORT_ENABLE_US, enabled) {
            return Err(if io.read16(offset) & port::CONNECTED == 0 {
                Error::Gone
            } else {
                Error::NoAnswer
            });
        }
        let status = io.read16(offset);
        io.write16(offset, status & !port::CONNECT_CHANGE | port::ENABLE_CHANGE);
        Ok(Some(if status & port::LOW_SPEED != 0 { Speed::Low } else { Speed::Full }))
    }

    fn disable(&mut self, port: u8) {
        self.port_write(port, 0, port::ENABLED);
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
        let packet = usize::from(max_packet.clamp(8, 64));
        if length > CONTROL_DATA || length.div_ceil(packet) + 2 > CONTROL_TDS {
            return Err(Error::BadRequest);
        }
        let qh = self.default_qh(target, max_packet, waiter)?;
        let into_host = setup.device_to_host();
        self.control.write(0, &setup.to_u64().to_le_bytes());
        if !into_host && length > 0 {
            self.control.write(PAGE_SIZE, &data[..length]);
        }
        let setup_phys = self.control.phys() as u32;
        let data_phys = setup_phys + PAGE_SIZE as u32;
        let packets = length.div_ceil(packet);
        let count = packets + 2;
        self.write_control_td(0, fresh(target, false), token(td::PID_SETUP, target, 0, false, 8), setup_phys, false);
        let pid = if into_host { td::PID_IN } else { td::PID_OUT };
        for i in 0..packets {
            let len = packet.min(length - i * packet);
            let toggle = i % 2 == 0;
            let buffer = data_phys + (i * packet) as u32;
            self.write_control_td(1 + i, fresh(target, into_host), token(pid, target, 0, toggle, len), buffer, false);
        }
        let status_pid = if into_host && length > 0 { td::PID_OUT } else { td::PID_IN };
        self.write_control_td(count - 1, fresh(target, false), token(status_pid, target, 0, true, 0), 0, true);
        device_barrier();
        self.pool.write32(qh, qh::ELEMENT, self.control_td_phys(0));
        let deadline = waiter.now_us().saturating_add(u64::from(timeout_ms) * 1000);
        let mut result = None;
        let mut status_stage = false;
        loop {
            match self.control_done(count) {
                Control::Running => {}
                Control::Short(moved) => {
                    // The controller stopped on the short packet: on to
                    // the status stage.
                    if !status_stage {
                        status_stage = true;
                        self.pool.write32(qh, qh::ELEMENT, self.control_td_phys(count - 1));
                    }
                    let status = self.control_status(count - 1);
                    if status & td::ACTIVE == 0 {
                        result = Some(outcome(status).map(|()| moved));
                        break;
                    }
                }
                Control::Done(done) => {
                    result = Some(done);
                    break;
                }
            }
            if waiter.now_us() >= deadline {
                break;
            }
            waiter.wait(crate::POLL_US);
        }
        self.pool.write32(qh, qh::ELEMENT, link::TERMINATE);
        device_barrier();
        let Some(result) = result else {
            self.forget(target.address, waiter);
            return Err(Error::TimedOut);
        };
        let came = result?.min(length);
        if into_host {
            self.control.read(PAGE_SIZE, &mut data[..came]);
        }
        Ok(came)
    }

    fn open(&mut self, endpoint: Endpoint, buffer: usize) -> Result<PipeId, Error> {
        if endpoint.kind != TransferType::Interrupt || buffer > CHUNK || endpoint.max_packet == 0 {
            return Err(Error::BadRequest);
        }
        let index = self.tables.pipes.iter().position(Option::is_none).ok_or(Error::NoMemory)?;
        let chunk = self.take_chunk().ok_or(Error::NoMemory)?;
        let Some(slot) = self.pool.take() else {
            self.chunks_used &= !(1 << chunk);
            return Err(Error::NoMemory);
        };
        self.pool.write32(slot, qh::ELEMENT, link::TERMINATE);
        let level =
            ((endpoint.period_microframes() / 8).max(1).trailing_zeros() as usize).min(LEVELS - 1);
        let phase = (0..1 << level).min_by_key(|&p| self.load[node(level, p)]).unwrap_or(0);
        let node = node(level, phase);
        self.load[node] = self.load[node].saturating_add(1);
        let at = self.tree[node];
        self.pool.write32(slot, qh::HEAD, self.pool.read32(at, qh::HEAD));
        device_barrier();
        self.pool.write32(at, qh::HEAD, self.pool.phys(slot) | link::QH);
        self.tables.pipes[index] =
            Some(Pipe { endpoint, slot, toggle: false, asked: None, chunk, node: node as u8 });
        Ok(index as PipeId)
    }

    fn start(&mut self, pipe: PipeId, data: Data<'_>) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Halted);
        }
        let p = self.pipe(pipe)?;
        if p.asked.is_some() {
            return Err(Error::BadRequest);
        }
        let (endpoint, slot, chunk, toggle) = (p.endpoint, p.slot, p.chunk, p.toggle);
        let packet = usize::from(endpoint.max_packet);
        let length = match data {
            Data::In(len) if endpoint.into_host && len <= CHUNK => len,
            Data::Out(bytes) if !endpoint.into_host && bytes.len() <= CHUNK => {
                self.buffers.write(usize::from(chunk) * CHUNK, bytes);
                bytes.len()
            }
            _ => return Err(Error::BadRequest),
        };
        let tds = length.div_ceil(packet).max(1);
        if tds > PIPE_TDS {
            return Err(Error::BadRequest);
        }
        let pid = if endpoint.into_host { td::PID_IN } else { td::PID_OUT };
        let base = self.pool.phys(slot);
        for i in 0..tds {
            let offset = 32 * (i + 1);
            let len = packet.min(length.saturating_sub(i * packet));
            let link = if i + 1 == tds {
                link::TERMINATE
            } else {
                base + 32 * (i as u32 + 2) | link::DEPTH
            };
            let token = token(pid, endpoint.target, endpoint.number, toggle ^ (i % 2 == 1), len);
            self.pool.write32(slot, offset + td::LINK, link);
            self.pool.write32(slot, offset + td::STATUS, fresh(endpoint.target, endpoint.into_host));
            self.pool.write32(slot, offset + td::TOKEN, token);
            let buffer = self.chunk_phys(chunk) + (i * packet) as u32;
            self.pool.write32(slot, offset + td::BUFFER, buffer);
        }
        device_barrier();
        self.pool.write32(slot, qh::ELEMENT, base + 32);
        if let Some(Some(p)) = self.tables.pipes.get_mut(usize::from(pipe)) {
            p.asked = Some((tds as u8, length));
        }
        Ok(())
    }

    fn poll(&mut self, pipe: PipeId) -> Option<Result<usize, Error>> {
        let p = self.tables.pipes.get(usize::from(pipe))?.as_ref()?;
        let (tds, asked) = p.asked?;
        let slot = p.slot;
        if self.failed {
            if let Some(Some(p)) = self.tables.pipes.get_mut(usize::from(pipe)) {
                p.asked = None;
            }
            return Some(Err(Error::Halted));
        }
        let (good, moved, done) = self.pipe_progress(slot, tds);
        let result = done?;
        self.pool.write32(slot, qh::ELEMENT, link::TERMINATE);
        if let Some(Some(p)) = self.tables.pipes.get_mut(usize::from(pipe)) {
            p.asked = None;
            p.toggle ^= good % 2 == 1;
        }
        Some(result.map(|()| moved.min(asked)))
    }

    fn received(&self, pipe: PipeId, out: &mut [u8]) {
        if let Ok(p) = self.pipe(pipe) {
            let len = out.len().min(CHUNK);
            self.buffers.read(usize::from(p.chunk) * CHUNK, &mut out[..len]);
        }
    }

    fn cancel(&mut self, pipe: PipeId, waiter: &mut dyn Waiter) {
        let Ok(p) = self.pipe(pipe) else { return };
        let Some((tds, _)) = p.asked else { return };
        let slot = p.slot;
        self.pool.write32(slot, qh::ELEMENT, link::TERMINATE);
        self.wait_frame(waiter);
        let (good, _, _) = self.pipe_progress(slot, tds);
        if let Some(Some(p)) = self.tables.pipes.get_mut(usize::from(pipe)) {
            p.asked = None;
            p.toggle ^= good % 2 == 1;
        }
    }

    fn reset_pipe(&mut self, pipe: PipeId) {
        if let Some(Some(p)) = self.tables.pipes.get_mut(usize::from(pipe)) {
            if p.asked.is_none() {
                p.toggle = false;
            }
        }
    }

    fn close(&mut self, pipe: PipeId, waiter: &mut dyn Waiter) {
        let index = usize::from(pipe);
        if self.pipe(pipe).is_err() {
            return;
        }
        self.unhang(index, waiter);
        if let Some(p) = self.tables.pipes[index].take() {
            let node = usize::from(p.node);
            self.load[node] = self.load[node].saturating_sub(1);
            self.chunks_used &= !(1 << p.chunk);
            self.pool.give(p.slot);
        }
    }

    fn forget(&mut self, address: u8, waiter: &mut dyn Waiter) {
        let Some(at) =
            self.tables.defaults.iter().position(|d| d.is_some_and(|d| d.target.address == address))
        else {
            return;
        };
        let Some(pipe) = self.tables.defaults[at].take() else { return };
        let target = self.pool.phys(pipe.qh);
        let mut at = self.head;
        for _ in 0..MAX_DEFAULT_PIPES + 1 {
            let next = self.pool.read32(at, qh::HEAD);
            if next & link::TERMINATE != 0 {
                break;
            }
            if next & !0xf == target {
                self.pool.write32(at, qh::HEAD, self.pool.read32(pipe.qh, qh::HEAD));
                break;
            }
            match self.pool.slot_at(next & !0xf) {
                Some(slot) => at = slot,
                None => break,
            }
        }
        device_barrier();
        self.wait_frame(waiter);
        self.pool.give(pipe.qh);
    }

    fn failed(&mut self) -> bool {
        if !self.failed {
            let status = self.io.read16(reg::USBSTS);
            if status == u16::MAX || status & (reg::HSE | reg::HCPE | reg::HALTED) != 0 {
                self.failed = true;
            }
        }
        self.failed
    }

    fn halt(&mut self, waiter: &mut dyn Waiter) {
        self.io.write16(reg::USBCMD, 0);
        let io = &self.io;
        wait_for(waiter, HALT_US, || io.read16(reg::USBSTS) & reg::HALTED != 0);
        self.failed = true;
    }
}

/// Stops the controller `io` reaches, so that it reads and writes no
/// memory: before the machine is powered off or restarted. Whether it
/// halted.
pub fn stop(io: &dyn Ports, delay: &mut dyn FnMut(u32)) -> bool {
    io.write16(reg::USBINTR, 0);
    io.write16(reg::USBCMD, 0);
    for _ in 0..20 {
        if io.read16(reg::USBSTS) & reg::HALTED != 0 {
            return true;
        }
        delay(1000);
    }
    false
}
