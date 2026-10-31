// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Finding the firmware's SMBIOS tables (Phase 11e, ADR 0231).
//!
//! Under UEFI the system table's configuration tables say where the entry
//! point is; without UEFI the BIOS keeps it on a 16-byte boundary from
//! 0xf0000 to 0xfffff. Either way the 64-bit entry point, `_SM3_`, comes
//! before the 32-bit one, as Linux takes them. The entry point says where
//! the structures are, which `radix-smbios` reads.
//!
//! The kernel reads the tables where the firmware keeps them, as Linux
//! does, in memory the firmware's memory map reserves. Tables in memory
//! it calls free, above the first MiB, may already be the kernel's own
//! by the time it looks, so they are refused.

use radix_acpi::Memory;
use radix_hal::{MemoryKind, Smbios};
use radix_smbios::{ANCHOR_32, ANCHOR_64, Entry, MAX_ENTRY, Table};
use radix_sync::Once;

use crate::acpi::PhysicalMemory;
use crate::memory;

/// The configuration table's GUID for the 64-bit entry point,
/// f2fd1544-9794-4a2c-992e-e5bbcf20e394, as it is in memory.
const SMBIOS3_GUID: [u8; 16] = [
    0x44, 0x15, 0xfd, 0xf2, 0x94, 0x97, 0x2c, 0x4a, 0x99, 0x2e, 0xe5, 0xbb, 0xcf, 0x20, 0xe3, 0x94,
];

/// And for the 32-bit one, eb9d2d31-2d88-11d3-9a16-0090273fc14d.
const SMBIOS_GUID: [u8; 16] = [
    0x31, 0x2d, 0x9d, 0xeb, 0x88, 0x2d, 0xd3, 0x11, 0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d,
];

/// The UEFI system table's signature, `IBI SYST`.
const SYSTEM_TABLE: u64 = 0x5453_5953_2049_4249;

/// The most configuration tables the kernel looks through: firmware has a
/// dozen or two.
const MAX_CONFIG_TABLES: u64 = 256;

/// The longest structure table the kernel takes. SMBIOS 2's are at most
/// 64 KiB; SMBIOS 3's may be longer, but servers' run to tens of KiB.
const MAX_TABLE: u64 = 1 << 20;

static FOUND: Once<Smbios> = Once::new();

/// The tables [`init`] found.
pub fn get() -> Option<Smbios> {
    FOUND.get().copied()
}

/// Finds the tables, through the UEFI system table at `system_table` if
/// the boot loader passed it, else in the BIOS's area, and logs why when
/// there are none the kernel can use.
pub fn init(system_table: Option<u64>) {
    let found = match system_table {
        Some(address) => from_uefi(address),
        None => from_bios(),
    };
    let Some((at, entry)) = found else {
        radix_klog::info!("SMBIOS: the firmware gives no tables");
        return;
    };
    let length = entry.length.min(MAX_TABLE as u32);
    let range = entry.address..entry.address.saturating_add(u64::from(length));
    if in_free_memory(range.clone()) {
        radix_klog::warn!(
            "SMBIOS: the firmware's tables at {:#x} are in memory its map calls free; they are \
             not used",
            entry.address
        );
        return;
    }
    let memory = PhysicalMemory;
    let (Some(entry_bytes), Some(table)) =
        (memory.bytes(at, entry.size), memory.bytes(range.start, length as usize))
    else {
        return;
    };
    let used = Table::new(table, entry.count).used();
    if used == 0 {
        radix_klog::warn!("SMBIOS: the table at {:#x} holds no structures", entry.address);
        return;
    }
    let _ = FOUND.set(Smbios { entry: entry_bytes, table: &table[..used] });
}

/// The entry point the UEFI system table at `address` names, and where
/// it is.
fn from_uefi(address: u64) -> Option<(u64, Entry)> {
    let memory = PhysicalMemory;
    let system = memory.bytes(address, 120)?;
    let word = |bytes: &[u8], at: usize| {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or_default())
    };
    if word(system, 0) != SYSTEM_TABLE {
        radix_klog::warn!("SMBIOS: no UEFI system table at {address:#x}");
        return None;
    }
    let count = word(system, 104).min(MAX_CONFIG_TABLES);
    let tables = memory.bytes(word(system, 112), count as usize * 24)?;
    let find = |guid: &[u8; 16]| {
        let (entries, _) = tables.as_chunks::<24>();
        entries.iter().find(|table| table[..16] == *guid).map(|table| word(table, 16))
    };
    [find(&SMBIOS3_GUID), find(&SMBIOS_GUID)]
        .into_iter()
        .flatten()
        .find_map(|at| Some((at, Entry::parse(memory.bytes(at, MAX_ENTRY)?)?)))
}

/// The entry point in the BIOS's area, and where it is.
fn from_bios() -> Option<(u64, Entry)> {
    const BASE: u64 = 0xf0000;
    let area = PhysicalMemory.bytes(BASE, 0x10000)?;
    let scan = |anchor: &[u8]| {
        (0..area.len()).step_by(16).find_map(|at| {
            let rest = &area[at..];
            if !rest.starts_with(anchor) {
                return None;
            }
            Some((BASE + at as u64, Entry::parse(&rest[..rest.len().min(MAX_ENTRY)])?))
        })
    };
    scan(ANCHOR_64).or_else(|| scan(ANCHOR_32))
}

/// Whether any of `range` is above the first MiB, which the kernel keeps
/// for the firmware, in memory the memory map calls usable.
fn in_free_memory(range: core::ops::Range<u64>) -> bool {
    memory::memory_map().iter().any(|region| {
        region.kind == MemoryKind::Usable
            && region.start.max(1 << 20).max(range.start) < region.end.min(range.end)
    })
}
