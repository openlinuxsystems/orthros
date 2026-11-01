// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! SD card readers for the Radix kernel (Phase 11g, ADR 0247): SD host
//! controllers as the *SD Specifications Part A2, SD Host Controller
//! Simplified Specification* (version 4.20) describes them, and the SD
//! memory cards in them, as the *Part 1, Physical Layer Simplified
//! Specification* (version 9.10) does. Laptops' readers that are on PCI
//! (class 08h, subclass 05h) are such controllers, as are the eMMC and SD
//! controllers of many chipsets; QEMU's `sdhci-pci` is one.
//!
//! A controller has one to six slots, each with its own 256 bytes of
//! registers in a BAR of its own ([`Slot`]). [`Slot::new`] resets one and
//! reads what it can do; it needs ADMA2, the controller's scatter-gather
//! DMA, which every controller since version 2.00 has, and uses its 64-bit
//! descriptors where the controller has them (version 3.00 and later).
//! [`Slot::identify`] powers the card up and sets it up as section 4.2 of
//! the physical layer says: CMD0, CMD8 (which only version 2.00 cards
//! answer), ACMD41 until the card is ready (SDHC and SDXC cards say they
//! are addressed in blocks), CMD2 for its CID, CMD3 for its address, CMD9
//! for its CSD and so its capacity ([`card`]), CMD7 to select it, then the
//! default speed's 25 MHz and the four-bit bus every SD memory card has
//! (ACMD6). [`Slot::start`] then starts a read or a write of whole
//! 512-byte blocks to or from a list of buffers (CMD17, CMD18, CMD24 and
//! CMD25, the controller sending CMD12 after the multiple ones), and
//! [`Slot::poll`] says when it is done: the controller's interrupt is
//! left off, and the kernel polls it as it does an IDE channel.
//!
//! Cards inserted later, UHS-I speeds and eMMC are left for later; so are
//! cards of version 1 that are MMC cards, which do not answer ACMD41.
//! The crate is a compartment with no `unsafe` code; its tests run it
//! against a simulated controller and card.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod card;
#[cfg(test)]
mod tests;

use radix_hw::{Dma, Mmio, device_barrier};

pub use card::{Card, Cid, Csd};

/// The PCI class and subclass of an SD host controller.
pub const PCI_CLASS: (u8, u8) = (0x08, 0x05);
/// The PCI configuration register that says where the slots are: bits 0
/// to 2 the first slot's BAR, 4 to 6 the slots less one.
pub const SLOT_INFORMATION: u16 = 0x40;
/// The bytes of a slot's registers.
pub const REGISTERS: usize = 0x100;
/// The bytes of a block.
pub const BLOCK: u32 = 512;
/// The most buffers a request takes: the descriptors in a page.
pub const MAX_BUFFERS: usize = 128;
/// The longest buffer a descriptor takes.
pub const MAX_BUFFER: u32 = 65536 - 4096;

/// The registers of a slot (2.1).
pub mod reg {
    /// 32-bit: the argument of the command.
    pub const ARGUMENT: usize = 0x08;
    /// 16-bit: the bytes of a block.
    pub const BLOCK_SIZE: usize = 0x04;
    /// 16-bit: the blocks of a transfer.
    pub const BLOCK_COUNT: usize = 0x06;
    /// 16-bit: how the command's data moves.
    pub const TRANSFER_MODE: usize = 0x0c;
    /// 16-bit: the command; writing it sends it.
    pub const COMMAND: usize = 0x0e;
    /// 4 × 32-bit: the card's response.
    pub const RESPONSE: usize = 0x10;
    /// 32-bit: what is going on.
    pub const PRESENT_STATE: usize = 0x24;
    /// 8-bit: the bus's width and the DMA used.
    pub const HOST_CONTROL: usize = 0x28;
    /// 8-bit: the card's power and voltage.
    pub const POWER_CONTROL: usize = 0x29;
    /// 16-bit: the card's clock.
    pub const CLOCK_CONTROL: usize = 0x2c;
    /// 8-bit: how long data may take.
    pub const TIMEOUT_CONTROL: usize = 0x2e;
    /// 8-bit: resets.
    pub const SOFTWARE_RESET: usize = 0x2f;
    /// 16-bit: what happened, cleared by writing 1s.
    pub const NORMAL_STATUS: usize = 0x30;
    /// 16-bit: the errors that happened.
    pub const ERROR_STATUS: usize = 0x32;
    /// 16-bit each: which of those are reported, and which interrupt.
    pub const NORMAL_STATUS_ENABLE: usize = 0x34;
    /// The errors reported.
    pub const ERROR_STATUS_ENABLE: usize = 0x36;
    /// The events that interrupt.
    pub const NORMAL_SIGNAL_ENABLE: usize = 0x38;
    /// The errors that interrupt.
    pub const ERROR_SIGNAL_ENABLE: usize = 0x3a;
    /// 64-bit: what the controller can do.
    pub const CAPABILITIES: usize = 0x40;
    /// 64-bit: the ADMA descriptors' address.
    pub const ADMA_ADDRESS: usize = 0x58;
    /// 16-bit: the specification's version the controller follows.
    pub const VERSION: usize = 0xfe;
}

/// Present State's bits.
mod present {
    pub const COMMAND_INHIBIT: u32 = 1 << 0;
    pub const DATA_INHIBIT: u32 = 1 << 1;
    pub const CARD_INSERTED: u32 = 1 << 16;
    pub const WRITABLE: u32 = 1 << 19;
}

/// Normal Interrupt Status's bits.
mod status {
    pub const COMMAND_COMPLETE: u16 = 1 << 0;
    pub const TRANSFER_COMPLETE: u16 = 1 << 1;
    pub const ERROR: u16 = 1 << 15;
    /// Error Interrupt Status: the command timed out.
    pub const COMMAND_TIMEOUT: u16 = 1 << 0;
}

/// Capabilities' bits.
mod caps {
    pub const ADMA2: u64 = 1 << 19;
    pub const VOLTS_3_3: u64 = 1 << 24;
    pub const VOLTS_3_0: u64 = 1 << 25;
    pub const VOLTS_1_8: u64 = 1 << 26;
    pub const SYSTEM_64: u64 = 1 << 28;
}

/// The specification's versions, as the Version register says them.
pub mod version {
    /// 2.00.
    pub const V2: u8 = 1;
    /// 3.00.
    pub const V3: u8 = 2;
}

/// The card status bits of an R1 response that say a command failed.
const CARD_ERRORS: u32 = 0xfdf8_0000;
/// The voltages ACMD41 offers: 2.7 to 3.6 V.
const VOLTAGES: u32 = 0x00ff_8000;

/// How a slot or its card failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No card is in the slot.
    NoCard,
    /// The controller did not finish in time.
    Timeout,
    /// The command failed: the Error Interrupt Status.
    Command(u16),
    /// The card said its command failed: its status.
    Card(u32),
    /// The controller or the card is of a kind the driver does not drive.
    Unsupported(&'static str),
    /// A request the driver cannot make.
    BadRequest,
}

impl Error {
    /// Why, in words.
    pub fn as_str(&self) -> &'static str {
        match self {
            Error::NoCard => "no card is in the slot",
            Error::Timeout => "the controller did not finish in time",
            Error::Command(bits) if bits & status::COMMAND_TIMEOUT != 0 => {
                "the card did not answer"
            }
            Error::Command(bits) if bits & (1 << 9) != 0 => "the controller's DMA failed",
            Error::Command(bits) if bits & (1 << 4) != 0 => "the card's data did not come in time",
            Error::Command(bits) if bits & (1 << 5) != 0 => "the card's data was garbled (CRC)",
            Error::Command(_) => "the command failed",
            Error::Card(bits) if bits & (1 << 26) != 0 => "the card is write-protected",
            Error::Card(bits) if bits & (1 << 31) != 0 => "beyond the card's end",
            Error::Card(_) => "the card refused the command",
            Error::Unsupported(why) => why,
            Error::BadRequest => "a request the driver cannot make",
        }
    }
}

/// Which way a transfer goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// From the card.
    Read,
    /// To the card.
    Write,
}

/// The responses (Physical Layer, 4.9), as the Command register's flags
/// ask for them: their length, and whether the CRC and index are checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Response {
    None,
    /// R1, the card's status, and R6 and R7, laid out as it is.
    R1,
    /// R1 with the card busy after it.
    R1b,
    /// R2: the CID or the CSD.
    R2,
    /// R3: the OCR, unchecked.
    R3,
}

impl Response {
    fn flags(self) -> u16 {
        match self {
            Response::None => 0x00,
            Response::R1 => 0x1a,
            Response::R1b => 0x1b,
            Response::R2 => 0x09,
            Response::R3 => 0x02,
        }
    }
}

/// The Command register's bit that says data goes with the command.
const DATA_PRESENT: u16 = 1 << 5;
/// The Transfer Mode register's bits.
mod mode {
    pub const DMA: u16 = 1 << 0;
    pub const BLOCK_COUNT: u16 = 1 << 1;
    pub const AUTO_CMD12: u16 = 1 << 2;
    pub const READ: u16 = 1 << 4;
    pub const MULTIPLE: u16 = 1 << 5;
}

/// A request waiting for the lines to be free, or in flight.
#[derive(Clone, Copy, Debug)]
enum Flight {
    /// Its command is ready to go once neither line is inhibited.
    Ready { command: u16, mode: u16, argument: u32, count: u16 },
    /// Sent: the card's status once it answered.
    Sent { status: Option<u32> },
}

/// A slot of an SD host controller.
pub struct Slot {
    regs: Mmio,
    version: u8,
    caps: u64,
    /// The ADMA2 descriptors are 64-bit ones (12 bytes).
    adma64: bool,
    /// The descriptor table.
    table: Dma,
    card: Option<Card>,
    flight: Option<Flight>,
    /// A transfer failed: the card is stopped and the lines reset first.
    stuck: bool,
}

/// Waits until `done` or `us` microseconds have passed, `delay`ing 10 at
/// a time; says whether it is done.
fn wait(delay: &mut dyn FnMut(u32), us: u32, mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..us.div_ceil(10) {
        if done() {
            return true;
        }
        delay(10);
    }
    done()
}

impl Slot {
    /// Resets the slot whose registers are `regs` and reads what it can
    /// do; its descriptor table comes from `alloc(pages, below_4g)`.
    pub fn new(
        regs: Mmio,
        alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>,
        delay: &mut dyn FnMut(u32),
    ) -> Result<Slot, Error> {
        if regs.len() < REGISTERS {
            return Err(Error::BadRequest);
        }
        let version = (regs.read16(reg::VERSION) & 0xff) as u8;
        regs.write8(reg::SOFTWARE_RESET, 1);
        if !wait(delay, 100_000, || regs.read8(reg::SOFTWARE_RESET) & 1 == 0) {
            return Err(Error::Timeout);
        }
        let caps = u64::from(regs.read32(reg::CAPABILITIES))
            | u64::from(regs.read32(reg::CAPABILITIES + 4)) << 32;
        if caps & caps::ADMA2 == 0 {
            return Err(Error::Unsupported("it has no ADMA2, the DMA the driver uses"));
        }
        let adma64 = version >= version::V3 && caps & caps::SYSTEM_64 != 0;
        let table = alloc(1, !adma64).ok_or(Error::Unsupported("out of memory"))?;
        // Everything reported, nothing interrupting: the slot is polled.
        regs.write16(reg::NORMAL_STATUS_ENABLE, 0x00ff);
        regs.write16(reg::ERROR_STATUS_ENABLE, 0x03ff);
        regs.write16(reg::NORMAL_SIGNAL_ENABLE, 0);
        regs.write16(reg::ERROR_SIGNAL_ENABLE, 0);
        regs.write8(reg::TIMEOUT_CONTROL, 0x0e);
        Ok(Slot { regs, version, caps, adma64, table, card: None, flight: None, stuck: false })
    }

    /// The version of the specification it follows: (major, minor).
    pub fn version(&self) -> (u8, u8) {
        match self.version {
            0 => (1, 0),
            1 => (2, 0),
            2 => (3, 0),
            3 => (4, 0),
            4 => (4, 10),
            5 => (4, 20),
            _ => (4, 20),
        }
    }

    /// Whether its DMA reaches all of memory, not only the first 4 GiB.
    pub fn reaches_64(&self) -> bool {
        self.adma64
    }

    /// Whether a card is in the slot.
    pub fn present(&self) -> bool {
        self.regs.read32(reg::PRESENT_STATE) & present::CARD_INSERTED != 0
    }

    /// The card set up, if there is one.
    pub fn card(&self) -> Option<&Card> {
        self.card.as_ref()
    }

    /// The base clock, in kHz.
    fn base_khz(&self) -> u32 {
        let mask = if self.version >= version::V3 { 0xff } else { 0x3f };
        ((self.caps >> 8) & mask) as u32 * 1000
    }

    /// Runs the card's clock as near `khz` as the controller can without
    /// going over; returns the speed it runs at.
    fn set_clock(&mut self, khz: u32, delay: &mut dyn FnMut(u32)) -> Result<u32, Error> {
        let base = self.base_khz();
        if base == 0 {
            return Err(Error::Unsupported("it does not say its base clock"));
        }
        self.regs.write16(reg::CLOCK_CONTROL, 0);
        // The clock is base / (2 * N), or base with N = 0: any N up to
        // 1023 from 3.00 on, a power of two up to 128 before.
        let (value, actual) = if base <= khz {
            (0, base)
        } else if self.version >= version::V3 {
            let n = base.div_ceil(2 * khz).min(1023);
            ((n & 0xff) << 8 | (n >> 8 & 3) << 6, base / (2 * n))
        } else {
            let mut n = 1;
            while n < 128 && base / (2 * n) > khz {
                n *= 2;
            }
            (n << 8, base / (2 * n))
        };
        let value = value as u16;
        self.regs.write16(reg::CLOCK_CONTROL, value | 1);
        if !wait(delay, 150_000, || self.regs.read16(reg::CLOCK_CONTROL) & 2 != 0) {
            return Err(Error::Timeout);
        }
        self.regs.write16(reg::CLOCK_CONTROL, value | 1 | 4);
        Ok(actual)
    }

    /// Resets the command line, and the data line with `data`.
    fn reset_lines(&self, data: bool, delay: &mut dyn FnMut(u32)) {
        let bits = if data { 6 } else { 2 };
        self.regs.write8(reg::SOFTWARE_RESET, bits);
        wait(delay, 100_000, || self.regs.read8(reg::SOFTWARE_RESET) & bits == 0);
    }

    /// Sends command `index` with `argument`, without data, and waits for
    /// its `response`.
    fn command(
        &mut self,
        index: u8,
        argument: u32,
        response: Response,
        delay: &mut dyn FnMut(u32),
    ) -> Result<card::Long, Error> {
        let regs = &self.regs;
        let busy = present::COMMAND_INHIBIT
            | if response == Response::R1b { present::DATA_INHIBIT } else { 0 };
        if !wait(delay, 10_000, || regs.read32(reg::PRESENT_STATE) & busy == 0) {
            return Err(Error::Timeout);
        }
        regs.write16(reg::NORMAL_STATUS, 0xffff);
        regs.write16(reg::ERROR_STATUS, 0xffff);
        regs.write32(reg::ARGUMENT, argument);
        regs.write16(reg::TRANSFER_MODE, 0);
        regs.write16(reg::COMMAND, u16::from(index) << 8 | response.flags());
        let done = status::COMMAND_COMPLETE | status::ERROR;
        if !wait(delay, 1_000_000, || regs.read16(reg::NORMAL_STATUS) & done != 0) {
            self.reset_lines(true, delay);
            return Err(Error::Timeout);
        }
        if regs.read16(reg::NORMAL_STATUS) & status::ERROR != 0 {
            let error = regs.read16(reg::ERROR_STATUS);
            regs.write16(reg::ERROR_STATUS, 0xffff);
            regs.write16(reg::NORMAL_STATUS, 0xffff);
            self.reset_lines(true, delay);
            return Err(Error::Command(error));
        }
        regs.write16(reg::NORMAL_STATUS, status::COMMAND_COMPLETE);
        if response == Response::R1b {
            let done = status::TRANSFER_COMPLETE | status::ERROR;
            if !wait(delay, 1_000_000, || regs.read16(reg::NORMAL_STATUS) & done != 0) {
                return Err(Error::Timeout);
            }
            regs.write16(reg::NORMAL_STATUS, 0xffff);
        }
        let long = core::array::from_fn(|i| regs.read32(reg::RESPONSE + 4 * i));
        if matches!(response, Response::R1 | Response::R1b)
            && long[0] & CARD_ERRORS != 0
            && !matches!(index, 3 | 8)
        {
            return Err(Error::Card(long[0]));
        }
        Ok(long)
    }

    /// An application command (ACMD): CMD55 to the card at `rca`, then
    /// `index`.
    fn app_command(
        &mut self,
        rca: u16,
        index: u8,
        argument: u32,
        response: Response,
        delay: &mut dyn FnMut(u32),
    ) -> Result<card::Long, Error> {
        self.command(55, u32::from(rca) << 16, Response::R1, delay)?;
        self.command(index, argument, response, delay)
    }

    /// Powers the card up and sets it up (Physical Layer, 4.2), ready for
    /// [`Slot::start`].
    pub fn identify(&mut self, delay: &mut dyn FnMut(u32)) -> Result<Card, Error> {
        if !self.present() {
            return Err(Error::NoCard);
        }
        self.card = None;
        // The highest voltage it gives: 3.3, 3.0 or 1.8 V.
        let volts = if self.caps & caps::VOLTS_3_3 != 0 {
            0b111
        } else if self.caps & caps::VOLTS_3_0 != 0 {
            0b110
        } else if self.caps & caps::VOLTS_1_8 != 0 {
            0b101
        } else {
            return Err(Error::Unsupported("it says it gives no voltage"));
        };
        self.regs.write8(reg::POWER_CONTROL, volts << 1);
        self.regs.write8(reg::POWER_CONTROL, volts << 1 | 1);
        delay(10_000);
        self.regs.write8(reg::HOST_CONTROL, 0);
        self.set_clock(400, delay)?;
        // 74 clocks before the first command.
        delay(1_000);
        self.command(0, 0, Response::None, delay)?;
        // Only cards of version 2.00 and later answer CMD8, with the
        // pattern sent back.
        let version2 = match self.command(8, 0x1aa, Response::R1, delay) {
            Ok(long) if long[0] & 0xfff == 0x1aa => true,
            Ok(_) => return Err(Error::Unsupported("the card does not take the slot's voltage")),
            Err(Error::Command(_) | Error::Timeout) => false,
            Err(err) => return Err(err),
        };
        let asked = VOLTAGES | if version2 { 1 << 30 } else { 0 };
        let mut ocr = 0;
        for _ in 0..100 {
            ocr = match self.app_command(0, 41, asked, Response::R3, delay) {
                Ok(long) => long[0],
                Err(Error::Command(_) | Error::Timeout) => {
                    return Err(Error::Unsupported("it is an MMC card, which is not driven yet"));
                }
                Err(err) => return Err(err),
            };
            if ocr & 1 << 31 != 0 {
                break;
            }
            delay(10_000);
        }
        if ocr & 1 << 31 == 0 {
            return Err(Error::Timeout);
        }
        let high_capacity = version2 && ocr & 1 << 30 != 0;
        let cid = Cid::parse(&self.command(2, 0, Response::R2, delay)?);
        let rca = (self.command(3, 0, Response::R1, delay)?[0] >> 16) as u16;
        let csd = self.command(9, u32::from(rca) << 16, Response::R2, delay)?;
        let blocks = match Csd::parse(&csd) {
            Csd::Standard(blocks) | Csd::High(blocks) => blocks,
            Csd::Ultra => return Err(Error::Unsupported("it is an SDUC card, not driven yet")),
            Csd::Unknown => return Err(Error::Unsupported("its CSD is of an unknown version")),
        };
        self.command(7, u32::from(rca) << 16, Response::R1b, delay)?;
        let khz = self.set_clock(25_000, delay)?;
        // Every SD memory card has a four-bit bus.
        self.app_command(rca, 6, 2, Response::R1, delay)?;
        self.regs.write8(reg::HOST_CONTROL, 1 << 1);
        if !high_capacity {
            self.command(16, BLOCK, Response::R1, delay)?;
        }
        let read_only = self.regs.read32(reg::PRESENT_STATE) & present::WRITABLE == 0;
        let card = Card { rca, high_capacity, blocks, cid, khz, width: 4, read_only };
        self.card = Some(card);
        Ok(card)
    }

    /// Starts `op` on `count` blocks from block `block`, to or from
    /// `buffers` (physical address and length, each a multiple of four
    /// bytes, together the blocks): the command goes once the lines are
    /// free, which [`Slot::poll`] looks for.
    pub fn start(
        &mut self,
        op: Op,
        block: u64,
        count: u32,
        buffers: &[(u64, u32)],
        delay: &mut dyn FnMut(u32),
    ) -> Result<(), Error> {
        let card = self.card.ok_or(Error::NoCard)?;
        let bytes: u64 = buffers.iter().map(|&(_, len)| u64::from(len)).sum();
        let fits = |&(phys, len): &(u64, u32)| {
            len > 0
                && len <= MAX_BUFFER
                && len % 4 == 0
                && phys % 4 == 0
                && (self.adma64 || phys + u64::from(len) <= 1 << 32)
        };
        if self.flight.is_some()
            || count == 0
            || count > u32::from(u16::MAX)
            || buffers.is_empty()
            || buffers.len() > MAX_BUFFERS
            || bytes != u64::from(count) * u64::from(BLOCK)
            || !buffers.iter().all(fits)
            || block + u64::from(count) > card.blocks
            || (op == Op::Write && card.read_only)
        {
            return Err(Error::BadRequest);
        }
        if self.stuck {
            // A transfer failed: the card is stopped, and both lines start
            // over.
            self.reset_lines(true, delay);
            let _ = self.command(12, 0, Response::R1b, delay);
            self.stuck = false;
        }
        // The descriptors: Valid and Act=Tran, End on the last.
        let size = if self.adma64 { 12 } else { 8 };
        for (i, &(phys, len)) in buffers.iter().enumerate() {
            let last = i + 1 == buffers.len();
            let attributes: u16 = 0x21 | if last { 0x02 } else { 0 };
            let at = i * size;
            self.table.write16(at, attributes);
            self.table.write16(at + 2, len as u16);
            self.table.write32(at + 4, phys as u32);
            if self.adma64 {
                self.table.write32(at + 8, (phys >> 32) as u32);
            }
        }
        device_barrier();
        let address = if card.high_capacity { block } else { block * u64::from(BLOCK) };
        let argument = u32::try_from(address).map_err(|_| Error::BadRequest)?;
        let multiple = count > 1;
        let index: u16 = match (op, multiple) {
            (Op::Read, false) => 17,
            (Op::Read, true) => 18,
            (Op::Write, false) => 24,
            (Op::Write, true) => 25,
        };
        let mut transfer = mode::DMA | mode::BLOCK_COUNT;
        if multiple {
            transfer |= mode::MULTIPLE | mode::AUTO_CMD12;
        }
        if op == Op::Read {
            transfer |= mode::READ;
        }
        let command = index << 8 | Response::R1.flags() | DATA_PRESENT;
        self.flight =
            Some(Flight::Ready { command, mode: transfer, argument, count: count as u16 });
        self.issue();
        Ok(())
    }

    /// Sends the command of the request that waits, if the lines are free.
    fn issue(&mut self) {
        let Some(Flight::Ready { command, mode, argument, count }) = self.flight else { return };
        let regs = &self.regs;
        let busy = present::COMMAND_INHIBIT | present::DATA_INHIBIT;
        if regs.read32(reg::PRESENT_STATE) & busy != 0 {
            return;
        }
        let table = self.table.phys();
        regs.write32(reg::ADMA_ADDRESS, table as u32);
        regs.write32(reg::ADMA_ADDRESS + 4, (table >> 32) as u32);
        let dma = if self.adma64 { 0b11 << 3 } else { 0b10 << 3 };
        regs.write8(reg::HOST_CONTROL, regs.read8(reg::HOST_CONTROL) & !(0b11 << 3) | dma);
        regs.write16(reg::NORMAL_STATUS, 0xffff);
        regs.write16(reg::ERROR_STATUS, 0xffff);
        regs.write16(reg::BLOCK_SIZE, BLOCK as u16);
        regs.write16(reg::BLOCK_COUNT, count);
        regs.write32(reg::ARGUMENT, argument);
        regs.write16(reg::TRANSFER_MODE, mode);
        regs.write16(reg::COMMAND, command);
        self.flight = Some(Flight::Sent { status: None });
    }

    /// How the request in flight ended; `None` while it goes on, or if
    /// there is none.
    pub fn poll(&mut self) -> Option<Result<(), Error>> {
        self.issue();
        let Some(Flight::Sent { status }) = self.flight else { return None };
        let regs = &self.regs;
        let normal = regs.read16(reg::NORMAL_STATUS);
        if normal & status::ERROR != 0 {
            let error = regs.read16(reg::ERROR_STATUS);
            regs.write16(reg::ERROR_STATUS, 0xffff);
            regs.write16(reg::NORMAL_STATUS, 0xffff);
            self.flight = None;
            self.stuck = true;
            return Some(Err(Error::Command(error)));
        }
        let mut status = status;
        if normal & status::COMMAND_COMPLETE != 0 {
            regs.write16(reg::NORMAL_STATUS, status::COMMAND_COMPLETE);
            status = Some(regs.read32(reg::RESPONSE));
            self.flight = Some(Flight::Sent { status });
        }
        if normal & status::TRANSFER_COMPLETE == 0 {
            return None;
        }
        regs.write16(reg::NORMAL_STATUS, 0xffff);
        self.flight = None;
        match status {
            Some(bits) if bits & CARD_ERRORS != 0 => {
                self.stuck = true;
                Some(Err(Error::Card(bits)))
            }
            _ => Some(Ok(())),
        }
    }

    /// Gives up the request in flight, which took too long: the lines are
    /// reset, and the card stopped before the next.
    pub fn abandon(&mut self, delay: &mut dyn FnMut(u32)) {
        self.flight = None;
        self.reset_lines(true, delay);
        self.stuck = true;
    }
}
