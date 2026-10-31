// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Memory modules' SPD data (Phase 11e, ADR 0232): the EEPROM on each
//! module, at 50h to 57h on the chipset's SMBus, that says what the
//! module is, who made it and when.
//!
//! Which EEPROM a module has follows from its memory's kind, which the
//! SMBIOS tables give, as Linux's `i2c_register_spd` chooses ([`Eeprom`]):
//!
//! - DDR to DDR3 and LPDDR to LPDDR3: a plain EEPROM of 256 bytes, each
//!   read at its offset (JEDEC's SPD, JESD21-C 4.1.2).
//! - DDR4 and LPDDR4: an EE1004, 512 bytes in two pages of 256. A Send
//!   Byte to 36h or 37h picks page 0 or 1 for every module on the bus at
//!   once; some modules take it without acknowledging, so the page is
//!   read back, by a Receive Byte at 36h that only page 0 acknowledges
//!   (JEDEC's EE1004-v).
//! - DDR5 and LPDDR5: an SPD5118 hub, whose registers MR0 and MR1 read
//!   51h and 18h. Its 1024 bytes are eight pages of 128, which MR11's
//!   bits 2:0 pick and offsets 80h to FFh read, with MR11's bit 3 clear
//!   for one-byte offsets (JEDEC's JESD300-5).
//!
//! [`read`] reads one whole over a [`Bus`], and leaves an EE1004 on page
//! 0 and an SPD5118 on the page it was on. [`Spd`] decodes what it read:
//! the memory's kind, the module's form, size and speed, its maker, date,
//! serial number and part number, and its checksums, by JEDEC's layouts
//! for DDR3 (JESD21-C Annex K), DDR4 (Annex L) and DDR5 (JESD400-5),
//! with makers' names from JEP106. The kernel reads; `hwinfo` decodes. No
//! code is taken from Linux or i2c-tools.

#![cfg_attr(not(test), no_std)]

#[cfg(test)]
mod tests;

use core::fmt;

/// The most bytes an SPD EEPROM holds: an SPD5118's.
pub const MAX_SIZE: usize = 1024;

/// Where the memory modules' EEPROMs answer: 50h for the first slot on.
pub const ADDRESSES: core::ops::RangeInclusive<u8> = 0x50..=0x57;

/// An EE1004's page selects, for pages 0 and 1; the first also tells the
/// page picked.
const EE1004_PAGE: [u8; 2] = [0x36, 0x37];
const EE1004_PAGE_SIZE: usize = 256;

/// An SPD5118's registers: its device type, and the legacy mode's
/// configuration with the page.
const SPD5118_TYPE: [u8; 2] = [0x51, 0x18];
const MR11: u8 = 0x0b;
const MR11_TWO_BYTES: u8 = 1 << 3;
const MR11_PAGE: u8 = 0b111;
/// Where the page picked reads.
const SPD5118_NVM: u8 = 0x80;
const SPD5118_PAGE_SIZE: usize = 128;

/// The SMBus transactions an SPD read takes, to 7-bit addresses.
pub trait Bus {
    /// Why one failed.
    type Error: fmt::Display;

    /// Send Byte: `value` alone.
    fn send_byte(&mut self, address: u8, value: u8) -> Result<(), Self::Error>;

    /// Receive Byte.
    fn receive_byte(&mut self, address: u8) -> Result<u8, Self::Error>;

    /// Read Byte: the byte at `command`.
    fn read_byte(&mut self, address: u8, command: u8) -> Result<u8, Self::Error>;

    /// Write Byte: `value` at `command`.
    fn write_byte(&mut self, address: u8, command: u8, value: u8) -> Result<(), Self::Error>;

    /// Whether `error` is that no device acknowledged.
    fn is_nack(error: &Self::Error) -> bool;
}

/// A kind of SPD EEPROM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eeprom {
    /// 256 bytes, read at their offsets.
    Spd,
    /// An EE1004: two pages of 256.
    Ee1004,
    /// An SPD5118: eight pages of 128.
    Spd5118,
}

impl Eeprom {
    /// The EEPROM of modules of memory kind `kind`, as SMBIOS type 17
    /// numbers kinds (DSP0134's 7.18.2), if there is one the kernel reads.
    pub fn of_kind(kind: u8) -> Option<Eeprom> {
        match kind {
            // DDR, DDR2, DDR3, LPDDR, LPDDR2 and LPDDR3.
            0x12 | 0x13 | 0x18 | 0x1b..=0x1d => Some(Eeprom::Spd),
            // DDR4 and LPDDR4.
            0x1a | 0x1e => Some(Eeprom::Ee1004),
            // DDR5 and LPDDR5.
            0x22 | 0x23 => Some(Eeprom::Spd5118),
            _ => None,
        }
    }

    /// Linux's name for its driver, which `/sys/bus/i2c/devices/*/name`
    /// gives.
    pub fn name(self) -> &'static str {
        match self {
            Eeprom::Spd => "spd",
            Eeprom::Ee1004 => "ee1004",
            Eeprom::Spd5118 => "spd5118",
        }
    }

    /// How many bytes it holds.
    pub fn size(self) -> usize {
        match self {
            Eeprom::Spd => 256,
            Eeprom::Ee1004 => 2 * EE1004_PAGE_SIZE,
            Eeprom::Spd5118 => 8 * SPD5118_PAGE_SIZE,
        }
    }
}

/// Why a read failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<E> {
    /// A transaction failed.
    Bus(E),
    /// An EE1004's page could not be picked.
    Page(u8),
    /// An SPD5118 takes two-byte offsets, which byte reads cannot give.
    TwoByteOffsets,
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bus(error) => write!(f, "{error}"),
            Error::Page(page) => write!(f, "page {page} could not be picked"),
            Error::TwoByteOffsets => f.write_str("the SPD5118 is set for two-byte offsets"),
        }
    }
}

/// Whether the device at `address` is an SPD5118, by its MR0 and MR1.
pub fn is_spd5118<B: Bus>(bus: &mut B, address: u8) -> bool {
    let mut id = [0; 2];
    for (register, byte) in id.iter_mut().enumerate() {
        match bus.read_byte(address, register as u8) {
            Ok(value) => *byte = value,
            Err(_) => return false,
        }
    }
    id == SPD5118_TYPE
}

/// Reads the whole of the `eeprom` at `address` into `out`, which must
/// hold [`Eeprom::size`] bytes; the rest of `out` is left alone.
pub fn read<B: Bus>(
    bus: &mut B,
    eeprom: Eeprom,
    address: u8,
    out: &mut [u8],
) -> Result<(), Error<B::Error>> {
    let out = &mut out[..eeprom.size()];
    match eeprom {
        Eeprom::Spd => read_bytes(bus, address, 0, out),
        Eeprom::Ee1004 => {
            let mut result = Ok(());
            for (page, bytes) in (0..).zip(out.chunks_mut(EE1004_PAGE_SIZE)) {
                result = ee1004_page(bus, page).and_then(|()| read_bytes(bus, address, 0, bytes));
                if result.is_err() {
                    break;
                }
            }
            // Modules start on page 0, and firmware may read them so.
            result.and(ee1004_page(bus, 0))
        }
        Eeprom::Spd5118 => {
            let config = bus.read_byte(address, MR11).map_err(Error::Bus)?;
            if config & MR11_TWO_BYTES != 0 {
                return Err(Error::TwoByteOffsets);
            }
            let mut result = Ok(());
            for (page, bytes) in (0..).zip(out.chunks_mut(SPD5118_PAGE_SIZE)) {
                result = bus
                    .write_byte(address, MR11, config & !MR11_PAGE | page)
                    .map_err(Error::Bus)
                    .and_then(|()| read_bytes(bus, address, SPD5118_NVM, bytes));
                if result.is_err() {
                    break;
                }
            }
            result.and(bus.write_byte(address, MR11, config).map_err(Error::Bus))
        }
    }
}

/// Reads `out.len()` bytes from offset `from` on, a byte at a time.
fn read_bytes<B: Bus>(
    bus: &mut B,
    address: u8,
    from: u8,
    out: &mut [u8],
) -> Result<(), Error<B::Error>> {
    for (offset, byte) in (from..=u8::MAX).zip(out.iter_mut()) {
        *byte = bus.read_byte(address, offset).map_err(Error::Bus)?;
    }
    Ok(())
}

/// Picks an EE1004's page for every module on the bus.
fn ee1004_page<B: Bus>(bus: &mut B, page: u8) -> Result<(), Error<B::Error>> {
    match bus.send_byte(EE1004_PAGE[usize::from(page)], 0) {
        Ok(()) => Ok(()),
        // Some modules pick it without acknowledging: ask which they did.
        Err(error) if B::is_nack(&error) => match bus.receive_byte(EE1004_PAGE[0]) {
            Ok(_) if page == 0 => Ok(()),
            Err(error) if B::is_nack(&error) && page == 1 => Ok(()),
            Ok(_) => Err(Error::Page(page)),
            Err(error) if B::is_nack(&error) => Err(Error::Page(page)),
            Err(error) => Err(Error::Bus(error)),
        },
        Err(error) => Err(Error::Bus(error)),
    }
}

/// What a module's SPD bytes say of it, for the kinds of memory whose
/// layout this crate knows: DDR3, DDR4 and DDR5. Each reading is `None`
/// where the bytes do not say, are not all there or make no sense.
#[derive(Clone, Copy, Debug)]
pub struct Spd<'a> {
    bytes: &'a [u8],
}

/// The layouts known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Ddr3,
    Ddr4,
    Ddr5,
}

/// Where a layout keeps the module's maker, date, serial number and part
/// number, and how long the part number is.
struct Made {
    maker: usize,
    date: usize,
    serial: usize,
    part: usize,
    part_len: usize,
}

impl Layout {
    fn made(self) -> Made {
        match self {
            Layout::Ddr3 => Made { maker: 117, date: 120, serial: 122, part: 128, part_len: 18 },
            Layout::Ddr4 => Made { maker: 320, date: 323, serial: 325, part: 329, part_len: 20 },
            Layout::Ddr5 => Made { maker: 512, date: 515, serial: 517, part: 521, part_len: 30 },
        }
    }

    /// JEDEC's speed grades, in MT/s.
    fn grades(self) -> &'static [u32] {
        match self {
            Layout::Ddr3 => &[800, 1066, 1333, 1600, 1866, 2133],
            Layout::Ddr4 => &[1600, 1866, 2133, 2400, 2666, 2933, 3200],
            Layout::Ddr5 => &[
                3200, 3600, 4000, 4400, 4800, 5200, 5600, 6000, 6400, 6800, 7200, 7600, 8000, 8400,
                8800,
            ],
        }
    }
}

/// A maker's code from JEP106: its bank, from 1, and its number in the
/// bank, odd parity bit and all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Maker {
    /// The bank, from 1.
    pub bank: u8,
    /// The number in the bank, with its parity bit.
    pub code: u8,
}

impl Maker {
    /// The maker's name, for the makers of memory modules and chips that
    /// JEP106 lists and this crate knows.
    pub fn name(self) -> Option<&'static str> {
        Some(match (self.bank, self.code) {
            (1, 0x2c) => "Micron Technology",
            (1, 0xad) => "SK Hynix",
            (1, 0xce) => "Samsung",
            (1, 0xfe) => "Elpida",
            (2, 0x98) => "Kingston",
            (3, 0x9e) => "Corsair",
            (4, 0x0b) => "Nanya Technology",
            (5, 0x43) => "Ramaxel Technology",
            (5, 0xcb) => "A-DATA Technology",
            (5, 0xcd) => "G.Skill",
            (5, 0xef) => "Team Group",
            (6, 0x9b) => "Crucial Technology",
            _ => return None,
        })
    }
}

impl fmt::Display for Maker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "JEP106 bank {} maker {:#04x}", self.bank, self.code),
        }
    }
}

/// When a module was made: the year and its week.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
    /// The year.
    pub year: u16,
    /// Its week, from 1.
    pub week: u8,
}

/// A module's serial number: four bytes, shown in hexadecimal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Serial(pub [u8; 4]);

impl fmt::Display for Serial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a:02X}{b:02X}{c:02X}{d:02X}")
    }
}

/// A two-digit BCD number's value.
fn bcd(byte: u8) -> Option<u8> {
    let (tens, ones) = (byte >> 4, byte & 0x0f);
    (tens < 10 && ones < 10).then_some(tens * 10 + ones)
}

/// The CRC that SPD data's checksums are: CRC-16 of polynomial 1021h,
/// starting from 0.
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 { crc << 1 } else { crc << 1 ^ 0x1021 };
        }
    }
    crc
}

/// The grade `rate` is within 1% of, or `rate` if none.
fn grade(rate: u32, grades: &[u32]) -> u32 {
    grades.iter().copied().find(|&grade| grade.abs_diff(rate) * 100 <= grade).unwrap_or(rate)
}

/// Rounds `2e6 / picoseconds`: the transfers per microsecond of a clock
/// of that period, two to a cycle.
fn transfers(picoseconds: u64) -> Option<u32> {
    (picoseconds != 0).then(|| ((2_000_000 + picoseconds / 2) / picoseconds) as u32)
}

impl<'a> Spd<'a> {
    /// The bytes read.
    pub fn new(bytes: &'a [u8]) -> Spd<'a> {
        Spd { bytes }
    }

    fn byte(&self, offset: usize) -> Option<u8> {
        self.bytes.get(offset).copied()
    }

    fn layout(&self) -> Option<Layout> {
        match self.byte(2)? {
            0x0b => Some(Layout::Ddr3),
            0x0c => Some(Layout::Ddr4),
            0x12 => Some(Layout::Ddr5),
            _ => None,
        }
    }

    /// Whether the EEPROM holds nothing: its memory kind byte is 0 or all
    /// ones, as an EEPROM never written reads.
    pub fn is_blank(&self) -> bool {
        matches!(self.byte(2), None | Some(0 | 0xff))
    }

    /// The kind of memory, as SPD byte 2 names it: `DDR4`, `LPDDR5X`...
    pub fn kind(&self) -> Option<&'static str> {
        const NAMES: [&str; 0x15] = [
            "FPM DRAM",
            "EDO DRAM",
            "Pipelined Nibble",
            "SDRAM",
            "ROM",
            "DDR SGRAM",
            "DDR",
            "DDR2",
            "DDR2 FB-DIMM",
            "DDR2 FB-DIMM Probe",
            "DDR3",
            "DDR4",
            "",
            "DDR4E",
            "LPDDR3",
            "LPDDR4",
            "LPDDR4X",
            "DDR5",
            "LPDDR5",
            "DDR5 NVDIMM-P",
            "LPDDR5X",
        ];
        let name = NAMES.get(usize::from(self.byte(2)?).checked_sub(1)?)?;
        (!name.is_empty()).then_some(*name)
    }

    /// The module's form: `UDIMM`, `SO-DIMM`, `RDIMM`...
    pub fn form(&self) -> Option<&'static str> {
        let layout = self.layout()?;
        Some(match (layout, self.byte(3)? & 0x0f) {
            (_, 1) => "RDIMM",
            (_, 2) => "UDIMM",
            (_, 3) => "SO-DIMM",
            (Layout::Ddr3, 4) => "Micro-DIMM",
            (Layout::Ddr4 | Layout::Ddr5, 4) | (Layout::Ddr3, 11) => "LRDIMM",
            _ => return None,
        })
    }

    /// The module's size, in bytes.
    pub fn size(&self) -> Option<u64> {
        let byte = |offset| self.byte(offset);
        let mib: u64 = match self.layout()? {
            Layout::Ddr3 => {
                let density = byte(4)? & 0x0f;
                let (width, ranks, bus) = (byte(7)? & 7, byte(7)? >> 3 & 7, byte(8)? & 7);
                if density > 6 || width > 3 || bus > 3 {
                    return None;
                }
                let ranks = [1, 2, 3, 4, 8].get(usize::from(ranks)).copied()?;
                (256u64 << density) / 8 * (8 << bus) / (4 << width) * ranks
            }
            Layout::Ddr4 => {
                const DENSITY: [u64; 10] =
                    [256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 12288, 24576];
                let density = DENSITY.get(usize::from(byte(4)? & 0x0f)).copied()?;
                let package = byte(6)?;
                // A 3DS stack's dies are ranks of their own.
                let dies = if package & 0b11 == 0b10 { u64::from(package >> 4 & 7) + 1 } else { 1 };
                let (width, ranks, bus) = (byte(12)? & 7, byte(12)? >> 3 & 7, byte(13)? & 7);
                if width > 3 || bus > 3 {
                    return None;
                }
                density / 8 * (8 << bus) / (4 << width) * (u64::from(ranks) + 1) * dies
            }
            Layout::Ddr5 => {
                const DENSITY: [u64; 9] = [0, 4096, 8192, 12288, 16384, 24576, 32768, 49152, 65536];
                let density =
                    DENSITY.get(usize::from(byte(4)? & 0x1f)).copied().filter(|d| *d != 0)?;
                let dies = match byte(4)? >> 5 {
                    0 => 1,
                    code @ 2..=5 => 1 << (code - 1),
                    _ => return None,
                };
                let width = byte(6)? >> 5;
                let ranks = u64::from(byte(234)? >> 3 & 7) + 1;
                let (bus, channels) = (byte(235)? & 7, u64::from(byte(235)? >> 5 & 3) + 1);
                if width > 3 || bus > 3 {
                    return None;
                }
                channels * (8 << bus) / (4 << width) * dies * density / 8 * ranks
            }
        };
        (mib != 0).then_some(mib << 20)
    }

    /// The fastest the module runs by JEDEC's timings, in MT/s, as its
    /// speed grade names it: 2666 for a DDR4-2666 module.
    pub fn speed(&self) -> Option<u32> {
        let layout = self.layout()?;
        let rate = match layout {
            Layout::Ddr3 => {
                // The medium and fine timebases, in femtoseconds.
                let (dividend, divisor) = (u64::from(self.byte(10)?), u64::from(self.byte(11)?));
                let medium = (1_000_000 * dividend).checked_div(divisor)?;
                let fine = self.byte(9)?;
                let fine =
                    (1000 * u64::from(fine >> 4)).checked_div(u64::from(fine & 0x0f)).unwrap_or(0);
                let correction = i64::from(self.byte(34)? as i8) * fine as i64;
                let period = (u64::from(self.byte(12)?) * medium).checked_add_signed(correction)?;
                transfers(period / 1000)?
            }
            Layout::Ddr4 => {
                // 125 ps and 1 ps are the timebases defined.
                if self.byte(17)? & 0x0f != 0 {
                    return None;
                }
                let period = (u64::from(self.byte(18)?) * 125)
                    .checked_add_signed(i64::from(self.byte(125)? as i8))?;
                transfers(period)?
            }
            Layout::Ddr5 => {
                transfers(u64::from(u16::from_le_bytes([self.byte(20)?, self.byte(21)?])))?
            }
        };
        Some(grade(rate, layout.grades()))
    }

    /// Who made the module.
    pub fn maker(&self) -> Option<Maker> {
        let at = self.layout()?.made().maker;
        let (bank, code) = (self.byte(at)?, self.byte(at + 1)?);
        if matches!((bank, code), (0, 0) | (0xff, 0xff)) {
            return None;
        }
        Some(Maker { bank: (bank & 0x7f) + 1, code })
    }

    /// When it was made.
    pub fn date(&self) -> Option<Date> {
        let at = self.layout()?.made().date;
        let (year, week) = (bcd(self.byte(at)?)?, bcd(self.byte(at + 1)?)?);
        (1..=53).contains(&week).then_some(Date { year: 2000 + u16::from(year), week })
    }

    /// Its serial number.
    pub fn serial(&self) -> Option<Serial> {
        let at = self.layout()?.made().serial;
        let bytes: [u8; 4] = self.bytes.get(at..at + 4)?.try_into().ok()?;
        (bytes != [0; 4] && bytes != [0xff; 4]).then_some(Serial(bytes))
    }

    /// Its part number, without the spaces it is padded with.
    pub fn part_number(&self) -> Option<&'a str> {
        let made = self.layout()?.made();
        let bytes = self.bytes.get(made.part..made.part + made.part_len)?;
        let end = bytes.iter().rposition(|b| !matches!(b, b' ' | 0 | 0xff))? + 1;
        let bytes = &bytes[..end];
        if !bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            return None;
        }
        core::str::from_utf8(bytes).ok()
    }

    /// Whether the checksums the layout has match the bytes they cover:
    /// DDR3's of its first 117 or 126 bytes, DDR4's of its first two
    /// blocks of 128, DDR5's of its first 510 bytes.
    pub fn checksums_match(&self) -> Option<bool> {
        match self.layout()? {
            Layout::Ddr3 => {
                let covered = if self.byte(0)? & 0x80 != 0 { 117 } else { 126 };
                self.crc_matches(0..covered, 126)
            }
            Layout::Ddr4 => {
                Some(self.crc_matches(0..126, 126)? && self.crc_matches(128..254, 254)?)
            }
            Layout::Ddr5 => self.crc_matches(0..510, 510),
        }
    }

    /// Whether the CRC of the bytes `covered` is the one stored at `at`,
    /// low byte first.
    fn crc_matches(&self, covered: core::ops::Range<usize>, at: usize) -> Option<bool> {
        let stored = u16::from_le_bytes([self.byte(at)?, self.byte(at + 1)?]);
        Some(crc16(self.bytes.get(covered)?) == stored)
    }
}
