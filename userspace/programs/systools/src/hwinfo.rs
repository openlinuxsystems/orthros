// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! hwinfo(8): what the machine is, as its firmware's SMBIOS tables say:
//! its maker and model, its board, chassis and firmware, its processors'
//! sockets and its memory devices (ADR 0231).
//!
//! The tables come whole from the kernel through the platform capability
//! in SLOT_PLATFORM (radix-platform(2)'s `smbios`), which a program has
//! when it was given admin.hardware, as dmidecode reads Linux's
//! `/sys/firmware/dmi/tables` as root. Without it hwinfo shows what anyone
//! may read in `/kernel/class/dmi/id`: the machine, board, chassis and
//! firmware, without the processors and memory. Serial numbers and the
//! UUID are left out unless asked for (`-s`), so that the output can be
//! shared as it is.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use radix_abi::Status;
use radix_abi::errors::Kernel;
use radix_abi::platform::{self, SmbiosTable};
use radix_abi::startup::SLOT_PLATFORM;
use radix_smbios::memory::{self, MemoryArray, MemoryDevice};
use radix_smbios::processor::Processor;
use radix_smbios::{Entry, Ids, Table, chassis_name, kind};

use crate::args::{Arg, Args};
use crate::sys::SYS;
use crate::util::{Fail, fail, usage};

/// Where the kernel shows what anyone may read of the tables.
const DMI: &str = "/kernel/class/dmi/id";

/// The sections, in the order they are shown.
const SECTIONS: [&str; 6] = ["system", "board", "chassis", "firmware", "processor", "memory"];

/// The width of a line's label, with its colon.
const LABEL: usize = 18;

/// Why there is nothing to show.
const NO_TABLES: &str = "the firmware gives no SMBIOS tables";

/// What needs the tables whole.
const NEEDS_CAPABILITY: &str =
    "this program holds no platform capability: it needs admin.hardware (spark(8))";

/// The tables, as the kernel gives them.
pub struct Tables {
    /// The entry point.
    entry: Vec<u8>,
    /// The structures.
    structures: Vec<u8>,
}

/// Reads them through the capability in SLOT_PLATFORM.
fn read_tables() -> Result<Tables, Status> {
    let read = |table| -> Result<Vec<u8>, Status> {
        let mut buffer = vec![0; 4096];
        loop {
            let length = platform::smbios(&SYS, SLOT_PLATFORM, table, &mut buffer)?.length;
            let length = usize::try_from(length).unwrap_or(usize::MAX);
            if length <= buffer.len() {
                buffer.truncate(length);
                return Ok(buffer);
            }
            buffer.resize(length, 0);
        }
    };
    Ok(Tables { entry: read(SmbiosTable::EntryPoint)?, structures: read(SmbiosTable::Structures)? })
}

/// The fields of `/kernel/class/dmi/id` there are, by name.
type Fields = Vec<(&'static str, String)>;

/// The names Linux gives the fields of `/sys/class/dmi/id`, which
/// [`Ids::fields`] gives too.
const FILES: [&str; 22] = [
    "bios_date",
    "bios_release",
    "bios_vendor",
    "bios_version",
    "board_asset_tag",
    "board_name",
    "board_serial",
    "board_vendor",
    "board_version",
    "chassis_asset_tag",
    "chassis_serial",
    "chassis_type",
    "chassis_vendor",
    "chassis_version",
    "ec_firmware_release",
    "product_family",
    "product_name",
    "product_serial",
    "product_sku",
    "product_uuid",
    "product_version",
    "sys_vendor",
];

/// The fields in `dir`, as the kernel shows them.
fn read_fields(dir: &Path) -> Fields {
    FILES
        .iter()
        .filter_map(|&name| {
            let text = fs::read(dir.join(name)).ok()?;
            let text = String::from_utf8_lossy(&text).trim_end_matches('\n').to_string();
            Some((name, text))
        })
        .collect()
}

/// A line of a section: its label and value.
struct Section {
    title: &'static str,
    lines: Vec<(String, String)>,
}

impl Section {
    fn new(title: &'static str) -> Section {
        Section { title, lines: Vec::new() }
    }

    fn add(&mut self, label: impl Into<String>, value: impl Into<String>) {
        self.lines.push((label.into(), value.into()));
    }

    /// Adds the field `name` of `fields`, if there is one.
    fn field(&mut self, label: &str, fields: &Fields, name: &str) {
        if let Some((_, value)) = fields.iter().find(|(n, _)| *n == name) {
            self.add(label, value.clone());
        }
    }
}

/// `bytes` in the largest unit that holds it whole.
fn size(bytes: u64) -> String {
    let units = [(40, "TiB"), (30, "GiB"), (20, "MiB"), (10, "KiB")];
    for (shift, unit) in units {
        if bytes >= 1 << shift && bytes.is_multiple_of(1 << shift) {
            return format!("{} {unit}", bytes >> shift);
        }
    }
    format!("{bytes} B")
}

/// `count` of `what`, plural but for one.
fn count(count: u16, what: &str) -> String {
    format!("{count} {what}{}", if count == 1 { "" } else { "s" })
}

/// A processor socket's line.
fn processor(p: &Processor<'_>) -> String {
    if !p.populated {
        return "empty".to_string();
    }
    let mut parts = Vec::new();
    if let Some(name) = p.version.or(p.manufacturer) {
        parts.push(name.to_string());
    }
    if let Some(cores) = p.cores {
        let mut text = count(cores, "core");
        if let Some(enabled) = p.enabled.filter(|&enabled| enabled < cores) {
            let _ = write!(text, " ({enabled} enabled)");
        }
        parts.push(text);
    }
    if let Some(threads) = p.threads {
        parts.push(count(threads, "thread"));
    }
    match (p.current_speed, p.max_speed) {
        (Some(now), Some(max)) if now != max => parts.push(format!("{now} MHz, up to {max} MHz")),
        (Some(mhz), _) | (None, Some(mhz)) => parts.push(format!("{mhz} MHz")),
        (None, None) => {}
    }
    parts.join(", ")
}

/// A memory device's label, and its line.
fn memory_device(d: &MemoryDevice<'_>, serials: bool) -> (String, String) {
    let mut label = d.locator.map_or_else(|| "Memory device".to_string(), |l| l.to_string());
    if let Some(bank) = d.bank {
        let _ = write!(label, " ({bank})");
    }
    if !d.is_present() {
        return (label, "empty".to_string());
    }
    let mut parts = Vec::new();
    let what = format!("{} {}", memory::kind_name(d.kind), memory::form_name(d.form));
    parts.push(match d.size {
        Some(bytes) => format!("{} {what}", size(bytes)),
        None => format!("{what} of unknown size"),
    });
    match (d.speed, d.configured_speed) {
        (Some(max), Some(set)) if set != max => parts.push(format!("{max} MT/s, set to {set}")),
        (Some(speed), _) | (None, Some(speed)) => parts.push(format!("{speed} MT/s")),
        (None, None) => {}
    }
    let maker: Vec<String> =
        [d.manufacturer, d.part_number].into_iter().flatten().map(|s| s.to_string()).collect();
    if !maker.is_empty() {
        parts.push(maker.join(" "));
    }
    if serials && let Some(serial) = d.serial {
        parts.push(format!("serial number {serial}"));
    }
    (label, parts.join(", "))
}

/// The sections `fields` and, if they were read, the tables give.
fn sections(fields: &Fields, tables: Option<(&Entry, &Table<'_>)>, serials: bool) -> Vec<Section> {
    let mut system = Section::new("System");
    system.field("Manufacturer", fields, "sys_vendor");
    system.field("Product", fields, "product_name");
    system.field("Version", fields, "product_version");
    system.field("Family", fields, "product_family");
    system.field("SKU", fields, "product_sku");
    system.field("Serial number", fields, "product_serial");
    system.field("UUID", fields, "product_uuid");
    let mut board = Section::new("Board");
    board.field("Manufacturer", fields, "board_vendor");
    board.field("Product", fields, "board_name");
    board.field("Version", fields, "board_version");
    board.field("Serial number", fields, "board_serial");
    board.field("Asset tag", fields, "board_asset_tag");
    let mut chassis = Section::new("Chassis");
    if let Some((_, kind)) = fields.iter().find(|(name, _)| *name == "chassis_type") {
        let name = kind.parse().ok().and_then(chassis_name);
        chassis.add("Type", name.map_or_else(|| format!("type {kind}"), str::to_string));
    }
    chassis.field("Manufacturer", fields, "chassis_vendor");
    chassis.field("Version", fields, "chassis_version");
    chassis.field("Serial number", fields, "chassis_serial");
    chassis.field("Asset tag", fields, "chassis_asset_tag");
    let mut firmware = Section::new("Firmware");
    firmware.field("Vendor", fields, "bios_vendor");
    firmware.field("Version", fields, "bios_version");
    firmware.field("Date", fields, "bios_date");
    firmware.field("Release", fields, "bios_release");
    firmware.field("EC release", fields, "ec_firmware_release");
    let mut processors = Section::new("Processor");
    let mut memory = Section::new("Memory");
    if let Some((entry, table)) = tables {
        let version = entry.to_string();
        firmware.add("SMBIOS", version.trim_start_matches("SMBIOS "));
        for (n, p) in table.of(kind::PROCESSOR).filter_map(|s| Processor::parse(&s)).enumerate() {
            let label = p.socket.map_or_else(|| format!("Socket {n}"), |s| s.to_string());
            processors.add(label, processor(&p));
        }
        let devices: Vec<_> =
            table.of(kind::MEMORY_DEVICE).filter_map(|s| MemoryDevice::parse(&s)).collect();
        let arrays: Vec<_> =
            table.of(kind::MEMORY_ARRAY).filter_map(|s| MemoryArray::parse(&s)).collect();
        if !devices.is_empty() {
            let present = devices.iter().filter(|d| d.is_present()).count();
            let total: u64 = devices.iter().filter_map(|d| d.size).sum();
            memory.add(
                "Installed",
                format!("{} in {present} of {} slots", size(total), devices.len()),
            );
        }
        // The arrays of system memory, where most machines have one.
        let capacity: u64 = arrays.iter().filter_map(|a| a.capacity).sum();
        if capacity > 0 {
            memory.add("Capacity", size(capacity));
        }
        if let Some(array) = arrays.first() {
            memory.add("Error correction", memory::ecc_name(array.ecc));
        }
        for device in &devices {
            let (label, line) = memory_device(device, serials);
            memory.add(label, line);
        }
    }
    vec![system, board, chassis, firmware, processors, memory]
}

/// Prints the sections `wanted` (all if none) that have lines, from
/// `tables` if the kernel gave them, else from the fields in `dmi`.
pub fn run(
    tables: &Result<Tables, Status>,
    dmi: &Path,
    wanted: &[&str],
    serials: bool,
    out: &mut dyn Write,
) -> Result<i32, Fail> {
    let needs_tables = serials || wanted.iter().any(|w| matches!(*w, "processor" | "memory"));
    let parsed = match tables {
        Ok(tables) => Entry::parse(&tables.entry).map(|entry| {
            let table = Table::new(&tables.structures, entry.count);
            (entry, table)
        }),
        Err(status) => {
            match Kernel::from_status(*status) {
                Some(Kernel::NotSupported) => return Err(fail(NO_TABLES.to_string())),
                Some(Kernel::NoSuchSlot | Kernel::EmptySlot | Kernel::WrongKind)
                    if needs_tables =>
                {
                    return Err(fail(NEEDS_CAPABILITY.to_string()));
                }
                Some(Kernel::MissingRights) if needs_tables => {
                    return Err(fail("its platform capability does not allow this".to_string()));
                }
                _ => {}
            }
            None
        }
    };
    let fields: Fields = match &parsed {
        Some((entry, table)) => Ids::read(table, entry.version())
            .fields(serials)
            .map(|(name, value)| (name, value.to_string()))
            .collect(),
        None => read_fields(dmi),
    };
    if fields.is_empty() && parsed.is_none() {
        return Err(fail(NO_TABLES.to_string()));
    }
    let mut first = true;
    for section in sections(&fields, parsed.as_ref().map(|(e, t)| (e, t)), serials) {
        let name = section.title.to_ascii_lowercase();
        if section.lines.is_empty() || !(wanted.is_empty() || wanted.contains(&name.as_str())) {
            continue;
        }
        let mut text = String::new();
        if !first {
            text.push('\n');
        }
        first = false;
        let _ = writeln!(text, "{}", section.title);
        for (label, value) in &section.lines {
            let label = format!("{label}:");
            let _ = writeln!(text, "  {label:<LABEL$}{value}");
        }
        out.write_all(text.as_bytes())?;
    }
    if parsed.is_none() && wanted.is_empty() {
        eprintln!(
            "hwinfo: the processors and memory need the SMBIOS tables whole: {NEEDS_CAPABILITY}"
        );
    }
    Ok(0)
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut serials = false;
    let mut wanted = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('s') => serials = true,
            Arg::Long(ref name) if name == "serials" => serials = true,
            Arg::Short('h') => {
                return Err(usage(format!(
                    "Usage: hwinfo [-s] [section...]\n  -s, --serials  with the serial numbers \
                     and the UUID\n  sections: {}",
                    SECTIONS.join(", ")
                )));
            }
            Arg::Operand(section) => match SECTIONS.iter().find(|&&s| *s == *section) {
                Some(&s) => wanted.push(s),
                None => {
                    let section = section.to_string_lossy();
                    let sections = SECTIONS.join(", ");
                    return Err(usage(format!("no section `{section}`: they are {sections}")));
                }
            },
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    run(&read_tables(), Path::new(DMI), &wanted, serials, &mut out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A structure of type `kind`, as the firmware lays it out.
    fn structure(kind: u8, fields: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut bytes = vec![kind, (4 + fields.len()) as u8, 0, 0];
        bytes.extend_from_slice(fields);
        for string in strings {
            bytes.extend_from_slice(string.as_bytes());
            bytes.push(0);
        }
        if strings.is_empty() {
            bytes.push(0);
        }
        bytes.push(0);
        bytes
    }

    /// A 64-bit entry point of SMBIOS 3.0.0.
    fn entry() -> Vec<u8> {
        let mut e = b"_SM3_".to_vec();
        e.extend_from_slice(&[0, 0x18, 3, 0, 0, 1, 0]);
        e.extend_from_slice(&0x1000u32.to_le_bytes());
        e.extend_from_slice(&0x7f00_0000u64.to_le_bytes());
        let sum = e.iter().fold(0u8, |s, b| s.wrapping_add(*b));
        e[5] = 0u8.wrapping_sub(sum);
        e
    }

    /// A laptop's tables: two slots, one of them filled.
    fn laptop() -> Tables {
        let mut t = Vec::new();
        let mut bios = vec![1, 2, 0, 0xf0, 3, 0xff];
        bios.extend_from_slice(&[0; 8]);
        bios.extend_from_slice(&[0, 0, 1, 33, 1, 2]);
        t.extend(structure(0, &bios, &["Dell Inc.", "1.33.0", "01/15/2025"]));
        let mut system = vec![1, 2, 0, 3];
        system.extend_from_slice(&[
            0x44, 0x45, 0x4c, 0x4c, 0x42, 0x00, 0x10, 0x39, 0x80, 0x51, 0xb7, 0xc0, 0x4f, 0x4e,
            0x33, 0x32,
        ]);
        system.extend_from_slice(&[6, 4, 5]);
        t.extend(structure(
            1,
            &system,
            &["Dell Inc.", "Inspiron 5570", "4XB9NM2", "0811", "Inspiron"],
        ));
        t.extend(structure(3, &[1, 0x0a, 0, 2, 0], &["Dell Inc.", "4XB9NM2"]));
        let mut cpu = vec![1, 3, 0xc6, 2];
        cpu.extend_from_slice(&[0; 8]);
        cpu.extend_from_slice(&[3, 0, 0, 0]);
        cpu.extend_from_slice(&4000u16.to_le_bytes());
        cpu.extend_from_slice(&1800u16.to_le_bytes());
        cpu.extend_from_slice(&[0x41, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 4, 4, 8]);
        t.extend(structure(
            4,
            &cpu,
            &["U3E1", "Intel(R) Corporation", "Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz"],
        ));
        let mut array = vec![3, 3, 3];
        array.extend_from_slice(&(32u32 << 20).to_le_bytes());
        array.extend_from_slice(&[0xfe, 0xff, 2, 0]);
        t.extend(structure(16, &array, &[]));
        let dimm = |size: u16, strings: bool| {
            let mut d = vec![0x00, 0x10, 0xfe, 0xff, 64, 0, 64, 0];
            d.extend_from_slice(&size.to_le_bytes());
            d.extend_from_slice(&[0x0d, 0, 1, 2, 0x1a, 0x80, 0]);
            d.extend_from_slice(&2400u16.to_le_bytes());
            d.extend_from_slice(if strings { &[3, 4, 0, 5, 1] } else { &[0, 0, 0, 0, 0] });
            d
        };
        t.extend(structure(
            17,
            &dimm(8192, true),
            &["DIMM A", "BANK 0", "Samsung", "12345678", "M471A1K43CB1-CRC"],
        ));
        t.extend(structure(17, &dimm(0, false), &["DIMM B", "BANK 2"]));
        t.extend(structure(127, &[], &[]));
        Tables { entry: entry(), structures: t }
    }

    fn output(
        tables: &Result<Tables, Status>,
        dmi: &Path,
        wanted: &[&str],
        serials: bool,
    ) -> (i32, String) {
        let mut out = Vec::new();
        let status = run(tables, dmi, wanted, serials, &mut out).unwrap_or(1);
        (status, String::from_utf8(out).unwrap())
    }

    #[test]
    fn everything_from_the_tables() {
        let (status, text) = output(&Ok(laptop()), Path::new("/nonexistent"), &[], false);
        assert_eq!(status, 0);
        assert_eq!(
            text,
            "System\n  Manufacturer:     Dell Inc.\n  Product:          Inspiron 5570\n  \
             Family:           Inspiron\n  SKU:              0811\n\nChassis\n  Type:             \
             Notebook\n  Manufacturer:     Dell Inc.\n\nFirmware\n  Vendor:           Dell \
             Inc.\n  Version:          1.33.0\n  Date:             01/15/2025\n  Release:          \
             1.33\n  EC release:       1.2\n  SMBIOS:           3.0.0\n\nProcessor\n  U3E1:             \
             Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz, 4 cores, 8 threads, 1800 MHz, up to 4000 \
             MHz\n\nMemory\n  Installed:        8 GiB in 1 of 2 slots\n  Capacity:         32 \
             GiB\n  Error correction: None\n  DIMM A (BANK 0):  8 GiB DDR4 SODIMM, 2400 MT/s, \
             Samsung M471A1K43CB1-CRC\n  DIMM B (BANK 2):  empty\n"
        );
        let (_, text) =
            output(&Ok(laptop()), Path::new("/nonexistent"), &["system", "memory"], true);
        assert!(text.contains("  Serial number:    4XB9NM2\n  UUID:             4c4c4544-0042-3910-8051-b7c04f4e3332\n"));
        assert!(text.contains("Samsung M471A1K43CB1-CRC, serial number 12345678\n"));
        assert!(!text.contains("Firmware"));
    }

    #[test]
    fn without_the_capability() {
        let dir = std::env::temp_dir().join(format!("hwinfo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for (name, value) in [
            ("sys_vendor", "QEMU"),
            ("product_name", "Standard PC (Q35 + ICH9, 2009)"),
            ("chassis_type", "1"),
            ("bios_vendor", "SeaBIOS"),
        ] {
            fs::write(dir.join(name), format!("{value}\n")).unwrap();
        }
        let none = Kernel::NoSuchSlot.status(0);
        let (status, text) = output(&Err(none), &dir, &[], false);
        assert_eq!(status, 0);
        assert_eq!(
            text,
            "System\n  Manufacturer:     QEMU\n  Product:          Standard PC (Q35 + ICH9, 2009)\n\n\
             Chassis\n  Type:             Other\n\nFirmware\n  Vendor:           SeaBIOS\n"
        );
        // What needs the tables whole fails without them.
        assert_eq!(output(&Err(none), &dir, &["memory"], false).0, 1);
        assert_eq!(output(&Err(none), &dir, &[], true).0, 1);
        let missing = Kernel::NotSupported.status(0);
        assert_eq!(output(&Err(missing), &dir, &[], false).0, 1);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(output(&Err(none), &dir, &[], false).0, 1);
    }

    #[test]
    fn sizes_and_counts() {
        assert_eq!(size(16 << 30), "16 GiB");
        assert_eq!(size(1536 << 20), "1536 MiB");
        assert_eq!(size(512), "512 B");
        assert_eq!(count(1, "core"), "1 core");
        assert_eq!(count(16, "thread"), "16 threads");
    }
}
