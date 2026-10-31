// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chipset's SMBus host controller (Phase 11e, ADR 0206): Intel's,
//! from the ICH to today's PCHs, and that of AMD's Fusion Controller Hub
//! (FCH) on Zen.
//!
//! The SMBus is the chipset's slow two-wire bus to the memory modules'
//! SPD EEPROMs, sensor chips and the like. Both controllers have the
//! register layout of Intel's PIIX4 at an I/O base: the host status
//! (0), control (2), command (3), the target's address (4) and two data
//! bytes (5 and 6). A transaction is set up in those, then started by
//! the control register's START bit (40h); the status says when it is
//! done (INTR), or that no device answered (DEV_ERR), that another
//! master took the bus (BUS_ERR) or that it failed (FAILED). Where each
//! finds its base and what else it needs:
//!
//! - Intel's SMBus function (00:1f.3, or 00:1f.4 on the PCHs from the 100
//!   Series on; class 0C05h) decodes its registers at the I/O base its
//!   BAR 4 holds once HOSTC's HST_EN (configuration register 40h, bit 0)
//!   is set, which firmware may have left clear. Reading the status sets
//!   INUSE_STS, the semaphore firmware's SMI handler shares, which is
//!   written back after each transaction (the ICH9 and Intel's PCH
//!   datasheets give these).
//! - The FCH's (in its SMBus function 1022:790b from revision 49h, at
//!   00:14.0) is enabled by bit 4 of its power management registers'
//!   DecodeEn, at the base in DecodeEn's second byte times 256. The FCH
//!   has four SMBus ports behind the one controller, which DecodeEn's
//!   third byte picks (bits 4:3), and an integrated microcontroller that
//!   may use the bus too: a transaction first takes the host semaphore
//!   (bit 4 of the controller's register 8, which reads back set once the
//!   host owns the bus; bit 5 gives it back), picks port 0, where the
//!   memory modules are, and puts the port back after. On Zen (from
//!   revision 51h) the power management registers are reached in memory
//!   at 0xfed80300 rather than through the index and data ports 0xcd6
//!   and 0xcd7. (AMD's BIOS and Kernel Developer's Guides and Processor
//!   Programming References give these; Linux's `i2c-piix4` is the
//!   reference for the port and the semaphore.)
//!
//! Only byte and word transfers are done: what reading an SPD EEPROM or
//! a sensor chip needs. The crate has no `unsafe` code; the kernel gives
//! it bus 0's configuration space and reaches I/O ports and device memory
//! for it ([`Platform`]). Its tests run it against simulated controllers.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use core::fmt;

use radix_hw::Mmio;
use radix_hw::pci::{self, Address};

/// What the controller needs from the kernel besides configuration space.
pub trait Platform {
    /// Maps `len` bytes of device memory at `phys`; `None` if it is RAM.
    fn map(&self, phys: u64, len: usize) -> Option<Mmio>;
    /// Reads an I/O port.
    fn in8(&self, port: u16) -> u8;
    /// Writes an I/O port.
    fn out8(&self, port: u16, value: u8);
    /// Waits at least `us` microseconds.
    fn delay_us(&self, us: u64);
}

const INTEL: u16 = 0x8086;
const AMD: u16 = 0x1022;

/// The registers from the base.
const HOST_STATUS: u16 = 0;
const HOST_CONTROL: u16 = 2;
const HOST_COMMAND: u16 = 3;
const TARGET_ADDRESS: u16 = 4;
const DATA0: u16 = 5;
const DATA1: u16 = 6;
/// The FCH's SMBus target control, with the host semaphore.
const FCH_SEMAPHORE: u16 = 8;

/// The host status register.
const HOST_BUSY: u8 = 1 << 0;
const INTR: u8 = 1 << 1;
const DEV_ERR: u8 = 1 << 2;
const BUS_ERR: u8 = 1 << 3;
const FAILED: u8 = 1 << 4;
/// Intel's: the semaphore firmware shares, and the byte done.
const INUSE: u8 = 1 << 6;
const BYTE_DONE: u8 = 1 << 7;
/// What a transaction leaves set, which is written back to clear it.
const DONE_FLAGS: u8 = BYTE_DONE | FAILED | BUS_ERR | DEV_ERR | INTR;

/// The host control register: the protocols, and START.
const BYTE_DATA: u8 = 0x08;
const WORD_DATA: u8 = 0x0c;
const START: u8 = 1 << 6;
/// Intel's: stops the transaction.
const KILL: u8 = 1 << 1;

/// The FCH's semaphore: take it, and give it back.
const SEMAPHORE_TAKE: u8 = 1 << 4;
const SEMAPHORE_GIVE: u8 = 1 << 5;

/// How long a transaction may take, and how often its status is read.
const POLL_US: u64 = 250;
const POLLS: u32 = 800;
/// How many times the FCH's semaphore is asked for, a millisecond apart.
const SEMAPHORE_TRIES: u32 = 500;

/// Intel's SMBus function: its class, where it is, BAR 4 and HOSTC.
const SMBUS_CLASS: (u8, u8) = (0x0c, 0x05);
const INTEL_SMBUS: [(u8, u8); 2] = [(31, 3), (31, 4)];
const INTEL_BAR: usize = 0x20;
const HOSTC: usize = 0x40;
const HST_EN: u8 = 1 << 0;

/// The FCH's SMBus function, and the revisions from which it is as here
/// and from which its power management registers are in memory.
const FCH_SMBUS: (u8, u8) = (20, 0);
const FCH_KERNCZ: (u16, u8) = (0x790b, 0x49);
const FCH_ZEN_REVISION: u8 = 0x51;
/// The power management registers: index and data ports, or where Zen
/// has them in memory.
const PM_INDEX: u16 = 0xcd6;
const PM_DATA: u16 = 0xcd7;
const PM_MMIO: u64 = 0xfed8_0300;
/// DecodeEn's bytes: the SMBus's enable (bit 4), its base's high byte,
/// and the port it is on (bits 4:3).
const PM_DECODE_EN0: u8 = 0x00;
const SMBUS_EN: u8 = 1 << 4;
const PM_DECODE_EN1: u8 = 0x01;
const PM_DECODE_EN2: u8 = 0x02;
const PORT_MASK: u8 = 0b11 << 3;

/// Why a transaction did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No device answered at the address.
    NoDevice,
    /// The controller was busy, or the FCH's microcontroller kept the bus.
    Busy,
    /// Another master took the bus.
    Collision,
    /// The controller said the transaction failed.
    Failed,
    /// It did not finish in time.
    Timeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::NoDevice => "no device answered",
            Error::Busy => "the bus is busy",
            Error::Collision => "another master took the bus",
            Error::Failed => "the transaction failed",
            Error::Timeout => "the transaction did not finish in time",
        })
    }
}

/// Why a controller there is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unusable {
    /// Intel's has no I/O base.
    NoBase,
    /// The FCH's is not enabled, or its registers are not reachable.
    FchOff,
}

impl fmt::Display for Unusable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Unusable::NoBase => "Intel's SMBus controller has no I/O base",
            Unusable::FchOff => "the FCH's SMBus controller is off or out of reach",
        })
    }
}

/// Which controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Intel's, of this device ID.
    Intel(u16),
    /// The FCH's, of this revision.
    Fch(u8),
}

/// The FCH's power management registers.
enum Pm {
    Ports,
    Mmio(Mmio),
}

/// An SMBus host controller.
pub struct Smbus<P> {
    platform: P,
    base: u16,
    kind: Kind,
    /// The FCH's power management registers.
    pm: Option<Pm>,
}

/// The configuration space of `device`.`function` on bus 0, if the
/// function is there.
fn function(bus: &Mmio, (device, function): (u8, u8)) -> Option<Mmio> {
    let address = Address { segment: 0, bus: 0, device, function };
    let config = bus.window(address.offset_in_bus(), pci::CONFIG_SIZE)?;
    (config.read16(0x00) != 0xffff).then_some(config)
}

/// Finds the SMBus controller in `bus`, the configuration space of bus 0
/// of segment 0; calls `unusable` with one there that is not used.
pub fn find<P: Platform>(
    bus: &Mmio,
    platform: P,
    mut unusable: impl FnMut(Unusable),
) -> Option<Smbus<P>> {
    let found = find_intel(bus).or_else(|| find_fch(bus, &platform))?;
    match found {
        Ok((base, kind, pm)) => Some(Smbus { platform, base, kind, pm }),
        Err(why) => {
            unusable(why);
            None
        }
    }
}

type Search = Result<(u16, Kind, Option<Pm>), Unusable>;

/// Intel's SMBus function, if the bus has one: its base turned on.
fn find_intel(bus: &Mmio) -> Option<Search> {
    INTEL_SMBUS.iter().find_map(|&at| {
        let config = function(bus, at)?;
        let class = (config.read8(0x0b), config.read8(0x0a));
        if config.read16(0x00) != INTEL || class != SMBUS_CLASS {
            return None;
        }
        let bar = config.read32(INTEL_BAR);
        let base = (bar & 0xffe0) as u16;
        if bar & 1 == 0 || base == 0 {
            return Some(Err(Unusable::NoBase));
        }
        config.write16(0x04, config.read16(0x04) | pci::command::IO);
        let hostc = config.read8(HOSTC);
        if hostc & HST_EN == 0 {
            config.write8(HOSTC, hostc | HST_EN);
        }
        Some(Ok((base, Kind::Intel(config.read16(0x02)), None)))
    })
}

/// The FCH's SMBus controller, if the bus has an FCH from Zen's kind on.
fn find_fch<P: Platform>(bus: &Mmio, platform: &P) -> Option<Search> {
    let config = function(bus, FCH_SMBUS)?;
    let (device, revision) = (config.read16(0x02), config.read8(0x08));
    if config.read16(0x00) != AMD || device != FCH_KERNCZ.0 || revision < FCH_KERNCZ.1 {
        return None;
    }
    let pm = if revision >= FCH_ZEN_REVISION {
        match platform.map(PM_MMIO, 8) {
            Some(regs) => Pm::Mmio(regs),
            None => return Some(Err(Unusable::FchOff)),
        }
    } else {
        Pm::Ports
    };
    let enabled = pm_read(platform, &pm, PM_DECODE_EN0) & SMBUS_EN != 0;
    let base = u16::from(pm_read(platform, &pm, PM_DECODE_EN1)) << 8;
    if !enabled || base == 0 {
        return Some(Err(Unusable::FchOff));
    }
    Some(Ok((base, Kind::Fch(revision), Some(pm))))
}

fn pm_read<P: Platform>(platform: &P, pm: &Pm, index: u8) -> u8 {
    match pm {
        Pm::Ports => {
            platform.out8(PM_INDEX, index);
            platform.in8(PM_DATA)
        }
        Pm::Mmio(regs) => regs.read8(usize::from(index)),
    }
}

fn pm_write<P: Platform>(platform: &P, pm: &Pm, index: u8, value: u8) {
    match pm {
        Pm::Ports => {
            platform.out8(PM_INDEX, index);
            platform.out8(PM_DATA, value);
        }
        Pm::Mmio(regs) => regs.write8(usize::from(index), value),
    }
}

impl<P: Platform> Smbus<P> {
    /// Its I/O base.
    pub fn base(&self) -> u16 {
        self.base
    }

    /// Which it is.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The I/O ports its registers take: Intel's BAR 4 is 32 bytes, the
    /// FCH's registers to its semaphore 16.
    pub fn ports(&self) -> core::ops::Range<u16> {
        let len = match self.kind {
            Kind::Intel(_) => 0x20,
            Kind::Fch(_) => 0x10,
        };
        self.base..self.base.saturating_add(len)
    }

    /// Its name, as Linux names its adapter: `SMBus I801 adapter at 0700`
    /// or `SMBus PIIX4 adapter port 0 at 0b00`.
    pub fn adapter_name(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        match self.kind {
            Kind::Intel(_) => write!(out, "SMBus I801 adapter at {:04x}", self.base),
            Kind::Fch(_) => write!(out, "SMBus PIIX4 adapter port 0 at {:04x}", self.base),
        }
    }

    /// Reads the byte at `command` from the device at `address` (7 bits).
    ///
    /// It takes `&mut self` as the controller does one transaction at a
    /// time: the kernel keeps it to one caller.
    pub fn read_byte_data(&mut self, address: u8, command: u8) -> Result<u8, Error> {
        self.transfer(address, command, BYTE_DATA, None).map(|(low, _)| low)
    }

    /// Reads the word at `command`, low byte first.
    pub fn read_word_data(&mut self, address: u8, command: u8) -> Result<u16, Error> {
        self.transfer(address, command, WORD_DATA, None)
            .map(|(low, high)| u16::from_le_bytes([low, high]))
    }

    /// Writes `value` at `command` of the device at `address`.
    pub fn write_byte_data(&mut self, address: u8, command: u8, value: u8) -> Result<(), Error> {
        self.transfer(address, command, BYTE_DATA, Some(value)).map(|_| ())
    }

    fn reg(&self, offset: u16) -> u16 {
        self.base + offset
    }

    fn read(&self, offset: u16) -> u8 {
        self.platform.in8(self.reg(offset))
    }

    fn write(&self, offset: u16, value: u8) {
        self.platform.out8(self.reg(offset), value);
    }

    /// One transaction of `protocol`: a read unless `value` is given.
    fn transfer(
        &mut self,
        address: u8,
        command: u8,
        protocol: u8,
        value: Option<u8>,
    ) -> Result<(u8, u8), Error> {
        match &self.pm {
            Some(pm) => {
                let semaphore = self.take_semaphore()?;
                let port = pm_read(&self.platform, pm, PM_DECODE_EN2);
                if port & PORT_MASK != 0 {
                    pm_write(&self.platform, pm, PM_DECODE_EN2, port & !PORT_MASK);
                }
                let result = self.transaction(address, command, protocol, value);
                if port & PORT_MASK != 0 {
                    pm_write(&self.platform, pm, PM_DECODE_EN2, port);
                }
                self.write(FCH_SEMAPHORE, semaphore | SEMAPHORE_GIVE);
                result
            }
            None => {
                let result = self.transaction(address, command, protocol, value);
                // Firmware may use the controller from now on.
                self.write(HOST_STATUS, INUSE | DONE_FLAGS);
                result
            }
        }
    }

    /// Takes the FCH's host semaphore from its microcontroller; returns the
    /// register as it was.
    fn take_semaphore(&self) -> Result<u8, Error> {
        let semaphore = self.read(FCH_SEMAPHORE);
        for _ in 0..SEMAPHORE_TRIES {
            self.write(FCH_SEMAPHORE, semaphore | SEMAPHORE_TAKE);
            if self.read(FCH_SEMAPHORE) & SEMAPHORE_TAKE != 0 {
                return Ok(semaphore);
            }
            self.platform.delay_us(1000);
        }
        Err(Error::Busy)
    }

    fn transaction(
        &self,
        address: u8,
        command: u8,
        protocol: u8,
        value: Option<u8>,
    ) -> Result<(u8, u8), Error> {
        let status = self.read(HOST_STATUS);
        if status & HOST_BUSY != 0 {
            return Err(Error::Busy);
        }
        if status & DONE_FLAGS != 0 {
            self.write(HOST_STATUS, status & DONE_FLAGS);
        }
        let read = u8::from(value.is_none());
        self.write(TARGET_ADDRESS, (address & 0x7f) << 1 | read);
        self.write(HOST_COMMAND, command);
        if let Some(value) = value {
            self.write(DATA0, value);
        }
        self.write(HOST_CONTROL, protocol | START);
        let mut done = None;
        for _ in 0..POLLS {
            self.platform.delay_us(POLL_US);
            let status = self.read(HOST_STATUS);
            if status & HOST_BUSY == 0 && status & (INTR | DEV_ERR | BUS_ERR | FAILED) != 0 {
                done = Some(status);
                break;
            }
        }
        let Some(status) = done else {
            if matches!(self.kind, Kind::Intel(_)) {
                self.write(HOST_CONTROL, KILL);
                self.platform.delay_us(POLL_US);
                self.write(HOST_CONTROL, 0);
            }
            self.write(HOST_STATUS, DONE_FLAGS);
            return Err(Error::Timeout);
        };
        self.write(HOST_STATUS, status & DONE_FLAGS);
        if status & FAILED != 0 {
            Err(Error::Failed)
        } else if status & BUS_ERR != 0 {
            Err(Error::Collision)
        } else if status & DEV_ERR != 0 {
            Err(Error::NoDevice)
        } else {
            Ok((self.read(DATA0), self.read(DATA1)))
        }
    }
}
