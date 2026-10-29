// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Debug Capability (xHCI 7.6): a USB 3 device inside the controller,
//! which a second computer reads the kernel's console from on a machine
//! with no serial port (ADR 0160).
//!
//! Once the capability is enabled, the first of the controller's USB 3
//! root ports that a *debug host* is plugged into (with a USB 3 A-to-A
//! debug cable, whose data lines cross and whose power line is cut) stops
//! being a root port: the controller is a device on it. The debug host
//! enumerates it from what its context says (vendor, product, revision
//! and strings) and finds one interface with a bulk endpoint each way; a
//! Linux debug host makes it a serial port, `/dev/ttyUSB0`, with its
//! `usb_debug` driver.
//!
//! The capability has its own registers, event ring and two transfer
//! rings, which are named as this machine sees them: it writes on the OUT
//! ring and reads from the IN ring. It raises no interrupt, so [`Dbc`] is
//! polled, and nothing here waits: [`Dbc::poll`] moves it on, so that the
//! kernel can drive it from the controller's thread, from its debugger and
//! after a panic alike. It goes as Linux's driver goes:
//!
//! - **On.** Turned off first (DCE 0), it is given its event ring, rings
//!   and context, and turned on (DCE and LSE). It then waits for a debug
//!   host to connect (CCS), and for the debug host to configure it (DCR).
//!   A debug host that has not configured it within 5 s has it turned off
//!   and on again, which makes it try again.
//! - **Running.** One write is in flight at a time, a page at most, and
//!   one read of a packet waits for the debug host's bytes; what came is
//!   held until [`Dbc::read`] takes it, and only then is the next read
//!   started.
//! - **Halts.** The debug host may halt an endpoint (HOT and HIT say so).
//!   A transfer that stalled is turned into a No Op if the endpoint still
//!   points at it, so it does not go again once the halt clears; a
//!   transfer that waited meanwhile is rung again.
//! - **Gone.** Pulling the cable out, or the debug host resetting the
//!   port, ends the transfers in flight and empties the rings, and the
//!   capability waits for a debug host again. If it reads as off (a reset
//!   of the controller turns it off) it is set up from the start.
//!
//! Transfer events are matched by their TRB's address, since controllers
//! number the capability's endpoints differently (0 and 1 on AMD's, 2 and
//! 3 on Intel's).

use radix_hw::{Dma, Mmio, PAGE_SIZE};

use crate::ring::{EventRing, Ring, Trb, control, kind};
use crate::{Error, Event, Xhci, code, write64};

/// Its registers (7.6.8), from the capability's start.
mod reg {
    /// The Doorbell: its target in bits 8 to 15.
    pub const DOORBELL: usize = 0x04;
    /// The event ring's segment table: its size, its address (64 bits),
    /// and the dequeue pointer (64 bits).
    pub const ERSTSZ: usize = 0x08;
    pub const ERSTBA: usize = 0x10;
    pub const ERDP: usize = 0x18;
    pub const CONTROL: usize = 0x20;
    /// Its status: the root port it is on in bits 24 to 31.
    pub const STATUS: usize = 0x24;
    pub const PORTSC: usize = 0x28;
    /// Its context's address, 64 bits.
    pub const CONTEXT: usize = 0x30;
    /// Device Descriptor Info: the protocol and vendor, then the product
    /// and revision.
    pub const INFO1: usize = 0x38;
    pub const INFO2: usize = 0x3c;
    /// The length of the registers.
    pub const LEN: usize = 0x40;
}

/// DCCTRL's fields.
mod ctrl {
    /// DbC Run: the debug host has configured it.
    pub const RUN: u32 = 1 << 0;
    /// Link Status Event Enable, which Linux calls the port's enable and
    /// sets with DCE.
    pub const PORT_ENABLE: u32 = 1 << 1;
    /// The OUT and IN endpoints are halted.
    pub const HALT_OUT: u32 = 1 << 2;
    pub const HALT_IN: u32 = 1 << 3;
    /// DbC Run changed (write 1 to clear).
    pub const RUN_CHANGE: u32 = 1 << 4;
    /// The most packets a burst may have, 8 bits from bit 16.
    pub const BURST_SHIFT: u32 = 16;
    /// DbC Enable.
    pub const ENABLE: u32 = 1 << 31;
}

/// DCPORTSC's fields.
mod port {
    pub const CONNECTED: u32 = 1 << 0;
    pub const ENABLED: u32 = 1 << 1;
    /// A port reset finished (write 1 to clear).
    pub const RESET_CHANGE: u32 = 1 << 21;
}

/// The doorbell's targets: the OUT ring, the IN ring.
const OUT_TARGET: u32 = 0;
const IN_TARGET: u32 = 1;

/// Where things are in the context's page: the context (the info
/// context, then the OUT and IN endpoint contexts, 64 bytes each), then
/// the four string descriptors, 64 bytes each.
const INFO: usize = 0;
const OUT_CONTEXT: usize = 64;
const IN_CONTEXT: usize = 128;
const STRINGS: usize = 256;
/// The longest string descriptor: 64 bytes, so 31 characters.
const STRING_LEN: usize = 64;

/// The endpoint contexts' types: Bulk OUT and Bulk IN.
const BULK_OUT: u32 = 2;
const BULK_IN: u32 = 6;
/// A SuperSpeed bulk endpoint's packet.
const PACKET: u32 = 1024;
/// What a read asks for: one packet, so that every packet the debug host
/// sends ends it.
const READ_SIZE: usize = PACKET as usize;

/// How long it may take to turn off or on, and how long a debug host may
/// take to configure it once connected.
const SWITCH_US: u64 = 100_000;
const ENUMERATION_US: u64 = 5_000_000;

/// What the debug host is told the device is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The vendor ID.
    pub vendor: u16,
    /// The product ID.
    pub product: u16,
    /// The device's revision, in BCD.
    pub revision: u16,
    /// The DbC Protocol field: 0 for one of the vendor's, 1 for the GNU
    /// Remote Debug Command Set.
    pub protocol: u8,
    /// Its manufacturer, in US English, as are the other strings: at
    /// most 31 characters each, of which any but ASCII are sent as `?`.
    pub manufacturer: &'static str,
    /// What it is.
    pub product_name: &'static str,
    /// Its serial number.
    pub serial: &'static str,
}

/// Where the capability is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Off: not started, or stopped.
    Off,
    /// Being turned off, set up and turned on.
    Starting,
    /// On: waiting for a debug host.
    Waiting,
    /// A debug host is connected and configuring it.
    Connecting,
    /// Configured: bytes go both ways.
    Running,
    /// It would not turn off or on, and is left off.
    Failed,
}

/// Where it is, as the driver moves it on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Off,
    TurningOff,
    TurningOn,
    Waiting,
    Connecting,
    Running,
    Failed,
}

/// What changed in a [`Dbc::poll`], for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// It is on, and waits for a debug host.
    Enabled,
    /// A debug host connected.
    Connected,
    /// The debug host configured it, on root port `port`.
    Configured {
        /// The root port, from 1.
        port: u8,
    },
    /// The debug host is gone: the cable was pulled out, the debug host
    /// unconfigured it, or it reset the port (`reset`). What was in
    /// flight is lost.
    Disconnected {
        /// Whether it reset the port.
        reset: bool,
    },
    /// It was turned off from outside (by a reset of the controller), and
    /// is set up again.
    TurnedOff,
    /// The debug host did not configure it in time; it is turned off and
    /// on again.
    NotConfigured,
    /// It would not turn off or on.
    Failed(&'static str),
}

/// A transfer ring of the capability, and its page for data.
struct Pipe {
    ring: Ring,
    /// The doorbell's target for it, and where its endpoint context is.
    target: u32,
    context: usize,
    /// The TRB of its transfer in flight.
    pending: Option<u64>,
    /// Whether the debug host has halted it.
    halted: bool,
}

impl Pipe {
    fn new(ring: Ring, target: u32, context: usize) -> Pipe {
        Pipe { ring, target, context, pending: None, halted: false }
    }

    /// Its page for data's physical address.
    fn buffer(&self) -> u64 {
        self.ring.phys() + PAGE_SIZE as u64
    }

    /// Empties it, ending what was in flight.
    fn reset(&mut self) {
        self.ring.reset();
        self.pending = None;
        self.halted = false;
    }

    /// Notes whether it is halted now; says whether a halt just cleared.
    fn halt_cleared(&mut self, halted: bool) -> bool {
        let cleared = self.halted && !halted;
        self.halted = halted;
        cleared
    }
}

/// The Debug Capability of a controller, which the driver runs.
pub struct Dbc {
    regs: Mmio,
    identity: Identity,
    /// The context and strings, a page.
    memory: Dma,
    events: EventRing,
    out: Pipe,
    input: Pipe,
    phase: Phase,
    /// When it came to its phase, on the caller's clock.
    since: u64,
    /// The most packets of a burst, as the capability says.
    burst: u32,
    /// What the last read brought, in the IN pipe's page, and how much of
    /// it [`Dbc::read`] has taken.
    came: usize,
    taken: usize,
}

impl Dbc {
    /// The Debug Capability of `xhci`, given its memory from `alloc` (a
    /// number of pages, and whether they must be below 4 GiB). It is off
    /// until [`Dbc::start`].
    #[inline(never)]
    pub fn new(
        xhci: &Xhci,
        identity: Identity,
        alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>,
    ) -> Result<Dbc, Error> {
        let at =
            xhci.info().debug_capability.ok_or(Error::Unsupported("it has no Debug Capability"))?;
        let regs = xhci
            .registers()
            .window(at, reg::LEN)
            .ok_or(Error::Unsupported("its Debug Capability is outside its window"))?;
        let below_4g = xhci.below_4g();
        let mut page = |pages: usize| alloc(pages, below_4g).ok_or(Error::NoMemory);
        let memory = page(1)?;
        memory.zero();
        let events = EventRing::new(page(1)?, page(1)?).ok_or(Error::NoMemory)?;
        let out = Ring::new(page(2)?).ok_or(Error::NoMemory)?;
        let input = Ring::new(page(2)?).ok_or(Error::NoMemory)?;
        Ok(Dbc {
            regs,
            identity,
            memory,
            events,
            out: Pipe::new(out, OUT_TARGET, OUT_CONTEXT),
            input: Pipe::new(input, IN_TARGET, IN_CONTEXT),
            phase: Phase::Off,
            since: 0,
            burst: 0,
            came: 0,
            taken: 0,
        })
    }

    /// Turns it off, to be set up and turned on by the next polls; `now`
    /// is the time in microseconds, on the clock [`Dbc::poll`] is given.
    pub fn start(&mut self, now: u64) {
        self.regs.write32(reg::CONTROL, 0);
        self.out.pending = None;
        self.input.pending = None;
        self.enter(Phase::TurningOff, now);
    }

    /// Turns it off for good, before the machine powers off or restarts,
    /// so that it stops using memory.
    pub fn stop(&mut self) {
        self.regs.write32(reg::CONTROL, 0);
        self.out.pending = None;
        self.input.pending = None;
        self.phase = Phase::Off;
    }

    /// Where it is.
    pub fn status(&self) -> Status {
        match self.phase {
            Phase::Off => Status::Off,
            Phase::TurningOff | Phase::TurningOn => Status::Starting,
            Phase::Waiting => Status::Waiting,
            Phase::Connecting => Status::Connecting,
            Phase::Running => Status::Running,
            Phase::Failed => Status::Failed,
        }
    }

    /// Whether a write is in flight.
    pub fn writing(&self) -> bool {
        self.out.pending.is_some()
    }

    /// Looks at it, and moves it on; `now` is the time in microseconds.
    /// Says what changed, if anything did.
    pub fn poll(&mut self, now: u64) -> Option<Change> {
        let control = self.regs.read32(reg::CONTROL);
        match self.phase {
            Phase::Off | Phase::Failed => return None,
            Phase::TurningOff if control & ctrl::ENABLE != 0 => {
                return self.late(now).then(|| self.fail("it did not turn off"));
            }
            Phase::TurningOff => {
                self.set_up(control);
                self.regs.write32(reg::CONTROL, ctrl::ENABLE | ctrl::PORT_ENABLE);
                self.enter(Phase::TurningOn, now);
                return self.turning_on(now);
            }
            Phase::TurningOn => return self.turning_on(now),
            Phase::Waiting | Phase::Connecting | Phase::Running => {}
        }
        if control & ctrl::ENABLE == 0 {
            self.start(now);
            return Some(Change::TurnedOff);
        }
        let change = match self.phase {
            Phase::Waiting => self.waiting(now),
            Phase::Connecting => self.connecting(control, now),
            _ => self.running(control, now),
        };
        self.drain();
        change
    }

    /// Starts a write of `data`, a page of it at most, to the debug host;
    /// returns how much it took: none while it is not running, while a
    /// write is in flight, or while the debug host has halted the OUT
    /// endpoint.
    pub fn write(&mut self, data: &[u8]) -> usize {
        if self.phase != Phase::Running
            || self.out.pending.is_some()
            || self.out.halted
            || data.is_empty()
        {
            return 0;
        }
        let length = data.len().min(PAGE_SIZE);
        self.out.ring.memory().write(PAGE_SIZE, &data[..length]);
        let at = self.out.ring.push(Trb {
            parameter: self.out.buffer(),
            status: length as u32,
            control: control::kind(kind::NORMAL) | control::IOC,
        });
        self.out.pending = Some(at);
        self.ring(self.out.target);
        length
    }

    /// Copies what the debug host sent into `out`; returns how much. Once
    /// all of a read is taken, the next starts.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let length = (self.came - self.taken).min(out.len());
        if length > 0 {
            self.input.ring.memory().read(PAGE_SIZE + self.taken, &mut out[..length]);
            self.taken += length;
        }
        if self.came > 0 && self.taken == self.came {
            self.came = 0;
            self.taken = 0;
            self.arm_read();
        }
        length
    }

    fn enter(&mut self, phase: Phase, now: u64) {
        self.phase = phase;
        self.since = now;
    }

    /// Whether it has taken too long to turn off or on.
    fn late(&self, now: u64) -> bool {
        now.saturating_sub(self.since) >= SWITCH_US
    }

    fn fail(&mut self, why: &'static str) -> Change {
        self.regs.write32(reg::CONTROL, 0);
        self.phase = Phase::Failed;
        Change::Failed(why)
    }

    fn ring(&self, target: u32) {
        self.regs.write32(reg::DOORBELL, target << 8);
    }

    fn turning_on(&mut self, now: u64) -> Option<Change> {
        if self.regs.read32(reg::CONTROL) & ctrl::ENABLE != 0 {
            self.enter(Phase::Waiting, now);
            Some(Change::Enabled)
        } else if self.late(now) {
            Some(self.fail("it did not turn on"))
        } else {
            None
        }
    }

    fn waiting(&mut self, now: u64) -> Option<Change> {
        if self.regs.read32(reg::PORTSC) & port::CONNECTED == 0 {
            return None;
        }
        self.enter(Phase::Connecting, now);
        Some(Change::Connected)
    }

    fn connecting(&mut self, control: u32, now: u64) -> Option<Change> {
        if control & ctrl::RUN != 0 {
            // Configuring it reset the port: clear that, and the rest.
            let portsc = self.regs.read32(reg::PORTSC);
            self.regs.write32(reg::PORTSC, portsc);
            self.clear_run_change(control);
            self.enter(Phase::Running, now);
            self.arm_read();
            let port = (self.regs.read32(reg::STATUS) >> 24) as u8;
            return Some(Change::Configured { port });
        }
        if self.regs.read32(reg::PORTSC) & port::CONNECTED == 0 {
            self.enter(Phase::Waiting, now);
            return None;
        }
        if now.saturating_sub(self.since) >= ENUMERATION_US {
            self.start(now);
            return Some(Change::NotConfigured);
        }
        None
    }

    fn running(&mut self, control: u32, now: u64) -> Option<Change> {
        let portsc = self.regs.read32(reg::PORTSC);
        if portsc & port::RESET_CHANGE != 0 {
            self.regs.write32(reg::PORTSC, portsc);
            self.clear_run_change(control);
            self.lost(now);
            return Some(Change::Disconnected { reset: true });
        }
        if portsc & (port::CONNECTED | port::ENABLED) == 0 || control & ctrl::RUN == 0 {
            self.clear_run_change(control);
            self.lost(now);
            return Some(Change::Disconnected { reset: false });
        }
        if self.out.halt_cleared(control & ctrl::HALT_OUT != 0) && self.out.pending.is_some() {
            self.ring(self.out.target);
        }
        if self.input.halt_cleared(control & ctrl::HALT_IN != 0) {
            if self.input.pending.is_some() {
                self.ring(self.input.target);
            } else {
                self.arm_read();
            }
        }
        self.clear_run_change(control);
        None
    }

    /// Clears DbC Run Change, if `control` has it, keeping it on; the
    /// halt bits are not written back, so they stay as they are.
    fn clear_run_change(&self, control: u32) {
        if control & ctrl::RUN_CHANGE != 0 {
            self.regs.write32(
                reg::CONTROL,
                (control & (ctrl::ENABLE | ctrl::PORT_ENABLE)) | ctrl::RUN_CHANGE,
            );
        }
    }

    /// The debug host is gone: what was in flight is lost, and the rings
    /// start again for the next.
    fn lost(&mut self, now: u64) {
        self.out.reset();
        self.input.reset();
        self.came = 0;
        self.taken = 0;
        self.write_endpoints();
        self.enter(Phase::Waiting, now);
    }

    /// Starts a read of a packet, unless one waits or what came has not
    /// all been taken.
    fn arm_read(&mut self) {
        if self.phase != Phase::Running
            || self.input.pending.is_some()
            || self.input.halted
            || self.came > 0
        {
            return;
        }
        let at = self.input.ring.push(Trb {
            parameter: self.input.buffer(),
            status: READ_SIZE as u32,
            control: control::kind(kind::NORMAL) | control::IOC | control::ISP,
        });
        self.input.pending = Some(at);
        self.ring(self.input.target);
    }

    /// Reads the events that came.
    fn drain(&mut self) {
        let mut read = false;
        while let Some(trb) = self.events.pop() {
            read = true;
            match Event::decode(trb) {
                Event::Transfer { trb, code, residual, .. } => {
                    self.transferred(trb, code, residual);
                }
                Event::Port { .. } => {
                    // A port reset is for the running phase to see.
                    let portsc = self.regs.read32(reg::PORTSC);
                    self.regs.write32(reg::PORTSC, portsc & !port::RESET_CHANGE);
                }
                _ => {}
            }
        }
        if read {
            write64(&self.regs, reg::ERDP, self.events.dequeue_phys());
        }
    }

    /// The transfer at `trb` ended with `code`, `residual` bytes short.
    fn transferred(&mut self, trb: u64, code: u8, residual: u32) {
        if self.out.pending == Some(trb) {
            self.out.pending = None;
            if code == code::STALL {
                Dbc::skip_stalled(&self.memory, &self.out, trb);
            }
        } else if self.input.pending == Some(trb) {
            self.input.pending = None;
            match code {
                code::SUCCESS | code::SHORT_PACKET => {
                    self.came = READ_SIZE.saturating_sub(residual as usize);
                    self.taken = 0;
                }
                code::STALL => Dbc::skip_stalled(&self.memory, &self.input, trb),
                _ => {}
            }
            self.arm_read();
        }
    }

    /// A transfer at `trb` of `pipe` stalled: if its endpoint context
    /// (in `memory`) still points at it, it becomes a No Op, so that it
    /// does not go again once the debug host clears the halt.
    fn skip_stalled(memory: &Dma, pipe: &Pipe, trb: u64) {
        if memory.read64(pipe.context + 8) & !0xf != trb || !pipe.ring.holds(trb) {
            return;
        }
        let at = (trb - pipe.ring.phys()) as usize;
        let cycle = pipe.ring.memory().read32(at + 12) & control::CYCLE;
        pipe.ring.memory().write32(at + 12, control::kind(kind::NO_OP) | cycle);
    }

    /// Gives it its event ring, empty rings and its context, and says
    /// what it is: all while it is off.
    fn set_up(&mut self, control: u32) {
        self.events.reset();
        self.out.reset();
        self.input.reset();
        self.came = 0;
        self.taken = 0;
        self.burst = (control >> ctrl::BURST_SHIFT) & 0xff;
        self.regs.write32(reg::ERSTSZ, 1);
        write64(&self.regs, reg::ERSTBA, self.events.table_phys());
        write64(&self.regs, reg::ERDP, self.events.dequeue_phys());
        self.write_info();
        self.write_endpoints();
        write64(&self.regs, reg::CONTEXT, self.memory.phys() + INFO as u64);
        let id = self.identity;
        self.regs.write32(reg::INFO1, u32::from(id.vendor) << 16 | u32::from(id.protocol));
        self.regs.write32(reg::INFO2, u32::from(id.revision) << 16 | u32::from(id.product));
    }

    /// The info context: where the string descriptors are, and their
    /// lengths.
    fn write_info(&self) {
        let id = self.identity;
        // String 0: the languages, US English alone.
        self.memory.write(STRINGS, &[4, 3, 0x09, 0x04]);
        let mut lengths = 4u32;
        for (i, text) in [id.manufacturer, id.product_name, id.serial].into_iter().enumerate() {
            let length = self.write_string(STRINGS + (i + 1) * STRING_LEN, text);
            lengths |= u32::from(length) << (8 * (i + 1));
        }
        for i in 0..4 {
            let phys = self.memory.phys() + (STRINGS + i * STRING_LEN) as u64;
            self.memory.write64(INFO + 8 * i, phys);
        }
        self.memory.write32(INFO + 32, lengths);
    }

    /// Writes `text` as a string descriptor at `at`; returns its length.
    fn write_string(&self, at: usize, text: &str) -> u8 {
        let mut length = 2;
        for c in text.chars().take((STRING_LEN - 2) / 2) {
            let unit = if c.is_ascii() { c as u16 } else { u16::from(b'?') };
            self.memory.write16(at + length, unit);
            length += 2;
        }
        self.memory.write8(at, length as u8);
        self.memory.write8(at + 1, radix_usb::descriptor::STRING);
        length as u8
    }

    /// The endpoint contexts: bulk endpoints of a SuperSpeed packet, at
    /// the start of their rings.
    fn write_endpoints(&self) {
        for (pipe, kind) in [(&self.out, BULK_OUT), (&self.input, BULK_IN)] {
            let info2 = kind << 3 | self.burst << 8 | PACKET << 16;
            self.memory.write32(pipe.context, 0);
            self.memory.write32(pipe.context + 4, info2);
            self.memory.write64(pipe.context + 8, pipe.ring.dequeue_pointer());
        }
    }
}
