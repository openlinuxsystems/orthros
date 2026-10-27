// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests of the register and DMA types and of PCI enumeration, against a
//! simulated configuration space.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use crate::pci::{self, Address, Bar, CONFIG_SIZE, Function, Msi, MsiX};
use crate::{Dma, Mmio, MsiMessage, Registers};

/// A simulated ECAM window of one bus: per-function configuration bytes,
/// with BARs that keep only the bits their size allows.
struct Bus {
    config: Mutex<BTreeMap<usize, Vec<u8>>>,
    /// Per (function offset, BAR register): the mask of writable bits.
    bar_masks: BTreeMap<(usize, usize), u32>,
}

impl Bus {
    fn new() -> Bus {
        Bus { config: Mutex::new(BTreeMap::new()), bar_masks: BTreeMap::new() }
    }

    fn add(&mut self, device: u8, function: u8, bytes: &[(usize, &[u8])]) -> usize {
        let base = (usize::from(device) << 15) | (usize::from(function) << 12);
        let mut config = std::vec![0u8; CONFIG_SIZE];
        for (offset, data) in bytes {
            config[*offset..*offset + data.len()].copy_from_slice(data);
        }
        self.config.lock().unwrap().insert(base, config);
        base
    }
}

impl Registers for Bus {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let base = offset & !(CONFIG_SIZE - 1);
        let config = self.config.lock().unwrap();
        let Some(bytes) = config.get(&base) else { return u64::MAX >> (64 - 8 * size) };
        let at = offset - base;
        let mut value = 0u64;
        for i in 0..size {
            value |= u64::from(bytes[at + i]) << (8 * i);
        }
        value
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let base = offset & !(CONFIG_SIZE - 1);
        let at = offset - base;
        let mut value = value;
        if size == 4 && (0x10..0x28).contains(&at) {
            // An unimplemented BAR is hard-wired to 0.
            let mask = self.bar_masks.get(&(base, at)).copied().unwrap_or(0);
            let old = self.read(offset, 4) as u32;
            // The low bits of a BAR's first register are read-only flags;
            // the upper half of a 64-bit BAR has none.
            let flags = if mask == u32::MAX { 0 } else { old & 0xf };
            value = u64::from((value as u32 & mask) | flags);
        }
        let mut config = self.config.lock().unwrap();
        let Some(bytes) = config.get_mut(&base) else { return };
        for i in 0..size {
            bytes[at + i] = (value >> (8 * i)) as u8;
        }
    }
}

/// Plain registers that keep what is written.
struct Memory(Mutex<Vec<u8>>);

impl Registers for Memory {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let bytes = self.0.lock().unwrap();
        (0..size).fold(0, |value, i| value | u64::from(bytes[offset + i]) << (8 * i))
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut bytes = self.0.lock().unwrap();
        for i in 0..size {
            bytes[offset + i] = (value >> (8 * i)) as u8;
        }
    }
}

fn le16(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

fn le32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

/// A bus with a virtio-blk-like function at 00:02.0 (a 16 KiB 64-bit
/// memory BAR 4, an I/O BAR 0, MSI-X with 2 entries in BAR 4 and a
/// vendor capability) and a two-function device at 00:03.
fn bus() -> Arc<Bus> {
    let mut bus = Bus::new();
    let virtio = bus.add(
        2,
        0,
        &[
            (0x00, &le16(0x1af4)),
            (0x02, &le16(0x1042)),
            (0x06, &le16(1 << 4)),
            (0x0b, &[0x01]),
            (0x0a, &[0x00]),
            (0x10, &le32(0xc001)),
            (0x20, &le32(0xfe00_000c)),
            (0x24, &le32(0x1)),
            (0x34, &[0x40]),
            // MSI-X at 0x40: 2 entries, table in BAR 4 at 0x2000.
            (0x40, &[0x11, 0x50]),
            (0x42, &le16(1)),
            (0x44, &le32(0x2004)),
            (0x48, &le32(0x3004)),
            // A vendor capability at 0x50, ending the list.
            (0x50, &[0x09, 0x00, 0x10, 0x01]),
        ],
    );
    bus.bar_masks.insert((virtio, 0x10), 0xffff_ffe0);
    bus.bar_masks.insert((virtio, 0x20), 0xffff_c000);
    bus.bar_masks.insert((virtio, 0x24), 0xffff_ffff);
    bus.add(3, 0, &[(0x00, &le16(0x8086)), (0x02, &le16(0x2922)), (0x0e, &[0x80])]);
    bus.add(3, 1, &[(0x00, &le16(0x8086)), (0x02, &le16(0x2930))]);
    // Function 1 of a device without function 0 is never looked at.
    bus.add(4, 1, &[(0x00, &le16(0x1234))]);
    Arc::new(bus)
}

fn functions(bus: &Arc<Bus>) -> Vec<Function> {
    let mut found = Vec::new();
    let window = Mmio::fake(bus.clone(), pci::BUS_SIZE);
    pci::scan(0, 0, 0, &mut |_| Some(window.clone()), &mut |f| found.push(f));
    found
}

#[test]
fn scan_finds_functions_in_order() {
    let found = functions(&bus());
    let addresses: Vec<String> = found.iter().map(|f| f.address().to_string()).collect();
    assert_eq!(addresses, ["0000:00:02.0", "0000:00:03.0", "0000:00:03.1"]);
    assert_eq!(found[0].vendor_id(), 0x1af4);
    assert_eq!(found[0].device_id(), 0x1042);
    assert_eq!(found[0].class(), (1, 0, 0));
    assert!(found[1].multifunction());
}

#[test]
fn bars_are_sized() {
    let found = functions(&bus());
    let virtio = &found[0];
    assert_eq!(virtio.bar(0), Some(Bar::Io { port: 0xc000, size: 0x20 }));
    assert_eq!(
        virtio.bar(4),
        Some(Bar::Memory { address: 0x1_fe00_0000, size: 0x4000, prefetchable: true })
    );
    assert_eq!(virtio.bar(5), None, "the upper half of a 64-bit BAR is not a BAR");
    assert_eq!(virtio.bar(1), None);
    // Sizing put the original values back.
    assert_eq!(virtio.read32(0x20), 0xfe00_000c);
    assert_eq!(virtio.read32(0x24), 0x1);
}

#[test]
fn capabilities_and_msix() {
    let found = functions(&bus());
    let virtio = &found[0];
    let caps: Vec<(u8, usize)> = virtio.capabilities().collect();
    assert_eq!(caps, [(0x11, 0x40), (0x09, 0x50)]);
    assert!(Msi::find(virtio).is_none());
    let msix = MsiX::find(virtio).unwrap();
    assert_eq!(msix.entries(), 2);
    assert_eq!(msix.table_location(), (4, 0x2000));
    msix.enable_masked(virtio);
    assert_eq!(virtio.read16(0x42) & 0xc000, 0xc000);
    msix.unmask_all(virtio);
    assert_eq!(virtio.read16(0x42) & 0xc000, 0x8000);

    let window = Mmio::fake(Arc::new(Memory(Mutex::new(std::vec![0u8; 64]))), 64);
    let message = MsiMessage { address: 0xfee0_0000, data: 0x51 };
    msix.set_entry(&window, 1, message);
    assert_eq!(window.read32(16), 0xfee0_0000);
    assert_eq!(window.read32(20), 0);
    assert_eq!(window.read32(24), 0x51);
    assert_eq!(window.read32(28), 0, "the entry is unmasked");
    msix.mask_entry(&window, 1);
    assert_eq!(window.read32(28), 1);
}

#[test]
fn a_looping_capability_list_ends() {
    let mut bus = Bus::new();
    bus.add(
        0,
        0,
        &[(0x00, &le16(1)), (0x06, &le16(1 << 4)), (0x34, &[0x40]), (0x40, &[0x05, 0x40])],
    );
    let bus = Arc::new(bus);
    let found = functions(&bus);
    assert_eq!(found[0].capabilities().count(), 48);
}

#[test]
fn bridges_lead_to_their_buses_once() {
    struct Buses(BTreeMap<usize, Arc<Bus>>);
    let mut zero = Bus::new();
    // A bridge on bus 0 to bus 1, and one on bus 1 back to bus 0.
    zero.add(1, 0, &[(0x00, &le16(0x8086)), (0x0e, &[0x01]), (0x19, &[1])]);
    let mut one = Bus::new();
    one.add(0, 0, &[(0x00, &le16(0x1af4)), (0x0e, &[0x01]), (0x19, &[0])]);
    let buses = Buses(BTreeMap::from([(0, Arc::new(zero)), (1, Arc::new(one))]));
    let mut found = Vec::new();
    let mut map = |bus: u8| {
        buses
            .0
            .get(&usize::from(bus))
            .map(|b| Mmio::fake(b.clone() as Arc<dyn Registers>, pci::BUS_SIZE))
    };
    pci::scan(0, 0, 255, &mut map, &mut |f| found.push(f.address()));
    assert_eq!(
        found,
        [
            Address { segment: 0, bus: 0, device: 1, function: 0 },
            Address { segment: 0, bus: 1, device: 0, function: 0 }
        ]
    );
}

#[test]
#[should_panic(expected = "outside")]
fn register_accesses_are_checked() {
    let window = Mmio::fake(bus(), 64);
    window.read32(64);
}

#[test]
#[should_panic(expected = "outside")]
fn misaligned_register_accesses_are_refused() {
    let window = Mmio::fake(bus(), 64);
    window.read32(2);
}

#[test]
#[should_panic(expected = "unaligned")]
fn accesses_are_aligned_in_memory_not_only_in_their_window() {
    let window = Mmio::fake(bus(), 64).window(2, 16).unwrap();
    window.read32(0);
}

#[test]
fn windows_stay_inside() {
    let window = Mmio::fake(bus(), 64);
    assert!(window.window(32, 32).is_some());
    assert!(window.window(33, 32).is_none());
    assert!(window.window(usize::MAX, 2).is_none());
}

#[test]
fn dma_memory_round_trips() {
    let dma = Dma::fake(100);
    dma.write32(4, 0xdead_beef);
    dma.write(10, b"hello");
    assert_eq!(dma.read32(4), 0xdead_beef);
    let mut out = [0u8; 5];
    dma.read(10, &mut out);
    assert_eq!(&out, b"hello");
    let mut seen = [0u8; 5];
    crate::fake_memory(dma.phys() + 10, &mut seen);
    assert_eq!(&seen, b"hello");
    dma.zero();
    assert_eq!(dma.read64(0), 0);
    assert_eq!(dma.read8(99), 0);
}

#[test]
#[should_panic(expected = "outside")]
fn dma_accesses_are_checked() {
    let dma = Dma::fake(16);
    dma.write64(16, 1);
}

/// CRC-32C a bit at a time, as RFC 3720 defines it.
fn crc32c_bits(crc: u32, data: &[u8]) -> u32 {
    let mut crc = crc;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
        }
    }
    crc
}

#[test]
fn crc32c_matches_its_definition() {
    let Some(_) = crate::crc32c::crc32c(0, b"") else {
        std::eprintln!("no CRC-32C instruction here");
        return;
    };
    // RFC 3720, B.4: 32 bytes of zeros, of ones, counting up and down.
    let up: Vec<u8> = (0..32).collect();
    let down: Vec<u8> = (0..32).rev().collect();
    for (data, sum) in [
        (&[0u8; 32][..], 0x8a91_36aa),
        (&[0xff; 32][..], 0x62a8_ab43),
        (&up, 0x46dd_794e),
        (&down, 0x113f_db5c),
    ] {
        assert_eq!(!crate::crc32c::crc32c(!0, data).unwrap(), sum);
    }
    // Every length around the three runs of a pass, from any register.
    let mut seed = 0x1234_5678_9abc_def1u64;
    let data: Vec<u8> = (0..3 * 4080 + 200)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect();
    for len in (0..64).chain(4000..4200).chain([8160, 8161, 12_240, 12_340]) {
        for (start, crc) in [(0, 0), (3, !0), (5, 0xdead_beef)] {
            let part = &data[start..start + len];
            assert_eq!(crate::crc32c::crc32c(crc, part), Some(crc32c_bits(crc, part)), "{len}");
        }
    }
}
