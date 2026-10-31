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
    for device in table.of(kind::MEMORY_DEVICE).filter_map(|s| MemoryDevice::parse(&s)) {
        slots += 1;
        filled += usize::from(device.is_present());
    }
    if slots > 0 {
        info!("DMI: Memory slots populated: {filled}/{slots}");
    }
    let _ = IDS.set(ids);
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
