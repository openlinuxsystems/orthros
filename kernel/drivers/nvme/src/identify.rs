// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a controller says of itself and of its namespaces, in answer to
//! Identify (NVM Express Base Specification 2.0, 5.17.2.1; NVM Command
//! Set Specification 1.0, 4.1.5): read field by field from the first
//! kibibyte of the 4 KiB the controller wrote, whatever it wrote.

use crate::command::LIST_ENTRIES;

/// The size of an Identify data structure.
pub const SIZE: usize = 4096;
/// How much of one the driver reads: every field it uses is in the first
/// kibibyte.
pub const HEAD: usize = 1024;

/// The size of a block the driver takes: the block layer's sector.
const BLOCK: u32 = 512;

/// What a controller is and does (Identify Controller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controller {
    /// The PCI vendor ID it gives.
    pub vendor: u16,
    /// Its serial number, model and firmware revision, in ASCII padded
    /// with spaces.
    pub serial: [u8; 20],
    /// See `serial`.
    pub model: [u8; 40],
    /// See `serial`.
    pub firmware: [u8; 8],
    /// The most pages one command's data take: from its largest transfer
    /// (MDTS), and at most the [`LIST_ENTRIES`] pages after the first
    /// that one PRP list holds.
    pub max_pages: usize,
    /// How many namespace IDs it has, the highest valid one (NN).
    pub namespaces: u32,
    /// Whether it has a volatile write cache, which Flush empties.
    pub write_cache: bool,
    /// Whether it deallocates blocks (Dataset Management).
    pub deallocate: bool,
    /// Whether it takes shadow doorbells (Doorbell Buffer Config, OACS
    /// bit 8), as controllers that hypervisors emulate may.
    pub shadow_doorbells: bool,
    /// How many blocks a write may cover and still be whole after a
    /// power failure (AWUPF), for a namespace that does not say.
    pub atomic_blocks: u32,
    /// How long it may take to shut down, in microseconds, if it says
    /// (RTD3E).
    pub shutdown_us: u32,
    /// The version it says it implements (VER), 0 for one before 1.2.
    pub version: u32,
}

/// What a namespace is (Identify Namespace).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Namespace {
    /// Its size in 512-byte blocks.
    pub blocks: u64,
    /// The smallest unit it writes without reading first, in bytes: the
    /// size it is best written in (NPWG), but no more than a write that
    /// is whole after a power failure; a power of two from 512 to 64 KiB.
    pub block_size: u32,
    /// Whether it is write-protected.
    pub read_only: bool,
}

/// Why a namespace is not one the driver takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unusable {
    /// It has no blocks: an inactive namespace ID.
    Empty,
    /// Its blocks are not 512 bytes: the size in bytes, or 0 for one too
    /// large to say.
    BlockSize(u32),
    /// Its blocks carry metadata, or end-to-end protection information.
    Metadata,
}

fn u16_at(data: &[u8; HEAD], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8; HEAD], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn u64_at(data: &[u8; HEAD], at: usize) -> u64 {
    u64::from(u32_at(data, at)) | (u64::from(u32_at(data, at + 4)) << 32)
}

impl Controller {
    /// A controller that has said nothing yet.
    pub const UNKNOWN: Controller = Controller {
        vendor: 0,
        serial: [0; 20],
        model: [0; 40],
        firmware: [0; 8],
        max_pages: 1,
        namespaces: 0,
        write_cache: false,
        deallocate: false,
        shadow_doorbells: false,
        atomic_blocks: 1,
        shutdown_us: 0,
        version: 0,
    };
}

/// Reads Identify Controller data.
pub fn controller(data: &[u8; HEAD]) -> Controller {
    let mut serial = [0u8; 20];
    serial.copy_from_slice(&data[4..24]);
    let mut model = [0u8; 40];
    model.copy_from_slice(&data[24..64]);
    let mut firmware = [0u8; 8];
    firmware.copy_from_slice(&data[64..72]);
    // MDTS is a power of two of the smallest memory page, which the
    // driver requires to be 4 KiB; 0 means no limit.
    let mdts = data[77];
    let max_pages =
        if mdts == 0 || mdts >= 8 { LIST_ENTRIES } else { (1 << mdts).min(LIST_ENTRIES) };
    Controller {
        vendor: u16_at(data, 0),
        serial,
        model,
        firmware,
        max_pages,
        namespaces: u32_at(data, 516),
        write_cache: data[525] & 1 != 0,
        deallocate: u16_at(data, 520) & (1 << 2) != 0,
        shadow_doorbells: u16_at(data, 256) & (1 << 8) != 0,
        atomic_blocks: u32::from(u16_at(data, 528)) + 1,
        shutdown_us: u32_at(data, 88),
        version: u32_at(data, 80),
    }
}

/// Reads the I/O command set's Identify Controller data for the NVM
/// command set: the most blocks one Dataset Management range covers
/// (DMRSL), or 0 for no limit.
pub fn range_limit(data: &[u8; HEAD]) -> u32 {
    u32_at(data, 4)
}

/// Reads Identify Namespace data, of a controller whose writes of
/// `atomic_blocks` are whole after a power failure.
pub fn namespace(data: &[u8; HEAD], atomic_blocks: u32) -> Result<Namespace, Unusable> {
    let blocks = u64_at(data, 0);
    if blocks == 0 {
        return Err(Unusable::Empty);
    }
    let features = data[24];
    let formats = usize::from(data[25]) + 1;
    let flbas = data[26];
    // The format in use: the low four bits, and two more above bit 4 for
    // a namespace with more than 16 formats.
    let index = usize::from(flbas & 0xf) | (usize::from((flbas >> 5) & 0x3) << 4);
    if index >= formats.min(64) {
        return Err(Unusable::BlockSize(0));
    }
    let format = u32_at(data, 128 + 4 * index);
    let metadata = format & 0xffff;
    let shift = (format >> 16) & 0xff;
    let size = if shift < 32 { 1u32 << shift } else { 0 };
    if size != BLOCK {
        return Err(Unusable::BlockSize(size));
    }
    // Protection information (DPS) needs metadata too, but a namespace
    // may say it with none.
    if metadata != 0 || data[29] & 0x7 != 0 {
        return Err(Unusable::Metadata);
    }
    // The preferred write granularity (NPWG), when it says (OPTPERF).
    let granularity = if features & (1 << 4) != 0 {
        (u32::from(u16_at(data, 64)) + 1).saturating_mul(BLOCK)
    } else {
        BLOCK
    };
    // The namespace's own power-fail atomic write (NAWUPF), when it says
    // (NSABP), or else the controller's.
    let atomic =
        if features & (1 << 1) != 0 { u32::from(u16_at(data, 44)) + 1 } else { atomic_blocks };
    let block_size = granularity.min(atomic.saturating_mul(BLOCK));
    let block_size =
        if block_size.is_power_of_two() && block_size <= 65_536 { block_size } else { BLOCK };
    Ok(Namespace { blocks, block_size, read_only: data[99] & 1 != 0 })
}

/// Whether `id`, read after `last` from an Identify active namespace
/// list, continues it: the list is in rising order and ends at the first
/// 0.
pub fn continues(last: u32, id: u32) -> bool {
    id > last
}

/// A string from Identify data with its padding cut off.
pub fn trimmed(text: &[u8]) -> &str {
    let text = core::str::from_utf8(text).unwrap_or("");
    text.trim_matches(|c: char| c == ' ' || c == '\0')
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Whatever a controller answers to Identify Controller, reading it
    /// does not panic, and a command's pages stay within one PRP list.
    #[kani::proof]
    fn controller_reads_any_answer() {
        let data: [u8; HEAD] = kani::any();
        let found = controller(&data);
        assert!((1..=LIST_ENTRIES).contains(&found.max_pages));
    }

    /// Whatever a controller answers to Identify Namespace, reading it
    /// does not panic, and a namespace it accepts has blocks of 512
    /// bytes, no metadata, and a write size the block layer takes.
    #[kani::proof]
    fn namespace_reads_any_answer() {
        let data: [u8; HEAD] = kani::any();
        if let Ok(found) = namespace(&data, kani::any()) {
            assert!(found.blocks > 0);
            assert!(found.block_size.is_power_of_two());
            assert!((512..=65_536).contains(&found.block_size));
            let index = usize::from(data[26] & 0xf) | (usize::from((data[26] >> 5) & 0x3) << 4);
            assert_eq!(data[128 + 4 * index + 2], 9);
            assert_eq!(data[128 + 4 * index], 0);
            assert_eq!(data[128 + 4 * index + 1], 0);
        }
    }
}
