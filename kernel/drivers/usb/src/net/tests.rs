// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::vec::Vec;

use super::ncm::{self, Parameters};
use super::{Notification, Port, Protocol, mac_from_string, notification, passed_over, port};
use crate::{Configuration, InterfaceDescriptor, descriptor};

/// A configuration as QEMU's `usb-net` has it in its CDC Ethernet
/// configuration: a communications interface of `subclass` with its
/// header, union and Ethernet networking descriptors and an interrupt
/// endpoint, and a data interface whose second setting has the bulk
/// pair.
fn configuration(subclass: u8, union: bool) -> Vec<u8> {
    let mut bytes = std::vec![9, 2, 0, 0, 2, 1, 0, 0xc0, 50];
    bytes.extend([9, 4, 0, 0, 1, 2, subclass, 0, 0]);
    bytes.extend([5, 0x24, 0x00, 0x10, 0x01]);
    if union {
        bytes.extend([5, 0x24, 0x06, 0, 1]);
    }
    // MAC string 3, no statistics, 1514-byte segments, no filters.
    bytes.extend([13, 0x24, 0x0f, 3, 0, 0, 0, 0, 0xea, 0x05, 0, 0, 0]);
    bytes.extend([7, 5, 0x81, 3, 16, 0, 9]);
    bytes.extend([9, 4, 1, 0, 0, 0x0a, 0, 0, 0]);
    bytes.extend([9, 4, 1, 1, 2, 0x0a, 0, 0, 0]);
    bytes.extend([7, 5, 0x82, 2, 0x00, 0x02, 0]);
    bytes.extend([7, 5, 0x03, 2, 0x00, 0x02, 0]);
    let len = bytes.len() as u16;
    bytes[2..4].copy_from_slice(&len.to_le_bytes());
    bytes
}

fn string_of(text: &str) -> Vec<u8> {
    let mut bytes = std::vec![(2 + 2 * text.len()) as u8, descriptor::STRING];
    for unit in text.encode_utf16() {
        bytes.extend(unit.to_le_bytes());
    }
    bytes
}

#[test]
fn an_ecm_adapter_is_found_with_its_data_setting() {
    let bytes = configuration(0x06, true);
    let config = Configuration::parse(&bytes).unwrap();
    let found: Port = port(&config).unwrap();
    assert_eq!(found.protocol, Protocol::Ecm);
    assert_eq!((found.control, found.data, found.alternate), (0, 1, 1));
    assert_eq!((found.bulk_in.address, found.bulk_out.address), (0x82, 0x03));
    assert_eq!(found.notify.map(|e| e.address), Some(0x81));
    assert_eq!((found.mac_string, found.max_segment), (3, 1514));
    // Without a union, the next interface; NCM's subclass is NCM.
    let bytes = configuration(0x0d, false);
    let found = port(&Configuration::parse(&bytes).unwrap()).unwrap();
    assert_eq!((found.protocol, found.data), (Protocol::Ncm, 1));
    // An ACM modem is no adapter.
    let bytes = configuration(0x02, true);
    assert_eq!(port(&Configuration::parse(&bytes).unwrap()), None);
}

#[test]
fn a_data_interface_without_a_bulk_pair_is_no_adapter() {
    let mut bytes = configuration(0x06, true);
    // The OUT endpoint made an interrupt one.
    let at = bytes.len() - 4;
    bytes[at] = 3;
    assert_eq!(port(&Configuration::parse(&bytes).unwrap()), None);
}

#[test]
fn the_mac_address_is_twelve_hex_digits() {
    assert_eq!(mac_from_string(&string_of("40ABCDef0102")), Some([0x40, 0xab, 0xcd, 0xef, 1, 2]));
    // Too short, not hex, multicast, all zeros.
    assert_eq!(mac_from_string(&string_of("40ABCDef01")), None);
    assert_eq!(mac_from_string(&string_of("40ABCDef01xy")), None);
    assert_eq!(mac_from_string(&string_of("01ABCDef0102")), None);
    assert_eq!(mac_from_string(&string_of("000000000000")), None);
    assert_eq!(mac_from_string(&[4, 2, 0x30, 0]), None);
}

#[test]
fn notifications_say_the_link_and_its_speed() {
    assert_eq!(notification(&[0xa1, 0x00, 1, 0, 0, 0, 0, 0]), Some(Notification::Connection(true)));
    assert_eq!(
        notification(&[0xa1, 0x00, 0, 0, 0, 0, 0, 0]),
        Some(Notification::Connection(false))
    );
    let mut speed = std::vec![0xa1, 0x2a, 0, 0, 0, 0, 8, 0];
    speed.extend(1_000_000_000u32.to_le_bytes());
    speed.extend(100_000_000u32.to_le_bytes());
    assert_eq!(notification(&speed), Some(Notification::Speed(1_000_000_000, 100_000_000)));
    assert_eq!(notification(&[0xa1, 0x00, 1, 0]), None);
    assert_eq!(notification(&[0x21, 0x00, 1, 0, 0, 0, 0, 0]), None);
}

fn parameters() -> Parameters {
    let mut bytes = [0u8; ncm::PARAMETERS];
    bytes[0..2].copy_from_slice(&28u16.to_le_bytes());
    bytes[2..4].copy_from_slice(&1u16.to_le_bytes());
    bytes[4..8].copy_from_slice(&16384u32.to_le_bytes());
    bytes[16..20].copy_from_slice(&16384u32.to_le_bytes());
    bytes[20..22].copy_from_slice(&4u16.to_le_bytes());
    bytes[22..24].copy_from_slice(&2u16.to_le_bytes());
    bytes[24..26].copy_from_slice(&4u16.to_le_bytes());
    Parameters::parse(&bytes).unwrap()
}

#[test]
fn ntb_parameters_are_read_and_checked() {
    let p = parameters();
    assert_eq!((p.in_max, p.out_max, p.out_divisor, p.out_remainder), (16384, 16384, 4, 2));
    // Without 16-bit blocks, or with a remainder past its divisor.
    let mut bytes = [0u8; ncm::PARAMETERS];
    bytes[2] = 2;
    assert_eq!(Parameters::parse(&bytes), None);
    bytes[2] = 1;
    bytes[20] = 4;
    bytes[22] = 4;
    assert_eq!(Parameters::parse(&bytes), None);
    assert_eq!(Parameters::parse(&bytes[..20]), None);
}

#[test]
fn a_block_carries_a_frame_where_the_adapter_wants_it() {
    let p = parameters();
    let frame: Vec<u8> = (0..100u8).collect();
    let mut out = [0u8; 2048];
    let len = ncm::build(&p, 7, &frame, &mut out).unwrap();
    assert_eq!(len, p.block_length(frame.len()));
    let found: Vec<_> = ncm::frames(&out[..len]).collect();
    assert_eq!(found.len(), 1);
    let (at, size) = found[0];
    assert_eq!((at % 4, size), (2, 100));
    assert_eq!(&out[at..at + size], &frame[..]);
    assert_eq!(&out[0..4], b"NCMH");
    assert_eq!(u16::from_le_bytes([out[6], out[7]]), 7);
    // Too long for the buffer.
    assert_eq!(ncm::build(&p, 8, &frame, &mut out[..64]), None);
}

/// A block of the adapter's with `frames`, in two tables.
fn block(frames: &[&[u8]]) -> Vec<u8> {
    let mut out = std::vec![0u8; 12];
    out[0..4].copy_from_slice(b"NCMH");
    out[4..6].copy_from_slice(&12u16.to_le_bytes());
    let mut offsets = Vec::new();
    for frame in frames {
        offsets.push((out.len(), frame.len()));
        out.extend_from_slice(frame);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }
    let (first, second) = offsets.split_at(offsets.len() / 2);
    let mut tables = Vec::new();
    for half in [first, second] {
        let at = out.len();
        tables.push(at);
        out.extend_from_slice(b"NCM0");
        out.extend(((8 + 4 * (half.len() + 1)) as u16).to_le_bytes());
        out.extend([0, 0]);
        for &(offset, len) in half {
            out.extend((offset as u16).to_le_bytes());
            out.extend((len as u16).to_le_bytes());
        }
        out.extend([0, 0, 0, 0]);
    }
    // The first table points at the second.
    let second_at = tables[1] as u16;
    out[tables[0] + 6..tables[0] + 8].copy_from_slice(&second_at.to_le_bytes());
    let len = out.len() as u16;
    out[8..10].copy_from_slice(&len.to_le_bytes());
    out[10..12].copy_from_slice(&(tables[0] as u16).to_le_bytes());
    out
}

#[test]
fn every_frame_of_a_block_is_found_through_its_tables() {
    let frames: [&[u8]; 4] = [&[1; 60], &[2; 1514], &[3; 64], &[4; 99]];
    let bytes = block(&frames);
    let found: Vec<&[u8]> = ncm::frames(&bytes).map(|(at, len)| &bytes[at..at + len]).collect();
    assert_eq!(found, frames);
}

#[test]
fn a_broken_block_gives_only_what_is_inside_it() {
    let frames: [&[u8]; 2] = [&[1; 60], &[2; 70]];
    let good = block(&frames);
    // Not a block.
    let mut bytes = good.clone();
    bytes[0] = b'X';
    assert_eq!(ncm::frames(&bytes).count(), 0);
    // Cut short: the block says it is longer than what came.
    assert_eq!(ncm::frames(&good[..good.len() - 4]).count(), 0);
    // A frame that runs past the end is left out, the other kept.
    let mut bytes = good.clone();
    let table = usize::from(u16::from_le_bytes([bytes[10], bytes[11]]));
    bytes[table + 10..table + 12].copy_from_slice(&5000u16.to_le_bytes());
    assert_eq!(ncm::frames(&bytes).map(|(_, len)| len).collect::<Vec<_>>(), [70]);
    // A table that points at itself ends after 64 tables.
    let mut bytes = good;
    bytes[table + 6..table + 8].copy_from_slice(&(table as u16).to_le_bytes());
    assert_eq!(ncm::frames(&bytes).count(), 64);
}

#[test]
fn a_makers_own_or_rndis_configuration_is_passed_over() {
    let first = |class, subclass, protocol| InterfaceDescriptor {
        number: 0,
        alternate: 0,
        endpoints: 1,
        class,
        subclass,
        protocol,
        string: 0,
    };
    // The RTL8153's first configuration, and QEMU's usb-net's.
    assert!(passed_over(&first(0xff, 0xff, 0x00)));
    assert!(passed_over(&first(0x02, 0x02, 0xff)));
    assert!(passed_over(&first(0xe0, 0x01, 0x03)));
    assert!(passed_over(&first(0xef, 0x01, 0x01)));
    // An ACM serial port, an ECM adapter, a disk: kept.
    assert!(!passed_over(&first(0x02, 0x02, 0x01)));
    assert!(!passed_over(&first(0x02, 0x06, 0x00)));
    assert!(!passed_over(&first(0x08, 0x06, 0x50)));
}
