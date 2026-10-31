// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Memory arrays (type 16) and memory devices (type 17): where the RAM
//! is, how much, of what kind and speed, and whose. A device is a module
//! in a slot, which may be empty, or the chips of memory soldered to the
//! board. DSP0134's 7.17 and 7.18.

use crate::{Str, Structure};

/// A physical memory array: the slots or chips one controller drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryArray {
    /// Its handle, which its devices name.
    pub handle: u16,
    /// How it corrects errors (DSP0134's 7.17.3): 3 none, 5 single-bit,
    /// 6 multi-bit ECC.
    pub ecc: u8,
    /// The most RAM it takes, in bytes, if it says.
    pub capacity: Option<u64>,
    /// How many devices (slots or chips) it has.
    pub devices: u16,
}

impl MemoryArray {
    /// The array `s` describes, if it is one.
    pub fn parse(s: &Structure<'_>) -> Option<MemoryArray> {
        if s.kind != crate::kind::MEMORY_ARRAY {
            return None;
        }
        let kib = s.dword(0x07)?;
        // 0x80000000 says the size is in the extended field, in bytes.
        let capacity = match kib {
            0x8000_0000 => {
                s.bytes(0x0f, 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
            }
            kib => Some(u64::from(kib) << 10),
        };
        Some(MemoryArray {
            handle: s.handle,
            ecc: s.byte(0x06)?,
            capacity: capacity.filter(|c| *c != 0),
            devices: s.word(0x0d)?,
        })
    }
}

/// The name of error correction `ecc` (DSP0134's 7.17.3).
pub fn ecc_name(ecc: u8) -> &'static str {
    match ecc {
        0x01 => "Other",
        0x03 => "None",
        0x04 => "Parity",
        0x05 => "Single-bit ECC",
        0x06 => "Multi-bit ECC",
        0x07 => "CRC",
        _ => "Unknown",
    }
}

/// A memory device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryDevice<'a> {
    /// The array it is part of.
    pub array: u16,
    /// Where it is: its slot's name, `DIMM A1`.
    pub locator: Option<Str<'a>>,
    /// Its bank's name.
    pub bank: Option<Str<'a>>,
    /// Its size in bytes: 0 for an empty slot, none if unknown.
    pub size: Option<u64>,
    /// Its form (DSP0134's 7.18.1): 9 a DIMM, 13 a SODIMM, 11 a row of
    /// chips.
    pub form: u8,
    /// Its kind of memory (7.18.2): 0x1a DDR4, 0x22 DDR5, 0x23 LPDDR5.
    pub kind: u8,
    /// The most it runs at, in MT/s.
    pub speed: Option<u32>,
    /// What it was set to run at, in MT/s.
    pub configured_speed: Option<u32>,
    /// Who made it.
    pub manufacturer: Option<Str<'a>>,
    /// Its serial number.
    pub serial: Option<Str<'a>>,
    /// Its asset tag.
    pub asset_tag: Option<Str<'a>>,
    /// Its part number.
    pub part_number: Option<Str<'a>>,
    /// Its ranks.
    pub rank: Option<u8>,
    /// The width of its data, in bits.
    pub data_width: Option<u16>,
    /// Its whole width, with the bits for error correction.
    pub total_width: Option<u16>,
    /// The voltage it was set to, in millivolts.
    pub voltage: Option<u16>,
}

/// A speed in a word at `offset`: none for 0, or the double word at
/// `extended` for 0xffff.
fn speed(s: &Structure<'_>, offset: usize, extended: usize) -> Option<u32> {
    match s.word(offset)? {
        0 => None,
        0xffff => s.dword(extended).filter(|speed| *speed != 0),
        speed => Some(u32::from(speed)),
    }
}

impl<'a> MemoryDevice<'a> {
    /// The device `s` describes, if it is one.
    pub fn parse(s: &Structure<'a>) -> Option<MemoryDevice<'a>> {
        if s.kind != crate::kind::MEMORY_DEVICE {
            return None;
        }
        let size = match s.word(0x0c)? {
            0xffff => None,
            // In the extended field, in MiB.
            0x7fff => s.dword(0x1c).map(|mib| u64::from(mib & 0x7fff_ffff) << 20),
            kib if kib & 0x8000 != 0 => Some(u64::from(kib & 0x7fff) << 10),
            mib => Some(u64::from(mib) << 20),
        };
        let width = |offset| s.word(offset).filter(|w| *w != 0 && *w != 0xffff);
        Some(MemoryDevice {
            array: s.word(0x04)?,
            locator: s.string(0x10),
            bank: s.string(0x11),
            size,
            form: s.byte(0x0e)?,
            kind: s.byte(0x12)?,
            speed: speed(s, 0x15, 0x54),
            configured_speed: speed(s, 0x20, 0x58),
            manufacturer: s.string(0x17),
            serial: s.string(0x18),
            asset_tag: s.string(0x19),
            part_number: s.string(0x1a),
            rank: s.byte(0x1b).map(|attributes| attributes & 0x0f).filter(|rank| *rank != 0),
            data_width: width(0x0a),
            total_width: width(0x08),
            voltage: s.word(0x26).filter(|mv| *mv != 0),
        })
    }

    /// Whether a module is there: a slot left empty says size 0.
    pub fn is_present(&self) -> bool {
        self.size != Some(0)
    }
}

/// The name of memory kind `kind` (DSP0134's 7.18.2).
pub fn kind_name(kind: u8) -> &'static str {
    const NAMES: [&str; 0x24] = [
        "Other",
        "Unknown",
        "DRAM",
        "EDRAM",
        "VRAM",
        "SRAM",
        "RAM",
        "ROM",
        "Flash",
        "EEPROM",
        "FEPROM",
        "EPROM",
        "CDRAM",
        "3DRAM",
        "SDRAM",
        "SGRAM",
        "RDRAM",
        "DDR",
        "DDR2",
        "DDR2 FB-DIMM",
        "Reserved",
        "Reserved",
        "Reserved",
        "DDR3",
        "FBD2",
        "DDR4",
        "LPDDR",
        "LPDDR2",
        "LPDDR3",
        "LPDDR4",
        "Logical non-volatile device",
        "HBM",
        "HBM2",
        "DDR5",
        "LPDDR5",
        "HBM3",
    ];
    usize::from(kind).checked_sub(1).and_then(|at| NAMES.get(at)).copied().unwrap_or("Unknown")
}

/// The name of form `form` (DSP0134's 7.18.1).
pub fn form_name(form: u8) -> &'static str {
    const NAMES: [&str; 0x11] = [
        "Other",
        "Unknown",
        "SIMM",
        "SIP",
        "Chip",
        "DIP",
        "ZIP",
        "Proprietary Card",
        "DIMM",
        "TSOP",
        "Row Of Chips",
        "RIMM",
        "SODIMM",
        "SRIMM",
        "FB-DIMM",
        "Die",
        "CAMM",
    ];
    usize::from(form).checked_sub(1).and_then(|at| NAMES.get(at)).copied().unwrap_or("Unknown")
}
