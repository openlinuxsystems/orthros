// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! AHCI, the Serial ATA host controller of PCs since 2004 (Intel's
//! Serial ATA AHCI 1.3.1 specification), and the ATA disks on it.
//!
//! An AHCI controller (a *host bus adapter*, HBA) is one PCI function with
//! its registers in BAR 5: a block for the whole controller, then one for
//! each of up to 32 ports, a port a link to one disk. Each port has a
//! *command list* of up to 32 slots in memory, each slot naming a *command
//! table* that holds the command's register frame and the list of buffers
//! its data move to or from (the PRD table), and an area where the
//! controller puts the frames the disk sends back. The driver fills in a
//! slot and sets its bit in the port's command issue register; the
//! controller clears the bit when the command is done.
//!
//! - [`Hba`] takes the controller over from the firmware, resets it, and
//!   finds the disks on its ports ([`Hba::probe`]): it resets every link
//!   at once, waits for them to come up, and asks each disk what it is
//!   (IDENTIFY DEVICE, [`ata::identify`]).
//! - [`Port`] runs one disk: reads and writes as native command queuing
//!   (NCQ) commands, up to 32 in flight, when the disk queues and DMA
//!   commands one at a time when it does not; cache flushes, writes
//!   through the cache (FUA) and TRIM, which the disk takes only while
//!   nothing else is in flight; and recovery after an error.
//! - [`ata`] builds the commands and reads what a disk says of itself.
//!
//! The crate is a compartment: it has no `unsafe` code, and reaches the
//! controller only through the [`Mmio`] windows and [`Dma`] memory that
//! `radix-hw` gives it. Nothing here sleeps or takes a lock: the kernel
//! passes in how to wait (`delay`) where the controller needs time, and
//! decides how threads wait for completions (in `radix`, woken from the
//! controller's interrupt). The tests run the driver against a simulated
//! controller and disk on the host, and Kani proves that reading what a
//! disk says of itself cannot go wrong, whatever it says.

#![cfg_attr(not(any(test, kani)), no_std)]
#![forbid(unsafe_code)]

pub mod ata;
mod port;

#[cfg(test)]
mod tests;

use core::fmt;

pub use port::{Info, MAX_SEGMENTS, Op, Outcome, Port};
use radix_hw::{Dma, Mmio};

/// The PCI class, subclass and programming interface of an AHCI
/// controller: mass storage, Serial ATA, AHCI 1.0.
pub const PCI_CLASS: (u8, u8, u8) = (0x01, 0x06, 0x01);
/// The BAR that holds the controller's registers (ABAR).
pub const BAR: usize = 5;

/// The controller's own registers (AHCI 1.3.1, 3.1).
mod hba {
    /// Capabilities.
    pub const CAP: usize = 0x00;
    /// Global control.
    pub const GHC: usize = 0x04;
    /// Interrupt status, a bit per port.
    pub const IS: usize = 0x08;
    /// Ports implemented, a bit per port.
    pub const PI: usize = 0x0c;
    /// Version.
    pub const VS: usize = 0x10;
    /// More capabilities.
    pub const CAP2: usize = 0x24;
    /// BIOS/OS handoff control and status.
    pub const BOHC: usize = 0x28;
    /// Where the first port's registers start.
    pub const PORTS: usize = 0x100;
    /// The size of each port's.
    pub const PORT_SIZE: usize = 0x80;

    /// CAP: 64-bit addresses.
    pub const CAP_S64A: u32 = 1 << 31;
    /// CAP: native command queuing.
    pub const CAP_SNCQ: u32 = 1 << 30;
    /// CAP: command list override (clears a stuck busy bit).
    pub const CAP_SCLO: u32 = 1 << 24;
    /// CAP: the controller has only AHCI mode.
    pub const CAP_SAM: u32 = 1 << 18;

    /// GHC: AHCI enable.
    pub const GHC_AE: u32 = 1 << 31;
    /// GHC: interrupt enable.
    pub const GHC_IE: u32 = 1 << 1;
    /// GHC: reset the controller.
    pub const GHC_HR: u32 = 1 << 0;

    /// CAP2: BIOS/OS handoff.
    pub const CAP2_BOH: u32 = 1 << 0;
    /// BOHC: the firmware is busy cleaning up.
    pub const BOHC_BB: u32 = 1 << 4;
    /// BOHC: the operating system owns the controller.
    pub const BOHC_OOS: u32 = 1 << 1;
    /// BOHC: the firmware owns the controller.
    pub const BOHC_BOS: u32 = 1 << 0;
}

/// What can go wrong with the controller or a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The controller's registers are too small for what it says it has.
    Registers,
    /// The controller did not finish resetting, or the firmware did not
    /// let it go.
    ResetFailed,
    /// No memory for the port's command list and tables.
    NoMemory,
    /// The request is malformed: not whole sectors, past the disk's end,
    /// too many buffers, or something the disk does not do.
    BadRequest,
    /// A buffer is above 4 GiB on a controller that reaches only the
    /// first 4 GiB.
    Unreachable,
    /// The port has no room for the request now; try once a request in
    /// flight is done.
    Full,
    /// The port stopped after an error, and takes nothing until it
    /// recovers ([`Port::recover`]).
    Stopped,
    /// The disk is gone, or stopped answering.
    Dead,
}

impl Error {
    /// What went wrong, in words.
    pub fn as_str(&self) -> &'static str {
        match self {
            Error::Registers => "its registers are too small",
            Error::ResetFailed => "it did not reset",
            Error::NoMemory => "out of memory",
            Error::BadRequest => "a malformed request",
            Error::Unreachable => "a buffer above 4 GiB, which it cannot reach",
            Error::Full => "no room for the request",
            Error::Stopped => "the port stopped after an error",
            Error::Dead => "the disk stopped answering",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a port has no disk the driver runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Absent {
    /// Nothing is plugged in, or the link did not come up.
    NoLink,
    /// The disk stayed busy.
    NotReady,
    /// An optical drive or another device that speaks ATAPI.
    Atapi,
    /// A port multiplier, which the driver does not handle.
    PortMultiplier,
    /// Some other signature.
    Unknown(u32),
    /// The disk did not answer IDENTIFY DEVICE.
    NoIdentity,
    /// The disk is not one the driver handles.
    Unsupported(ata::Unsupported),
    /// The port's engine would not stop or start.
    Stuck,
    /// No memory for the port.
    NoMemory,
}

impl fmt::Display for Absent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Absent::NoLink => f.write_str("no link"),
            Absent::NotReady => f.write_str("the disk stayed busy"),
            Absent::Atapi => f.write_str("an ATAPI device (an optical drive)"),
            Absent::PortMultiplier => f.write_str("a port multiplier"),
            Absent::Unknown(signature) => write!(f, "an unknown device (signature {signature:#x})"),
            Absent::NoIdentity => f.write_str("the disk did not say what it is"),
            Absent::Unsupported(why) => match why {
                ata::Unsupported::Checksum => f.write_str("its identity's checksum is wrong"),
                ata::Unsupported::NotAta => f.write_str("not an ATA device"),
                ata::Unsupported::No48Bit => f.write_str("it has no 48-bit addresses"),
                ata::Unsupported::SectorSize(size) => {
                    write!(f, "its sectors are {size} bytes, and only 512 are supported")
                }
                ata::Unsupported::Empty => f.write_str("it has no sectors"),
            },
            Absent::Stuck => f.write_str("the port would not start"),
            Absent::NoMemory => f.write_str("out of memory"),
        }
    }
}

/// An AHCI controller, taken over from the firmware and reset.
pub struct Hba {
    regs: Mmio,
    cap: u32,
    implemented: u32,
    version: u32,
}

impl fmt::Debug for Hba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hba")
            .field("cap", &format_args!("{:#x}", self.cap))
            .field("implemented", &format_args!("{:#x}", self.implemented))
            .finish_non_exhaustive()
    }
}

/// How long the firmware may take to let the controller go, and how long
/// more when it says it is busy (AHCI 1.3.1, 10.6.3).
const HANDOFF_US: u32 = 25_000;
const HANDOFF_BUSY_US: u32 = 2_000_000;
/// How long a controller reset may take (10.4.3).
const RESET_US: u32 = 1_000_000;
/// How often the driver looks again while it waits for the controller.
const POLL_US: u32 = 1_000;

/// Waits up to `limit_us` microseconds, `step_us` at a time, for `done`.
fn wait_for(
    delay: &mut dyn FnMut(u32),
    limit_us: u32,
    step_us: u32,
    mut done: impl FnMut() -> bool,
) -> bool {
    let mut waited = 0;
    loop {
        if done() {
            return true;
        }
        if waited >= limit_us {
            return false;
        }
        delay(step_us);
        waited += step_us;
    }
}

impl Hba {
    /// Takes over the controller whose registers are `regs`: from the
    /// firmware first, if it says it has them (BIOS/OS handoff), then
    /// resets it into AHCI mode with its interrupts off. `delay` waits the
    /// given number of microseconds.
    pub fn new(regs: Mmio, delay: &mut dyn FnMut(u32)) -> Result<Hba, Error> {
        if regs.len() < hba::PORTS {
            return Err(Error::Registers);
        }
        let cap2 = regs.read32(hba::CAP2);
        if cap2 & hba::CAP2_BOH != 0 {
            let bohc = regs.read32(hba::BOHC);
            regs.write32(hba::BOHC, bohc | hba::BOHC_OOS);
            let mut released = wait_for(delay, HANDOFF_US, POLL_US, || {
                regs.read32(hba::BOHC) & hba::BOHC_BOS == 0
            });
            if !released && regs.read32(hba::BOHC) & hba::BOHC_BB != 0 {
                released = wait_for(delay, HANDOFF_BUSY_US, POLL_US, || {
                    regs.read32(hba::BOHC) & hba::BOHC_BOS == 0
                });
            }
            if !released {
                return Err(Error::ResetFailed);
            }
        }
        // AHCI mode first: a controller that also emulates IDE resets into
        // the mode it is in.
        regs.write32(hba::GHC, regs.read32(hba::GHC) | hba::GHC_AE);
        regs.write32(hba::GHC, hba::GHC_AE | hba::GHC_HR);
        if !wait_for(delay, RESET_US, POLL_US, || regs.read32(hba::GHC) & hba::GHC_HR == 0) {
            return Err(Error::ResetFailed);
        }
        regs.write32(hba::GHC, hba::GHC_AE);
        let cap = regs.read32(hba::CAP);
        let count = (cap & 0x1f) + 1;
        let all = if count == 32 { u32::MAX } else { (1 << count) - 1 };
        // The firmware sets which ports are wired; a controller it never
        // set up says none, and then every port it has is tried.
        let implemented = match regs.read32(hba::PI) {
            0 => all,
            ports => ports,
        };
        let last = 32 - implemented.leading_zeros() as usize;
        if regs.len() < hba::PORTS + last * hba::PORT_SIZE {
            return Err(Error::Registers);
        }
        let version = regs.read32(hba::VS);
        Ok(Hba { regs, cap, implemented, version })
    }

    /// Its AHCI version, as (major, minor): (1, 31) for 1.3.1.
    pub fn version(&self) -> (u16, u16) {
        ((self.version >> 16) as u16, self.version as u16)
    }

    /// The ports that are wired, a bit each.
    pub fn implemented(&self) -> u32 {
        self.implemented
    }

    /// How many command slots each port has.
    pub fn slots(&self) -> u8 {
        ((self.cap >> 8) & 0x1f) as u8 + 1
    }

    /// Whether it does native command queuing.
    pub fn ncq(&self) -> bool {
        self.cap & hba::CAP_SNCQ != 0
    }

    /// Whether it reaches memory above 4 GiB.
    pub fn wide(&self) -> bool {
        self.cap & hba::CAP_S64A != 0
    }

    /// Whether it only has AHCI mode (no IDE emulation).
    pub fn ahci_only(&self) -> bool {
        self.cap & hba::CAP_SAM != 0
    }

    /// The registers of port `index`, if it is wired.
    fn port_regs(&self, index: usize) -> Option<Mmio> {
        if index >= 32 || self.implemented & (1 << index) == 0 {
            return None;
        }
        self.regs.window(hba::PORTS + index * hba::PORT_SIZE, hba::PORT_SIZE)
    }

    /// Finds the disks: sets up every wired port, resets every link at
    /// once, waits for them, and asks each disk what it is. Calls `found`
    /// with each port's number and its disk, or why it has none (ports
    /// with nothing plugged in are left out). `ports` holds the ports
    /// while their links come up, the first in its first entry: the
    /// caller's, so that it need not be on a stack, and empty again when
    /// this returns; ports past its end are not looked at. `alloc` gives
    /// zeroed DMA memory in pages, and `delay` waits the given number of
    /// microseconds. The ports' interrupts are on when they are handed
    /// over; the controller's are not ([`Hba::enable_interrupts`]).
    pub fn probe(
        &self,
        ports: &mut [Option<Port>],
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
        delay: &mut dyn FnMut(u32),
        found: &mut dyn FnMut(usize, Result<Port, Absent>),
    ) {
        let len = ports.len().min(32);
        let ports = &mut ports[..len];
        for slot in ports.iter_mut() {
            *slot = None;
        }
        for (index, slot) in ports.iter_mut().enumerate() {
            let Some(regs) = self.port_regs(index) else { continue };
            match Port::prepare(regs, self.cap, alloc, delay) {
                Ok(port) => *slot = Some(port),
                Err(why) => found(index, Err(why)),
            }
        }
        // Every link's reset together: the signal must last a millisecond.
        for port in ports.iter_mut().flatten() {
            port.begin_link_reset();
        }
        delay(port::COMRESET_US);
        for port in ports.iter_mut().flatten() {
            port.end_link_reset();
        }
        let mut links = port::Links::new(ports);
        links.settle(ports, delay);
        for (index, slot) in ports.iter_mut().enumerate() {
            let Some(mut port) = slot.take() else { continue };
            if !links.up(index) {
                if links.present(index) {
                    found(index, Err(Absent::NoLink));
                }
                continue;
            }
            found(index, port.start(delay).map(|()| port));
        }
    }

    /// Turns the controller's interrupt on: one for all ports, an MSI.
    pub fn enable_interrupts(&self) {
        let ghc = self.regs.read32(hba::GHC);
        self.regs.write32(hba::GHC, ghc | hba::GHC_IE);
    }

    /// Turns it off.
    pub fn disable_interrupts(&self) {
        let ghc = self.regs.read32(hba::GHC);
        self.regs.write32(hba::GHC, ghc & !hba::GHC_IE);
    }

    /// The ports with an interrupt pending, a bit each. The interrupt
    /// handler collects each one's completions ([`Port::complete`], which
    /// clears the port's own status first) and then clears these bits
    /// ([`Hba::acknowledge`]).
    pub fn pending(&self) -> u32 {
        self.regs.read32(hba::IS) & self.implemented
    }

    /// Clears the controller's interrupt status for `ports`.
    pub fn acknowledge(&self, ports: u32) {
        self.regs.write32(hba::IS, ports);
    }
}
