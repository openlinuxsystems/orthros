// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The firmware's SMBIOS tables (Phase 11e, ADR 0231): what the machine
//! is, as its maker names it.
//!
//! At boot the kernel logs the tables' version, the machine as Linux does
//! (`DMI: <maker> <product>/<board>, BIOS <version> <date>`) and how many
//! of its memory slots are filled, and keeps what Linux keeps for
//! `/sys/class/dmi/id`, which `/kernel/class/dmi/id` shows ([`files`]).
//! The serial numbers and the UUID are not there: Linux shows them to
//! root alone, and the VFS has no owners, so they are read with the
//! tables whole through the platform capability's `smbios`, as dmidecode
//! reads Linux's `/sys/firmware/dmi/tables`.

use core::fmt;

use radix_hal::{Platform, Smbios};
use radix_klog::info;
use radix_smbios::memory::MemoryDevice;
use radix_smbios::{Entry, Field, Ids, Str, Table, kind};
use radix_sync::Once;

use crate::Arch;

static IDS: Once<Ids<'static>> = Once::new();
static MEMORY: Once<Memory> = Once::new();

/// The memory devices the tables list, as the SMBus's SPD EEPROMs are
/// found by them (ADR 0232).
#[derive(Clone, Copy, Debug)]
pub struct Memory {
    /// The devices listed: the slots, filled or not, or soldered chips.
    pub slots: usize,
    /// The filled ones whose kind the tables say.
    pub modules: usize,
    /// Their kind (DSP0134's 7.18.2), if they are all of one.
    pub kind: Option<u8>,
}

/// Reads the tables the platform found, and logs what they say.
pub fn init() {
    let Some(Smbios { entry, table }) = Arch::smbios() else {
        info!("SMBIOS: no tables the kernel can use");
        return;
    };
    let Some(entry) = Entry::parse(entry) else { return };
    info!("{entry} present.");
    let table = Table::new(table, entry.count);
    let ids = Ids::read(&table, entry.version());
    info!("DMI: {}", Machine(&ids));
    let (mut slots, mut filled) = (0, 0);
    let mut memory = Memory { slots: 0, modules: 0, kind: None };
    let mut mixed = false;
    for device in table.of(kind::MEMORY_DEVICE).filter_map(|s| MemoryDevice::parse(&s)) {
        slots += 1;
        filled += usize::from(device.is_present());
        // Kinds 1 and 2 are "Other" and "Unknown".
        if device.is_present() && device.kind > 2 {
            memory.modules += 1;
            mixed |= memory.kind.is_some_and(|kind| kind != device.kind);
            memory.kind = Some(device.kind);
        }
    }
    if slots > 0 {
        info!("DMI: Memory slots populated: {filled}/{slots}");
    }
    memory.slots = slots;
    if mixed {
        memory.kind = None;
    }
    let _ = IDS.set(ids);
    let _ = MEMORY.set(memory);
}

/// The memory devices the tables list, if there are tables.
pub fn memory() -> Option<Memory> {
    MEMORY.get().copied()
}

/// Whether there are tables, so `/kernel/class/dmi` has files.
pub fn found() -> bool {
    IDS.get().is_some()
}

/// Calls `file` for each file of `/kernel/class/dmi/id` there is, as
/// Linux names them, in the order of their names: without the serial
/// numbers and the UUID.
pub fn files(file: &mut dyn FnMut(&'static str, Field<'static>)) {
    let Some(ids) = IDS.get() else { return };
    for (name, field) in ids.fields(false) {
        file(name, field);
    }
}

/// The machine, as Linux's boot line names it.
struct Machine<'a>(&'a Ids<'static>);

impl fmt::Display for Machine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ids = self.0;
        let or_none = |s: Option<Str<'static>>| s.unwrap_or(Str(b""));
        write!(f, "{} {}", or_none(ids.sys_vendor), or_none(ids.product_name))?;
        if let Some(board) = ids.board_name {
            write!(f, "/{board}")?;
        }
        write!(f, ", BIOS {} {}", or_none(ids.bios_version), or_none(ids.bios_date))
    }
}
