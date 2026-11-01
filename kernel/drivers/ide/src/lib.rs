// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! PCI IDE for the Radix kernel (Phase 11g, ADR 0244): the ATA disks on an
//! IDE controller's two channels, and the SATA disks of a chipset whose
//! firmware sets its SATA controller to IDE mode, which then shows them
//! the same way.
//!
//! A channel has two devices, the master (0) and the slave (1), which
//! share its registers: a command block of eight ports and a control
//! port, in I/O space, at the legacy addresses (0x1f0 and 0x3f6, 0x170
//! and 0x376) or in the function's BARs 0 to 3 in native mode. Commands
//! go one at a time: the driver picks a device, writes the command's
//! address and count, and the command. Data moves by bus-master DMA (the
//! *Programming Interface for Bus Master IDE Controller*, revision 1.0):
//! eight more ports for each channel in BAR 4 take the address of a table
//! of physical region descriptors (PRDs), each a buffer below 4 GiB of at
//! most 64 KiB that does not cross a 64 KiB line, and a start bit.
//!
//! The commands are ATA's (ACS-4): IDENTIFY DEVICE, in PIO, then READ DMA
//! EXT, WRITE DMA EXT and FLUSH CACHE EXT, with 48-bit addresses; what
//! IDENTIFY says is read by `radix_ahci::ata`, which AHCI's disks share.
//! Packet (ATAPI) devices, optical drives, are found and left alone.
//!
//! The channel's interrupt is off (nIEN): the kernel routes neither the
//! legacy IRQs 14 and 15 nor INTx, so a command is done when the device
//! is no longer busy and, for DMA, the bus master has stopped. The crate
//! is a compartment with no `unsafe` code; its tests run it against a
//! simulated channel and disks.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use radix_ahci::ata::{self, Identity, Unsupported, command};
use radix_hw::{Dma, IoPorts, device_barrier};

/// The PCI class and subclass of an IDE controller; its programming
/// interface says how each channel is reached ([`interface`]).
pub const PCI_CLASS: (u8, u8) = (0x01, 0x01);
/// The BAR of the bus-master registers, in I/O space.
pub const BUS_MASTER_BAR: usize = 4;
/// The legacy ports of the channels' command blocks and control ports,
/// for those not in native mode.
pub const LEGACY_PORTS: [(u16, u16); 2] = [(0x1f0, 0x3f6), (0x170, 0x376)];
/// The ports of a command block.
pub const COMMAND_PORTS: u16 = 8;
/// The ports of a channel's bus-master registers.
pub const BUS_MASTER_PORTS: u16 = 8;
/// The most PRDs a command takes.
pub const MAX_PRDS: usize = 256;

/// The bits of the programming interface.
pub mod interface {
    /// The primary channel is in native mode: its ports are BARs 0 and 1.
    pub const PRIMARY_NATIVE: u8 = 1 << 0;
    /// The secondary channel is in native mode: BARs 2 and 3.
    pub const SECONDARY_NATIVE: u8 = 1 << 2;
    /// The controller has bus-master DMA.
    pub const BUS_MASTER: u8 = 1 << 7;
}

/// The command block's registers.
mod reg {
    pub const DATA: u16 = 0;
    pub const ERROR: u16 = 1;
    pub const COUNT: u16 = 2;
    pub const LBA0: u16 = 3;
    pub const LBA1: u16 = 4;
    pub const LBA2: u16 = 5;
    pub const DEVICE: u16 = 6;
    pub const STATUS: u16 = 7;
    pub const COMMAND: u16 = 7;
    /// The control port: the alternate status, read without clearing
    /// the interrupt; written, the device control register.
    pub const ALT_STATUS: u16 = 0;
    pub const DEVICE_CONTROL: u16 = 0;
    /// Device control: interrupts off, a software reset.
    pub const NIEN: u8 = 1 << 1;
    pub const SRST: u8 = 1 << 2;
    /// Device register: the slave.
    pub const SLAVE: u8 = 1 << 4;
    /// Status: a fault.
    pub const DF: u8 = 1 << 5;
}

/// A channel's bus-master registers.
mod bm {
    pub const COMMAND: u16 = 0;
    pub const STATUS: u16 = 2;
    pub const PRD: u16 = 4;
    /// Command: start, and the direction (set: into memory).
    pub const START: u8 = 1 << 0;
    pub const INTO_MEMORY: u8 = 1 << 3;
    /// Status: moving, an error, the device's interrupt, and the drives
    /// the firmware set up for DMA.
    pub const ACTIVE: u8 = 1 << 0;
    pub const ERROR: u8 = 1 << 1;
    pub const INTERRUPT: u8 = 1 << 2;
    pub const DMA_CAPABLE: u8 = 3 << 5;
}

/// A PRD's last-entry bit.
const PRD_LAST: u32 = 1 << 31;
/// The signatures a packet device leaves in LBA mid and high, parallel
/// and Serial ATA.
const ATAPI: [(u8, u8); 2] = [(0x14, 0xeb), (0x69, 0x96)];
/// How long a reset or IDENTIFY may take, and how often the driver
/// looks meanwhile, in microseconds.
const RESET_US: u32 = 2_000_000;
const IDENTIFY_US: u32 = 3_000_000;
const POLL_US: u32 = 100;

/// Whether IDENTIFY DEVICE's `words` are a Serial ATA device's: its
/// Serial ATA capabilities (word 76) are neither 0 nor all ones, or its
/// transport (word 222, bits 15 to 12) is Serial (ACS-4, 7.13.6.37 and
/// 7.13.6.53).
fn serial(words: &[u16; 256]) -> bool {
    let capabilities = words[76];
    (capabilities != 0 && capabilities != 0xffff) || words[222] >> 12 == 1
}

/// What is on one end of a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    /// Nothing answers.
    Absent,
    /// An ATA disk, as it says it is.
    Ata {
        /// What it says of itself.
        identity: Identity,
        /// Whether it is a Serial ATA disk, which its words 76 (Serial
        /// ATA capabilities) or 222 (its transport) say: then the
        /// controller is a SATA controller set to IDE mode.
        serial: bool,
    },
    /// A packet device (an optical drive), which is left alone.
    Atapi,
    /// An ATA device the driver does not drive.
    Unsupported(Unsupported),
}

/// What a command does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Reads sectors into memory.
    Read,
    /// Writes sectors from memory.
    Write,
    /// Has the disk write its cache out.
    Flush,
}

/// How a command could not be done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The request does not fit: too many sectors or buffers, or a
    /// buffer that does not make whole sectors.
    BadRequest,
    /// Another command is in flight on the channel.
    Busy,
    /// The device said it failed.
    Device {
        /// Its status register.
        status: u8,
        /// Its error register.
        error: u8,
    },
    /// The bus master could not reach memory.
    Bus,
}

/// The command in flight on a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Flight {
    drive: u8,
    dma: bool,
}

/// An IDE channel and its two devices, reached through its command block,
/// its control port and its bus-master registers.
pub struct Channel<P: IoPorts> {
    command: P,
    control: P,
    bus_master: P,
    /// The PRD table: a page below 4 GiB.
    prds: Dma,
    flight: Option<Flight>,
    selected: Option<u8>,
    /// Each device's logical sector, in bytes, as IDENTIFY said.
    sector: [u32; 2],
}

impl<P: IoPorts> Channel<P> {
    /// The channel of these ports, with its PRD table from `alloc` (a
    /// page below 4 GiB); `None` without memory or with too few ports.
    pub fn new(
        command: P,
        control: P,
        bus_master: P,
        alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>,
    ) -> Option<Channel<P>> {
        if command.len() < COMMAND_PORTS
            || control.is_empty()
            || bus_master.len() < BUS_MASTER_PORTS
        {
            return None;
        }
        let prds = alloc(1, true)?;
        Some(Channel {
            command,
            control,
            bus_master,
            prds,
            flight: None,
            selected: None,
            sector: [512; 2],
        })
    }

    /// Whether anything answers on the channel: a floating bus reads all
    /// ones.
    pub fn present(&self) -> bool {
        self.control.read8(reg::ALT_STATUS) != 0xff
    }

    /// Resets both devices (a software reset), with interrupts off, and
    /// waits for the master to be ready; whether it was in time. Whatever
    /// was in flight is lost.
    pub fn reset(&mut self, delay: &mut dyn FnMut(u32)) -> bool {
        self.stop_dma();
        self.flight = None;
        self.selected = None;
        self.control.write8(reg::DEVICE_CONTROL, reg::NIEN | reg::SRST);
        delay(5);
        self.control.write8(reg::DEVICE_CONTROL, reg::NIEN);
        delay(2000);
        self.wait_not_busy(RESET_US, delay)
    }

    fn status(&self) -> u8 {
        self.control.read8(reg::ALT_STATUS)
    }

    fn wait_not_busy(&self, limit_us: u32, delay: &mut dyn FnMut(u32)) -> bool {
        let mut waited = 0;
        loop {
            let status = self.status();
            if status & ata::STATUS_BSY == 0 || status == 0xff {
                return status != 0xff;
            }
            if waited >= limit_us {
                return false;
            }
            delay(POLL_US);
            waited += POLL_US;
        }
    }

    /// Picks `drive`, and gives it 400 ns to answer.
    fn select(&mut self, drive: u8, lba: bool) {
        let mut device = if drive == 1 { reg::SLAVE } else { 0 } | 0xa0;
        if lba {
            device |= ata::DEVICE_LBA;
        }
        self.command.write8(reg::DEVICE, device);
        if self.selected != Some(drive) {
            for _ in 0..5 {
                self.status();
            }
            self.selected = Some(drive);
        }
    }

    /// Finds what `drive` is: IDENTIFY DEVICE, by PIO.
    pub fn identify(&mut self, drive: u8, delay: &mut dyn FnMut(u32)) -> Device {
        if self.flight.is_some() || drive > 1 {
            return Device::Absent;
        }
        self.select(drive, false);
        if self.status() == 0xff || !self.wait_not_busy(RESET_US, delay) {
            return Device::Absent;
        }
        let signature = (self.command.read8(reg::LBA1), self.command.read8(reg::LBA2));
        if ATAPI.contains(&signature) {
            return Device::Atapi;
        }
        for register in [reg::COUNT, reg::LBA0, reg::LBA1, reg::LBA2] {
            self.command.write8(register, 0);
        }
        self.command.write8(reg::COMMAND, command::IDENTIFY_DEVICE);
        if self.status() == 0 {
            return Device::Absent;
        }
        let mut waited = 0;
        loop {
            let status = self.status();
            if status & ata::STATUS_BSY == 0 {
                if status & ata::STATUS_ERR != 0 {
                    let signature = (self.command.read8(reg::LBA1), self.command.read8(reg::LBA2));
                    return if ATAPI.contains(&signature) { Device::Atapi } else { Device::Absent };
                }
                if status & ata::STATUS_DRQ != 0 {
                    break;
                }
                if status == 0 {
                    return Device::Absent;
                }
            }
            if waited >= IDENTIFY_US {
                return Device::Absent;
            }
            delay(POLL_US);
            waited += POLL_US;
        }
        let mut words = [0u16; 256];
        for word in &mut words {
            *word = self.command.read16(reg::DATA);
        }
        // Reading the status ends the command.
        self.command.read8(reg::STATUS);
        match ata::identify(&words) {
            Ok(identity) => {
                self.sector[usize::from(drive)] = identity.logical_sector;
                Device::Ata { identity, serial: serial(&words) }
            }
            Err(why) => Device::Unsupported(why),
        }
    }

    /// Whether a command is in flight.
    pub fn busy(&self) -> bool {
        self.flight.is_some()
    }

    /// Starts `op` on `drive`: for a read or write, of `count` of its
    /// logical sectors from `lba`, into or out of `buffers` (physical
    /// address below 4 GiB, and length), which hold exactly them.
    pub fn start(
        &mut self,
        drive: u8,
        op: Op,
        lba: u64,
        count: u32,
        buffers: &[(u32, u32)],
    ) -> Result<(), Error> {
        if self.flight.is_some() {
            return Err(Error::Busy);
        }
        if drive > 1 {
            return Err(Error::BadRequest);
        }
        let dma = op != Op::Flush;
        if dma {
            if count == 0 || count > 65_536 || lba >= 1 << 48 {
                return Err(Error::BadRequest);
            }
            let bytes = u64::from(count) * u64::from(self.sector[usize::from(drive)]);
            if self.write_prds(buffers)? != bytes {
                return Err(Error::BadRequest);
            }
        }
        self.select(drive, true);
        if self.status() & (ata::STATUS_BSY | ata::STATUS_DRQ) != 0 {
            return Err(Error::Busy);
        }
        let code = match op {
            Op::Read => command::READ_DMA_EXT,
            Op::Write => command::WRITE_DMA_EXT,
            Op::Flush => command::FLUSH_CACHE_EXT,
        };
        if dma {
            self.bus_master.write32(bm::PRD, self.prds.phys() as u32);
            let status = self.bus_master.read8(bm::STATUS) & bm::DMA_CAPABLE;
            self.bus_master.write8(bm::STATUS, status | bm::ERROR | bm::INTERRUPT);
            let direction = if op == Op::Read { bm::INTO_MEMORY } else { 0 };
            self.bus_master.write8(bm::COMMAND, direction);
            // A count of 65,536 is written as 0; the high bytes go first.
            let [count_low, count_high] = ((count & 0xffff) as u16).to_le_bytes();
            let lba = lba.to_le_bytes();
            for (register, value) in [
                (reg::COUNT, count_high),
                (reg::LBA0, lba[3]),
                (reg::LBA1, lba[4]),
                (reg::LBA2, lba[5]),
                (reg::COUNT, count_low),
                (reg::LBA0, lba[0]),
                (reg::LBA1, lba[1]),
                (reg::LBA2, lba[2]),
            ] {
                self.command.write8(register, value);
            }
        }
        device_barrier();
        self.command.write8(reg::COMMAND, code);
        if dma {
            let direction = if op == Op::Read { bm::INTO_MEMORY } else { 0 };
            self.bus_master.write8(bm::COMMAND, direction | bm::START);
        }
        self.flight = Some(Flight { drive, dma });
        Ok(())
    }

    /// Fills the PRD table with `buffers`, which hold the command's
    /// sectors; how many bytes they hold.
    fn write_prds(&mut self, buffers: &[(u32, u32)]) -> Result<u64, Error> {
        let mut total = 0u64;
        let mut entries = 0;
        for &(start, len) in buffers {
            let (mut at, mut left) = (u64::from(start), u64::from(len));
            if at + left > 1 << 32 {
                return Err(Error::BadRequest);
            }
            while left > 0 {
                // Not across a 64 KiB line.
                let room = 0x1_0000 - (at & 0xffff);
                let part = left.min(room);
                if entries == MAX_PRDS {
                    return Err(Error::BadRequest);
                }
                let size = (part & 0xffff) as u32;
                self.prds.write32(8 * entries, at as u32);
                self.prds.write32(8 * entries + 4, size);
                entries += 1;
                at += part;
                left -= part;
                total += part;
            }
        }
        if entries == 0 {
            return Err(Error::BadRequest);
        }
        let last = 8 * (entries - 1) + 4;
        self.prds.write32(last, self.prds.read32(last) | PRD_LAST);
        Ok(total)
    }

    /// The command in flight, if it is done: how it ended. `None` while
    /// it runs, or when none is.
    pub fn poll(&mut self) -> Option<Result<(), Error>> {
        let flight = self.flight?;
        let status = self.status();
        if status & ata::STATUS_BSY != 0 {
            return None;
        }
        let bus = if flight.dma { self.bus_master.read8(bm::STATUS) } else { 0 };
        let failed = status & (ata::STATUS_ERR | reg::DF) != 0 || bus & bm::ERROR != 0;
        if flight.dma && bus & bm::ACTIVE != 0 && !failed {
            return None;
        }
        device_barrier();
        self.flight = None;
        // Reading the status ends the command.
        let status = self.command.read8(reg::STATUS);
        let error = self.command.read8(reg::ERROR);
        if flight.dma {
            self.stop_dma();
        }
        if bus & bm::ERROR != 0 {
            return Some(Err(Error::Bus));
        }
        if status & (ata::STATUS_ERR | reg::DF) != 0 {
            return Some(Err(Error::Device { status, error }));
        }
        Some(Ok(()))
    }

    /// The drive of the command in flight.
    pub fn in_flight(&self) -> Option<u8> {
        self.flight.map(|flight| flight.drive)
    }

    fn stop_dma(&self) {
        let command = self.bus_master.read8(bm::COMMAND);
        self.bus_master.write8(bm::COMMAND, command & !bm::START);
        let status = self.bus_master.read8(bm::STATUS) & bm::DMA_CAPABLE;
        self.bus_master.write8(bm::STATUS, status | bm::ERROR | bm::INTERRUPT);
    }
}
