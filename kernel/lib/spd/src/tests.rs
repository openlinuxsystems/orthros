// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reads over a simulated bus of each kind of EEPROM, with the EE1004's
//! page shared by the bus and modules that do not acknowledge its page
//! selects, and the decoding of SPD data made by JEDEC's layouts for a
//! DDR3, a DDR4 and a DDR5 module.

use std::collections::HashMap;
use std::vec::Vec;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Nack,
    Failed,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Fault::Nack => "no device answered",
            Fault::Failed => "the transaction failed",
        })
    }
}

enum Device {
    Plain(Vec<u8>),
    Ee1004(Vec<u8>),
    Spd5118 { mr11: u8, nvm: Vec<u8>, locked: bool },
}

struct Sim {
    devices: HashMap<u8, Device>,
    /// The EE1004s' page, which every one on the bus shares.
    page: u8,
    /// Whether their page selects are acknowledged, and whether they take
    /// them at all.
    acknowledged: bool,
    stuck: bool,
}

impl Sim {
    fn new(devices: impl IntoIterator<Item = (u8, Device)>) -> Sim {
        Sim { devices: devices.into_iter().collect(), page: 0, acknowledged: true, stuck: false }
    }

    fn has_ee1004(&self) -> bool {
        self.devices.values().any(|d| matches!(d, Device::Ee1004(_)))
    }
}

impl Bus for Sim {
    type Error = Fault;

    fn send_byte(&mut self, address: u8, _value: u8) -> Result<(), Fault> {
        if let Some(page) = EE1004_PAGE.iter().position(|a| *a == address)
            && self.has_ee1004()
        {
            if !self.stuck {
                self.page = page as u8;
            }
            return if self.acknowledged { Ok(()) } else { Err(Fault::Nack) };
        }
        self.devices.get(&address).map(|_| ()).ok_or(Fault::Nack)
    }

    fn receive_byte(&mut self, address: u8) -> Result<u8, Fault> {
        if address == EE1004_PAGE[0] && self.has_ee1004() {
            return if self.page == 0 { Ok(0) } else { Err(Fault::Nack) };
        }
        self.devices.get(&address).map(|_| 0).ok_or(Fault::Nack)
    }

    fn read_byte(&mut self, address: u8, command: u8) -> Result<u8, Fault> {
        let at = usize::from(command);
        match self.devices.get(&address).ok_or(Fault::Nack)? {
            Device::Plain(bytes) => Ok(bytes[at]),
            Device::Ee1004(bytes) => Ok(bytes[usize::from(self.page) * 256 + at]),
            Device::Spd5118 { mr11, nvm, .. } => Ok(match command {
                0 => 0x51,
                1 => 0x18,
                MR11 => *mr11,
                0x80.. => nvm[usize::from(*mr11 & MR11_PAGE) * 128 + at - 0x80],
                _ => 0,
            }),
        }
    }

    fn write_byte(&mut self, address: u8, command: u8, value: u8) -> Result<(), Fault> {
        match self.devices.get_mut(&address).ok_or(Fault::Nack)? {
            Device::Spd5118 { mr11, locked: false, .. } if command == MR11 => {
                *mr11 = value;
                Ok(())
            }
            _ => Err(Fault::Failed),
        }
    }

    fn is_nack(error: &Fault) -> bool {
        *error == Fault::Nack
    }
}

/// `len` bytes, each its offset plus `seed`.
fn pattern<const N: usize>(seed: u8) -> [u8; N] {
    core::array::from_fn(|n| (n as u8).wrapping_add(seed).wrapping_add((n >> 8) as u8))
}

#[test]
fn the_eeprom_follows_the_memory_kind() {
    assert_eq!(Eeprom::of_kind(0x18), Some(Eeprom::Spd), "DDR3");
    assert_eq!(Eeprom::of_kind(0x1d), Some(Eeprom::Spd), "LPDDR3");
    assert_eq!(Eeprom::of_kind(0x1a), Some(Eeprom::Ee1004), "DDR4");
    assert_eq!(Eeprom::of_kind(0x22), Some(Eeprom::Spd5118), "DDR5");
    assert_eq!(Eeprom::of_kind(0x23), Some(Eeprom::Spd5118), "LPDDR5");
    // QEMU's "RAM", "Unknown" and HBM have none.
    for kind in [0x07, 0x02, 0x20] {
        assert_eq!(Eeprom::of_kind(kind), None);
    }
    let named: Vec<_> = [Eeprom::Spd, Eeprom::Ee1004, Eeprom::Spd5118]
        .iter()
        .map(|e| (e.name(), e.size()))
        .collect();
    assert_eq!(named, [("spd", 256), ("ee1004", 512), ("spd5118", 1024)]);
}

#[test]
fn a_plain_eeprom_reads_byte_by_byte() {
    let bytes = pattern::<256>(7);
    let mut bus = Sim::new([(0x50, Device::Plain(bytes.to_vec()))]);
    let mut out = [0xaa; MAX_SIZE];
    assert_eq!(read(&mut bus, Eeprom::Spd, 0x50, &mut out), Ok(()));
    assert_eq!(out[..256], bytes);
    assert!(out[256..].iter().all(|b| *b == 0xaa), "the rest is left alone");
    assert_eq!(read(&mut bus, Eeprom::Spd, 0x51, &mut out), Err(Error::Bus(Fault::Nack)));
    assert!(!is_spd5118(&mut bus, 0x50));
}

#[test]
fn an_ee1004_reads_both_pages_and_is_left_on_page_0() {
    let (first, second) = (pattern::<512>(0), pattern::<512>(0x40));
    let mut bus =
        Sim::new([(0x50, Device::Ee1004(first.to_vec())), (0x52, Device::Ee1004(second.to_vec()))]);
    let mut out = [0; MAX_SIZE];
    for (address, bytes) in [(0x50, first), (0x52, second)] {
        // The firmware left page 1 picked.
        bus.page = 1;
        assert_eq!(read(&mut bus, Eeprom::Ee1004, address, &mut out), Ok(()));
        assert_eq!(out[..512], bytes);
        assert_eq!(bus.page, 0);
    }
    // Modules that take page selects without acknowledging them.
    bus.acknowledged = false;
    out.fill(0);
    assert_eq!(read(&mut bus, Eeprom::Ee1004, 0x52, &mut out), Ok(()));
    assert_eq!(out[..512], second);
    assert_eq!(bus.page, 0);
    // Modules that do not take them.
    bus.stuck = true;
    assert_eq!(read(&mut bus, Eeprom::Ee1004, 0x50, &mut out), Err(Error::Page(1)));
    bus.page = 1;
    assert_eq!(read(&mut bus, Eeprom::Ee1004, 0x50, &mut out), Err(Error::Page(0)));
}

#[test]
fn an_spd5118_reads_its_eight_pages_and_keeps_its_page() {
    let nvm = pattern::<1024>(3);
    let hub = |mr11, locked| Device::Spd5118 { mr11, nvm: nvm.to_vec(), locked };
    let mut bus = Sim::new([(0x51, hub(3, false))]);
    assert!(is_spd5118(&mut bus, 0x51));
    assert!(!is_spd5118(&mut bus, 0x50), "nothing answers there");
    let mut out = [0; MAX_SIZE];
    assert_eq!(read(&mut bus, Eeprom::Spd5118, 0x51, &mut out), Ok(()));
    assert_eq!(out, nvm);
    assert!(matches!(bus.devices[&0x51], Device::Spd5118 { mr11: 3, .. }), "back on page 3");
    // Set for two-byte offsets, or with its configuration locked.
    bus.devices.insert(0x51, hub(MR11_TWO_BYTES, false));
    assert_eq!(read(&mut bus, Eeprom::Spd5118, 0x51, &mut out), Err(Error::TwoByteOffsets));
    bus.devices.insert(0x51, hub(0, true));
    assert_eq!(read(&mut bus, Eeprom::Spd5118, 0x51, &mut out), Err(Error::Bus(Fault::Failed)));
}

/// Puts `text` at `at`, padded with spaces to `len`.
fn put_text(bytes: &mut [u8], at: usize, len: usize, text: &str) {
    bytes[at..at + len].fill(b' ');
    bytes[at..at + text.len()].copy_from_slice(text.as_bytes());
}

/// Stores the CRC of `covered` at `at`.
fn put_crc(bytes: &mut [u8], covered: core::ops::Range<usize>, at: usize) {
    let crc = crc16(&bytes[covered]);
    bytes[at..at + 2].copy_from_slice(&crc.to_le_bytes());
}

/// A Kingston 4 GiB DDR3-1600 SO-DIMM: one rank of 4 Gb x8 chips.
fn ddr3() -> [u8; 256] {
    let mut b = [0; 256];
    b[0] = 0x92; // 256 bytes, CRC of bytes 0 to 116.
    b[1] = 0x13;
    b[2] = 0x0b;
    b[3] = 0x03;
    b[4] = 0x04;
    b[7] = 0x01;
    b[8] = 0x03;
    b[9] = 0x11; // Fine timebase: 1 ps.
    b[10] = 1; // Medium timebase: 1/8 ns.
    b[11] = 8;
    b[12] = 0x0a; // tCKmin: 1.25 ns.
    b[117] = 0x01; // Kingston: bank 2.
    b[118] = 0x98;
    b[120] = 0x15;
    b[121] = 0x30;
    b[122..126].copy_from_slice(&[0x0a, 0x1b, 0x2c, 0x3d]);
    put_text(&mut b, 128, 18, "KCP3L16SS8/4");
    put_crc(&mut b, 0..117, 126);
    b
}

/// A Samsung 8 GiB DDR4-2666 SO-DIMM: one rank of 8 Gb x8 chips.
fn ddr4() -> [u8; 512] {
    let mut b = [0; 512];
    b[0] = 0x23;
    b[1] = 0x11;
    b[2] = 0x0c;
    b[3] = 0x03;
    b[4] = 0x45;
    b[12] = 0x01;
    b[13] = 0x03;
    b[18] = 0x06; // tCKAVGmin: 750 ps.
    b[320] = 0x80; // Samsung: bank 1.
    b[321] = 0xce;
    b[323] = 0x19;
    b[324] = 0x12;
    b[325..329].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
    put_text(&mut b, 329, 20, "M471A1K43CB1-CTD");
    put_crc(&mut b, 0..126, 126);
    put_crc(&mut b, 128..254, 254);
    b
}

/// A Micron 16 GiB DDR5-4800 SO-DIMM: two channels of 32 bits, one rank
/// of 16 Gb x8 chips.
fn ddr5() -> [u8; 1024] {
    let mut b = [0; 1024];
    b[0] = 0x30;
    b[1] = 0x10;
    b[2] = 0x12;
    b[3] = 0x03;
    b[4] = 0x04;
    b[6] = 0x20;
    b[20..22].copy_from_slice(&416u16.to_le_bytes()); // tCKAVGmin, in ps.
    b[234] = 0x00;
    b[235] = 0x22;
    b[512] = 0x80; // Micron: bank 1.
    b[513] = 0x2c;
    b[515] = 0x22;
    b[516] = 0x05;
    b[517..521].copy_from_slice(&[0xe0, 0x01, 0x02, 0x03]);
    put_text(&mut b, 521, 30, "MTC8C1084S1SC48BA1");
    put_crc(&mut b, 0..510, 510);
    b
}

#[test]
fn the_checksum_is_jedecs_crc() {
    // CRC-16/XMODEM's check value.
    assert_eq!(crc16(b"123456789"), 0x31c3);
}

#[test]
fn ddr3_ddr4_and_ddr5_modules_decode() {
    let (ddr3, ddr4, ddr5) = (ddr3(), ddr4(), ddr5());
    let decoded = |bytes: &[u8]| {
        let spd = Spd::new(bytes);
        (
            spd.kind(),
            spd.form(),
            spd.size().map(|size| size >> 30),
            spd.speed(),
            spd.maker().map(|maker| std::format!("{maker}")),
            spd.date(),
            spd.serial().map(|serial| std::format!("{serial}")),
            spd.part_number().map(std::string::String::from),
            spd.checksums_match(),
        )
    };
    let date = |year, week| Some(Date { year, week });
    assert_eq!(
        decoded(&ddr3),
        (
            Some("DDR3"),
            Some("SO-DIMM"),
            Some(4),
            Some(1600),
            Some("Kingston".into()),
            date(2015, 30),
            Some("0A1B2C3D".into()),
            Some("KCP3L16SS8/4".into()),
            Some(true)
        )
    );
    assert_eq!(
        decoded(&ddr4),
        (
            Some("DDR4"),
            Some("SO-DIMM"),
            Some(8),
            Some(2666),
            Some("Samsung".into()),
            date(2019, 12),
            Some("12345678".into()),
            Some("M471A1K43CB1-CTD".into()),
            Some(true)
        )
    );
    assert_eq!(
        decoded(&ddr5),
        (
            Some("DDR5"),
            Some("SO-DIMM"),
            Some(16),
            Some(4800),
            Some("Micron Technology".into()),
            date(2022, 5),
            Some("E0010203".into()),
            Some("MTC8C1084S1SC48BA1".into()),
            Some(true)
        )
    );
}

#[test]
fn speeds_come_to_their_grades_and_stacks_count_their_dies() {
    let mut b = ddr4();
    // tCKAVGmin as 875 ps less 42, and 1 ns less 62.
    for (medium, fine, grade) in [(0x07, 0xd6, 2400), (0x08, 0xc2, 2133), (0x05, 0x00, 3200)] {
        b[18] = medium;
        b[125] = fine;
        assert_eq!(Spd::new(&b).speed(), Some(grade));
    }
    // A 3DS RDIMM: two packages ranks of four 16 Gb x4 dies a stack.
    b[3] = 0x01;
    b[4] = 0x86;
    b[6] = 0xb2;
    b[12] = 0x08;
    let spd = Spd::new(&b);
    assert_eq!((spd.form(), spd.size()), (Some("RDIMM"), Some(256 << 30)));
    assert_eq!(spd.checksums_match(), Some(false), "the bytes changed after their CRC");
}

#[test]
fn blank_or_unknown_data_says_nothing() {
    let zeros = [0; 512];
    let spd = Spd::new(&zeros);
    assert!(spd.is_blank());
    assert_eq!((spd.kind(), spd.size(), spd.speed(), spd.maker()), (None, None, None, None));
    assert_eq!((spd.date(), spd.serial(), spd.part_number()), (None, None, None));
    assert_eq!(spd.checksums_match(), None);
    // An LPDDR4X's layout is not known, but its kind is.
    let mut b = [0; 512];
    b[2] = 0x11;
    let spd = Spd::new(&b);
    assert!(!spd.is_blank());
    assert_eq!((spd.kind(), spd.form(), spd.size()), (Some("LPDDR4X"), None, None));
    // A maker not known, a week past 53 and a part number not in ASCII.
    let mut b = ddr4();
    b[320..322].copy_from_slice(&[0x01, 0x94]);
    b[324] = 0x54;
    b[330] = 0xc3;
    let spd = Spd::new(&b);
    assert_eq!(std::format!("{}", spd.maker().unwrap()), "JEP106 bank 2 maker 0x94");
    assert_eq!((spd.date(), spd.part_number()), (None, None));
    // Cut short.
    assert_eq!(Spd::new(&ddr4()[..256]).maker(), None);
}
