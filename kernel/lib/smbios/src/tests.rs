// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::memory::{self, MemoryArray, MemoryDevice};
use super::processor::Processor;
use super::*;

/// A structure of type `kind`: its formatted area after the header, then
/// its strings.
pub(crate) fn structure(kind: u8, handle: u16, fields: &[u8], strings: &[&str]) -> Vec<u8> {
    let mut bytes = vec![kind, (4 + fields.len()) as u8];
    bytes.extend_from_slice(&handle.to_le_bytes());
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

/// The table QEMU's q35 gives SeaBIOS, cut to what the tests read, with
/// `-smbios type=1,serial=QEMU-1234`.
pub(crate) fn qemu() -> Vec<u8> {
    let mut t = Vec::new();
    // BIOS: vendor 1, version 2, segment, date 3, ROM size, characteristics,
    // extension bytes, then the BIOS's release 0.0 and no EC.
    let mut bios = vec![1, 2, 0x00, 0xe8, 3, 0x00];
    bios.extend_from_slice(&[0x08, 0, 0, 0, 0, 0, 0, 0]);
    bios.extend_from_slice(&[0x00, 0x04, 0x00, 0x00, 0xff, 0xff]);
    t.extend(structure(kind::BIOS, 0, &bios, &["SeaBIOS", "1.16.3-debian-1.16.3-2", "04/01/2014"]));
    // System: manufacturer 1, product 2, version 3, serial 4, a UUID,
    // wake-up type, no SKU, no family.
    let mut system = vec![1, 2, 3, 4];
    system.extend_from_slice(&[
        0x44, 0x45, 0x4c, 0x4c, 0x42, 0x00, 0x10, 0x39, 0x80, 0x51, 0xb7, 0xc0, 0x4f, 0x4e, 0x33,
        0x32,
    ]);
    system.extend_from_slice(&[0x06, 0, 0]);
    t.extend(structure(
        kind::SYSTEM,
        0x100,
        &system,
        &["QEMU", "Standard PC (Q35 + ICH9, 2009)", "pc-q35-8.2", "QEMU-1234"],
    ));
    // Chassis: manufacturer 1, type 1 (Other), no version, serial or tag.
    t.extend(structure(kind::CHASSIS, 0x300, &[1, 0x01, 0, 0, 0, 3, 3, 3, 2], &["QEMU"]));
    // Processor: socket 1, central, family, manufacturer 2, an ID, version
    // 3, voltage, clocks, populated and enabled, 1 core of 1, 1 thread.
    let mut cpu = vec![1, 0x03, 0xfe, 2];
    cpu.extend_from_slice(&[0; 8]);
    cpu.extend_from_slice(&[3, 0, 0, 0]);
    cpu.extend_from_slice(&2000u16.to_le_bytes());
    cpu.extend_from_slice(&2000u16.to_le_bytes());
    cpu.extend_from_slice(&[0x41, 0x01]);
    cpu.extend_from_slice(&[0xff; 6]);
    cpu.extend_from_slice(&[0, 0, 0]);
    cpu.extend_from_slice(&[1, 1, 1]);
    cpu.extend_from_slice(&[0x02, 0x00, 0xfe, 0x00]);
    t.extend(structure(kind::PROCESSOR, 0x400, &cpu, &["CPU 0", "QEMU", "pc-q35-8.2"]));
    // A memory array of 1 GiB in one device, no ECC.
    let mut array = vec![0x01, 0x03, 0x06];
    array.extend_from_slice(&(1u32 << 20).to_le_bytes());
    array.extend_from_slice(&0xfffeu16.to_le_bytes());
    array.extend_from_slice(&1u16.to_le_bytes());
    t.extend(structure(kind::MEMORY_ARRAY, 0x1000, &array, &[]));
    // Its device: 1024 MiB of RAM, a DIMM, locator 1, manufacturer 2.
    let mut dimm = Vec::new();
    dimm.extend_from_slice(&0x1000u16.to_le_bytes());
    dimm.extend_from_slice(&0xfffeu16.to_le_bytes());
    dimm.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
    dimm.extend_from_slice(&1024u16.to_le_bytes());
    dimm.extend_from_slice(&[0x09, 0, 1, 0, 0x07]);
    dimm.extend_from_slice(&0x0002u16.to_le_bytes());
    dimm.extend_from_slice(&0u16.to_le_bytes());
    dimm.extend_from_slice(&[2, 0, 0, 0, 0]);
    t.extend(structure(kind::MEMORY_DEVICE, 0x1100, &dimm, &["DIMM 0", "QEMU"]));
    t.extend(structure(kind::END, 0x7f00, &[], &[]));
    t
}

/// A Framework Desktop's table as SMBIOS 3.6 gives it, cut down: its
/// system, board and chassis, and one of its eight LPDDR5x devices.
pub(crate) fn desktop() -> Vec<u8> {
    let mut t = Vec::new();
    let mut bios = vec![1, 2, 0x00, 0xf0, 3, 0xff];
    bios.extend_from_slice(&[0; 8]);
    bios.extend_from_slice(&[0x03, 0x0d, 0x03, 0x02, 0x00, 0x13]);
    bios.extend_from_slice(&[0x10, 0x00]);
    t.extend(structure(kind::BIOS, 0, &bios, &["INSYDE Corp.", "03.02", "06/10/2025"]));
    let mut system = vec![1, 2, 3, 4];
    system.extend_from_slice(&[0xff; 16]);
    system.extend_from_slice(&[0x06, 5, 6]);
    t.extend(structure(
        kind::SYSTEM,
        1,
        &system,
        &[
            "Framework",
            "Desktop (AMD Ryzen AI Max 300 Series)",
            "A6",
            "FRAKDESKTOP0001",
            "FRAMDACP06",
            "Desktop",
        ],
    ));
    t.extend(structure(
        kind::BOARD,
        2,
        &[1, 2, 3, 4, 5, 0x09, 0, 0, 0x0a, 0],
        &["Framework", "FRANMFCP06", "A6", "FRANMFCP06000001", "   "],
    ));
    t.extend(structure(
        kind::CHASSIS,
        3,
        &[1, 0x03, 2, 3, 0, 3, 3, 3, 3],
        &["Framework", "A6", "FRAKDESKTOP0001"],
    ));
    // An LPDDR5 device of 16 GiB in the extended size field, a row of
    // chips at 8000 MT/s, configured to the same, rank 2, 500 mV.
    let mut lp = Vec::new();
    lp.extend_from_slice(&0x20u16.to_le_bytes());
    lp.extend_from_slice(&0xfffeu16.to_le_bytes());
    lp.extend_from_slice(&32u16.to_le_bytes());
    lp.extend_from_slice(&32u16.to_le_bytes());
    lp.extend_from_slice(&0x7fffu16.to_le_bytes());
    lp.extend_from_slice(&[0x0b, 0, 1, 2, 0x23]);
    lp.extend_from_slice(&0x4080u16.to_le_bytes());
    lp.extend_from_slice(&8000u16.to_le_bytes());
    lp.extend_from_slice(&[3, 4, 0, 5, 0x02]);
    lp.extend_from_slice(&16384u32.to_le_bytes());
    lp.extend_from_slice(&8000u16.to_le_bytes());
    lp.extend_from_slice(&500u16.to_le_bytes());
    lp.extend_from_slice(&500u16.to_le_bytes());
    lp.extend_from_slice(&500u16.to_le_bytes());
    t.extend(structure(
        kind::MEMORY_DEVICE,
        0x21,
        &lp,
        &["DIMM 0", "P0 CHANNEL A", "Micron Technology", "00000000", "MT62F2G32D4DS-023 WT"],
    ));
    // A slot left empty.
    let mut empty = vec![0x20, 0x00, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0x0b, 0, 1, 2, 0x02];
    empty.extend_from_slice(&[0; 14]);
    t.extend(structure(kind::MEMORY_DEVICE, 0x22, &empty, &["DIMM 1", "P0 CHANNEL B"]));
    t.extend(structure(kind::END, 0x7f00, &[], &[]));
    t
}

/// A 64-bit entry point for a table at `address` of at most `length`.
fn entry_64(major: u8, minor: u8, docrev: u8, address: u64, length: u32) -> Vec<u8> {
    let mut e = b"_SM3_".to_vec();
    e.extend_from_slice(&[0, 0x18, major, minor, docrev, 1, 0]);
    e.extend_from_slice(&length.to_le_bytes());
    e.extend_from_slice(&address.to_le_bytes());
    let sum = e.iter().fold(0u8, |s, b| s.wrapping_add(*b));
    e[5] = 0u8.wrapping_sub(sum);
    e
}

/// A 32-bit entry point, likewise, with its `_DMI_` part.
fn entry_32(major: u8, minor: u8, address: u32, length: u16, count: u16) -> Vec<u8> {
    let mut e = b"_SM_".to_vec();
    e.extend_from_slice(&[0, 0x1f, major, minor, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
    e.extend_from_slice(b"_DMI_");
    e.push(0);
    e.extend_from_slice(&length.to_le_bytes());
    e.extend_from_slice(&address.to_le_bytes());
    e.extend_from_slice(&count.to_le_bytes());
    e.push(0x28);
    let dmi = e[0x10..].iter().fold(0u8, |s, b| s.wrapping_add(*b));
    e[0x15] = 0u8.wrapping_sub(dmi);
    let sum = e.iter().fold(0u8, |s, b| s.wrapping_add(*b));
    e[4] = 0u8.wrapping_sub(sum);
    e
}

#[test]
fn entry_points_of_both_forms() {
    let e = Entry::parse(&entry_64(3, 7, 0, 0x7f8f_6000, 0x2b0)).unwrap();
    assert_eq!(
        (e.major, e.minor, e.address, e.length, e.count, e.size),
        (3, 7, 0x7f8f_6000, 0x2b0, None, 0x18)
    );
    assert_eq!(e.to_string(), "SMBIOS 3.7.0");
    assert_eq!(e.version(), 0x030700);
    let e = Entry::parse(&entry_32(2, 8, 0xf_6e40, 0x1a3, 9)).unwrap();
    assert_eq!((e.address, e.length, e.count, e.size), (0xf_6e40, 0x1a3, Some(9), 0x1f));
    assert_eq!(e.to_string(), "SMBIOS 2.8");
    // Versions Linux mends.
    assert_eq!(Entry::parse(&entry_32(2, 0x33, 0, 0, 0)).unwrap().to_string(), "SMBIOS 2.6");
    assert_eq!(Entry::parse(&entry_32(2, 0x1f, 0, 0, 0)).unwrap().version(), 0x020300);
    // A wrong checksum, of the whole or of `_DMI_`, or anchor.
    let mut bad = entry_64(3, 0, 0, 0, 0);
    bad[0x10] ^= 1;
    assert_eq!(Entry::parse(&bad), None);
    let mut bad = entry_32(2, 8, 0, 0, 0);
    bad[0x16] ^= 1;
    bad[4] ^= 1;
    assert_eq!(Entry::parse(&bad), None);
    assert_eq!(Entry::parse(b"_SM4_ and more bytes than an entry point has"), None);
    assert_eq!(Entry::parse(b"_SM3_"), None);
}

#[test]
fn ids_as_linux_keeps_them() {
    let bytes = qemu();
    let table = Table::new(&bytes, Some(8));
    let ids = Ids::read(&table, 0x020800);
    let text = |s: Option<Str<'_>>| s.map(|s| s.to_string());
    assert_eq!(text(ids.bios_vendor).as_deref(), Some("SeaBIOS"));
    assert_eq!(text(ids.bios_version).as_deref(), Some("1.16.3-debian-1.16.3-2"));
    assert_eq!(text(ids.bios_date).as_deref(), Some("04/01/2014"));
    assert_eq!(ids.bios_release, Some(Release(0, 0)));
    assert_eq!(ids.ec_firmware_release, None);
    assert_eq!(text(ids.sys_vendor).as_deref(), Some("QEMU"));
    assert_eq!(text(ids.product_name).as_deref(), Some("Standard PC (Q35 + ICH9, 2009)"));
    assert_eq!(text(ids.product_serial).as_deref(), Some("QEMU-1234"));
    assert_eq!(ids.product_uuid.unwrap().to_string(), "4c4c4544-0042-3910-8051-b7c04f4e3332");
    assert_eq!(ids.product_sku, None);
    assert_eq!(ids.board_name, None);
    assert_eq!(text(ids.chassis_vendor).as_deref(), Some("QEMU"));
    assert_eq!(ids.chassis_type, Some(1));
    assert_eq!(ids.chassis_version, None);
    let fields: Vec<_> = ids.fields(false).map(|(name, f)| format!("{name}={f}")).collect();
    assert_eq!(
        fields,
        [
            "bios_date=04/01/2014",
            "bios_release=0.0",
            "bios_vendor=SeaBIOS",
            "bios_version=1.16.3-debian-1.16.3-2",
            "chassis_type=1",
            "chassis_vendor=QEMU",
            "product_name=Standard PC (Q35 + ICH9, 2009)",
            "product_version=pc-q35-8.2",
            "sys_vendor=QEMU",
        ]
    );
    let private: Vec<_> =
        ids.fields(true).filter(|(name, _)| name.starts_with("product_")).collect();
    assert_eq!(private.len(), 4);
    assert_eq!(private[1].1.to_string(), "QEMU-1234");
    assert_eq!(private[2].1.to_string(), "4c4c4544-0042-3910-8051-b7c04f4e3332");
    // Before SMBIOS 2.6 the UUID's fields are big-endian.
    let old = Ids::read(&table, 0x020500);
    assert_eq!(old.product_uuid.unwrap().to_string(), "44454c4c-4200-1039-8051-b7c04f4e3332");
    assert_eq!(table.used(), bytes.len());
    assert_eq!(table.structures().count(), 7);
    assert_eq!(chassis_name(1), Some("Other"));
    assert_eq!(chassis_name(3), Some("Desktop"));
    assert_eq!(chassis_name(0x24), Some("Stick PC"));
    assert_eq!(chassis_name(0), None);

    let bytes = desktop();
    let ids = Ids::read(&Table::new(&bytes, None), 0x030600);
    assert_eq!(ids.bios_release, Some(Release(3, 2)));
    assert_eq!(ids.ec_firmware_release, Some(Release(0, 0x13)));
    assert_eq!(text(ids.product_sku).as_deref(), Some("FRAMDACP06"));
    assert_eq!(text(ids.product_family).as_deref(), Some("Desktop"));
    // All 0xff: no UUID.
    assert_eq!(ids.product_uuid, None);
    assert_eq!(text(ids.board_name).as_deref(), Some("FRANMFCP06"));
    assert_eq!(text(ids.board_serial).as_deref(), Some("FRANMFCP06000001"));
    // Spaces alone are no string.
    assert_eq!(ids.board_asset_tag, None);
    assert_eq!(ids.chassis_type, Some(3));
}

#[test]
fn memory_and_processors() {
    let bytes = qemu();
    let table = Table::new(&bytes, None);
    let array = MemoryArray::parse(&table.first(kind::MEMORY_ARRAY).unwrap()).unwrap();
    assert_eq!(
        array,
        MemoryArray { handle: 0x1000, ecc: 0x06, capacity: Some(1 << 30), devices: 1 }
    );
    assert_eq!(memory::ecc_name(array.ecc), "Multi-bit ECC");
    let dimm = MemoryDevice::parse(&table.first(kind::MEMORY_DEVICE).unwrap()).unwrap();
    assert_eq!(dimm.size, Some(1 << 30));
    assert_eq!(dimm.locator.unwrap().to_string(), "DIMM 0");
    assert_eq!(dimm.bank, None);
    assert_eq!(dimm.manufacturer.unwrap().to_string(), "QEMU");
    assert_eq!((memory::form_name(dimm.form), memory::kind_name(dimm.kind)), ("DIMM", "RAM"));
    assert_eq!(
        (dimm.speed, dimm.configured_speed, dimm.rank, dimm.voltage),
        (None, None, None, None)
    );
    assert_eq!(dimm.data_width, None);
    let cpu = Processor::parse(&table.first(kind::PROCESSOR).unwrap()).unwrap();
    assert_eq!(cpu.socket.unwrap().to_string(), "CPU 0");
    assert_eq!(cpu.version.unwrap().to_string(), "pc-q35-8.2");
    assert!(cpu.populated);
    assert_eq!(
        (cpu.max_speed, cpu.cores, cpu.enabled, cpu.threads),
        (Some(2000), Some(1), Some(1), Some(1))
    );
    assert!(Processor::parse(&table.first(kind::BIOS).unwrap()).is_none());

    let bytes = desktop();
    let table = Table::new(&bytes, None);
    let devices: Vec<_> =
        table.of(kind::MEMORY_DEVICE).filter_map(|s| MemoryDevice::parse(&s)).collect();
    assert_eq!(devices.len(), 2);
    let lp = &devices[0];
    assert_eq!(lp.size, Some(16 << 30));
    assert_eq!(
        (memory::form_name(lp.form), memory::kind_name(lp.kind)),
        ("Row Of Chips", "LPDDR5")
    );
    assert_eq!((lp.speed, lp.configured_speed), (Some(8000), Some(8000)));
    assert_eq!(lp.part_number.unwrap().to_string(), "MT62F2G32D4DS-023 WT");
    assert_eq!(lp.serial.unwrap().to_string(), "00000000");
    assert_eq!(lp.asset_tag, None);
    assert_eq!(
        (lp.rank, lp.data_width, lp.total_width, lp.voltage),
        (Some(2), Some(32), Some(32), Some(500))
    );
    assert!(lp.is_present());
    assert!(!devices[1].is_present());
    assert_eq!(memory::kind_name(0x1a), "DDR4");
    assert_eq!(memory::kind_name(0x22), "DDR5");
    assert_eq!(memory::kind_name(0x99), "Unknown");
    assert_eq!(memory::form_name(0x0d), "SODIMM");
}

#[test]
fn tables_cut_short_or_counted() {
    let bytes = qemu();
    // The count ends the walk before the end-of-table structure.
    assert_eq!(Table::new(&bytes, Some(2)).structures().count(), 2);
    // A table cut in a structure's strings stops before it.
    let cut = &bytes[..bytes.len() - 6];
    assert_eq!(Table::new(cut, None).structures().count(), 6);
    // A length below the header's stops the walk.
    let mut bad = bytes.clone();
    bad[1] = 3;
    assert_eq!(Table::new(&bad, None).structures().count(), 0);
    // A string number past the strings is none; strings that are not
    // UTF-8, or hold control characters, show `?`.
    let odd = structure(kind::SYSTEM, 0, &[1, 2, 9, 0], &["Caf\u{e9}", "tab\there"]);
    let mut latin = odd.clone();
    let at = latin.iter().position(|b| *b == 0xc3).unwrap();
    latin.splice(at..at + 2, [0xe9]);
    let table = Table::new(&latin, None);
    let s = table.first(kind::SYSTEM).unwrap();
    assert_eq!(s.string(0x04).unwrap().to_string(), "Caf?");
    assert_eq!(s.string(0x05).unwrap().to_string(), "tab?here");
    assert_eq!(s.string(0x06), None);
    assert_eq!(s.string(0x07), None);
    let table = Table::new(&odd, None);
    assert_eq!(table.first(kind::SYSTEM).unwrap().string(0x04).unwrap().to_string(), "Caf\u{e9}");
}
