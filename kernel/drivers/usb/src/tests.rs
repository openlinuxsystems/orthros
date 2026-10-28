// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Descriptors as devices send them, whole, short and broken.

use proptest::prelude::*;
use std::string::String;
use std::vec::Vec;

use crate::*;

/// A high-speed keyboard's device descriptor, as QEMU's `usb-kbd` gives
/// it.
const KEYBOARD: [u8; 18] =
    [18, 1, 0x00, 0x02, 0, 0, 0, 64, 0x27, 0x06, 0x01, 0x00, 0x00, 0x00, 1, 4, 11, 1];

/// A configuration with a boot keyboard interface (a HID descriptor and
/// an interrupt IN endpoint) and a second interface with two bulk
/// endpoints and a SuperSpeed companion.
fn configuration() -> Vec<u8> {
    let mut bytes = std::vec![9, 2, 0, 0, 2, 1, 6, 0xa0, 50];
    bytes.extend([9, 4, 0, 0, 1, 3, 1, 1, 0]);
    bytes.extend([9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0]);
    bytes.extend([7, 5, 0x81, 3, 8, 0, 7]);
    bytes.extend([9, 4, 1, 0, 2, 8, 6, 0x50, 5]);
    bytes.extend([7, 5, 0x82, 2, 0x00, 0x04, 0]);
    bytes.extend([6, 0x30, 15, 0, 0, 0]);
    bytes.extend([7, 5, 0x03, 2, 0x00, 0x04, 0]);
    let len = bytes.len() as u16;
    bytes[2..4].copy_from_slice(&len.to_le_bytes());
    bytes
}

/// The text of the string descriptor of `units`.
fn text(bytes: &[u8]) -> String {
    let mut out = String::new();
    string(bytes, &mut out).unwrap();
    out
}

/// A string descriptor of `units`.
fn descriptor_of(units: &[u16]) -> Vec<u8> {
    let mut bytes = std::vec![(2 + 2 * units.len()) as u8, descriptor::STRING];
    for unit in units {
        bytes.extend(unit.to_le_bytes());
    }
    bytes
}

#[test]
fn setup_packets_are_little_endian() {
    let get = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 18);
    assert_eq!(get.to_u64(), 0x0012_0000_0100_0680);
    assert!(get.device_to_host());
    let set = Setup::set_configuration(1);
    assert_eq!(set.to_u64(), 0x0000_0000_0001_0900);
    assert!(!set.device_to_host());
    let string = Setup::get_descriptor(descriptor::STRING, 4, 0x0409, 255);
    assert_eq!(string.to_u64(), 0x00ff_0409_0304_0680);
}

#[test]
fn a_device_descriptor_says_what_the_device_is() {
    let device = DeviceDescriptor::parse(&KEYBOARD).unwrap();
    assert_eq!(device.usb, 0x0200);
    assert_eq!((device.vendor, device.product, device.release), (0x0627, 1, 0));
    assert_eq!((device.class, device.subclass, device.protocol), (0, 0, 0));
    assert_eq!(
        (device.manufacturer_string, device.product_string, device.serial_string),
        (1, 4, 11)
    );
    assert_eq!(device.configurations, 1);
    assert_eq!(device.max_packet0(Speed::High), Some(64));
    // 64 is not a packet size a low-speed or SuperSpeed device may have.
    assert_eq!(device.max_packet0(Speed::Low), None);
    assert_eq!(device.max_packet0(Speed::Super), None);
}

#[test]
fn the_first_eight_bytes_are_enough_for_the_packet_size() {
    let mut first = [0u8; 8];
    first.copy_from_slice(&KEYBOARD[..8]);
    let device = DeviceDescriptor::parse(&first).unwrap();
    assert_eq!(device.max_packet0(Speed::High), Some(64));
    assert_eq!((device.vendor, device.configurations), (0, 0));
    // Fewer than eight, or not a device's, is nothing.
    assert_eq!(DeviceDescriptor::parse(&KEYBOARD[..7]), None);
    let mut other = KEYBOARD;
    other[1] = descriptor::CONFIGURATION;
    assert_eq!(DeviceDescriptor::parse(&other), None);
    // Bytes past the length the descriptor says are not its own.
    let mut short = KEYBOARD;
    short[0] = 12;
    let device = DeviceDescriptor::parse(&short).unwrap();
    assert_eq!(device.product, 1);
    assert_eq!((device.manufacturer_string, device.configurations), (0, 0));
}

#[test]
fn packet_sizes_follow_the_speed() {
    let with = |raw: u8| {
        let mut bytes = KEYBOARD;
        bytes[7] = raw;
        DeviceDescriptor::parse(&bytes).unwrap()
    };
    assert_eq!(with(8).max_packet0(Speed::Low), Some(8));
    for raw in [8, 16, 32, 64] {
        assert_eq!(with(raw).max_packet0(Speed::Full), Some(u16::from(raw)));
    }
    assert_eq!(with(12).max_packet0(Speed::Full), None);
    assert_eq!(with(9).max_packet0(Speed::Super), Some(512));
    assert_eq!(with(9).max_packet0(Speed::SuperPlus), Some(512));
    assert_eq!(Speed::Full.default_max_packet(), 8);
    assert_eq!(Speed::Super.default_max_packet(), 512);
    assert!(Speed::SuperPlus.is_super() && !Speed::High.is_super());
}

#[test]
fn a_configuration_lists_its_interfaces_and_their_endpoints() {
    let bytes = configuration();
    let config = Configuration::parse(&bytes).unwrap();
    assert_eq!((config.interfaces, config.value, config.max_power), (2, 1, 50));
    assert_eq!(usize::from(config.total_length), bytes.len());
    let interfaces: Vec<_> = config.interfaces().collect();
    assert_eq!(interfaces.len(), 2);
    let keyboard = &interfaces[0];
    assert_eq!(
        (keyboard.descriptor.class, keyboard.descriptor.subclass, keyboard.descriptor.protocol),
        (class::HID, 1, 1)
    );
    let kinds: Vec<u8> = keyboard.descriptors().map(|(kind, _)| kind).collect();
    assert_eq!(kinds, [descriptor::HID, descriptor::ENDPOINT]);
    let endpoints: Vec<_> = keyboard.endpoints().collect();
    assert_eq!(endpoints.len(), 1);
    let endpoint = endpoints[0];
    assert_eq!((endpoint.number(), endpoint.is_in()), (1, true));
    assert_eq!(endpoint.transfer_type(), TransferType::Interrupt);
    assert_eq!((endpoint.max_packet(), endpoint.interval), (8, 7));
    let storage = &interfaces[1];
    assert_eq!(storage.descriptor.number, 1);
    assert_eq!(storage.descriptor.string, 5);
    let endpoints: Vec<_> = storage.endpoints().collect();
    assert_eq!(endpoints.len(), 2);
    assert_eq!((endpoints[0].number(), endpoints[0].is_in()), (2, true));
    assert_eq!((endpoints[1].number(), endpoints[1].is_in()), (3, false));
    assert!(endpoints.iter().all(|e| e.transfer_type() == TransferType::Bulk));
    assert_eq!(endpoints[0].max_packet(), 1024);
    // The companion stays with the interface's descriptors.
    assert_eq!(
        storage
            .descriptors()
            .filter(|&(k, _)| k == descriptor::SUPERSPEED_ENDPOINT_COMPANION)
            .count(),
        1
    );
}

#[test]
fn high_bandwidth_endpoints_say_their_extra_transactions() {
    let endpoint = EndpointDescriptor::parse(&[7, 5, 0x81, 1, 0x00, 0x14, 1]).unwrap();
    assert_eq!(endpoint.transfer_type(), TransferType::Isochronous);
    assert_eq!((endpoint.max_packet(), endpoint.extra_transactions()), (1024, 2));
}

#[test]
fn a_configuration_cut_short_or_broken_ends_where_it_breaks() {
    let bytes = configuration();
    // What came is less than the length it says: the walk stops at the
    // end of what came, without a descriptor that ran past it.
    let config = Configuration::parse(&bytes[..30]).unwrap();
    let interfaces: Vec<_> = config.interfaces().collect();
    assert_eq!(interfaces.len(), 1);
    assert_eq!(interfaces[0].endpoints().count(), 0);
    // A descriptor of length 0 ends the walk rather than looping.
    let mut broken = bytes.clone();
    broken[18] = 0;
    let config = Configuration::parse(&broken).unwrap();
    assert_eq!(config.descriptors().count(), 1);
    // A length past what the configuration says it holds counts only what
    // it says.
    let mut long = bytes.clone();
    long.extend([9, 4, 2, 0, 0, 3, 0, 0, 0]);
    let config = Configuration::parse(&long).unwrap();
    assert_eq!(config.interfaces().count(), 2);
    // A total length shorter than the header still reads the header.
    let mut tiny = bytes.clone();
    tiny[2..4].copy_from_slice(&3u16.to_le_bytes());
    let config = Configuration::parse(&tiny).unwrap();
    assert_eq!(config.interfaces().count(), 0);
    assert_eq!(Configuration::parse(&bytes[..8]).map(|c| c.value), None);
}

#[test]
fn strings_are_utf16_without_their_padding() {
    let units: Vec<u16> = "QEMU USB Keyboard".encode_utf16().collect();
    assert_eq!(text(&descriptor_of(&units)), "QEMU USB Keyboard");
    let padded: Vec<u16> = "  SanDisk  \0\0".encode_utf16().collect();
    assert_eq!(text(&descriptor_of(&padded)), "SanDisk");
    // A lone surrogate is replaced; control characters are dropped.
    assert_eq!(text(&descriptor_of(&[0x41, 0xd800, 0x42, 0x0a, 0x1b, 0x43])), "A\u{fffd}BC");
    // Characters outside the BMP come in pairs.
    let units: Vec<u16> = "a\u{1f5b1}".encode_utf16().collect();
    assert_eq!(text(&descriptor_of(&units)), "a\u{1f5b1}");
    // An odd byte at the end, and bytes past bLength, are left out.
    let mut odd = descriptor_of(&[0x41, 0x42]);
    odd[0] = 5;
    assert_eq!(text(&odd), "A");
    // Not a string: nothing.
    assert_eq!(text(&[4, descriptor::DEVICE, 0x41, 0]), "");
}

#[test]
fn the_first_language_is_read_or_us_english() {
    assert_eq!(first_language(&[4, 3, 0x07, 0x04]), 0x0407);
    assert_eq!(first_language(&[2, 3]), 0x0409);
    assert_eq!(first_language(&[]), 0x0409);
}

#[test]
fn classes_have_names() {
    assert_eq!(class_name(class::HID, 1, 1), "keyboard");
    assert_eq!(class_name(class::HID, 1, 2), "mouse");
    assert_eq!(class_name(class::HID, 0, 0), "HID");
    assert_eq!(class_name(class::MASS_STORAGE, 6, 0x50), "mass storage");
    assert_eq!(class_name(class::HUB, 0, 0), "hub");
    assert_eq!(class_name(class::WIRELESS, 1, 1), "Bluetooth");
    assert_eq!(class_name(0x42, 0, 0), "unknown class");
}

proptest! {
    /// Whatever a device sends, reading it neither panics nor loops.
    #[test]
    fn any_bytes_read_safely(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        let _ = DeviceDescriptor::parse(&bytes);
        let mut patched = bytes.clone();
        if patched.len() >= 9 {
            patched[1] = descriptor::CONFIGURATION;
            patched[0] = patched[0].max(9);
        }
        if let Some(config) = Configuration::parse(&patched) {
            let mut seen = 0usize;
            for interface in config.interfaces() {
                seen += 1 + interface.descriptors().count() + interface.endpoints().count();
            }
            prop_assert!(seen <= patched.len());
            prop_assert!(config.descriptors().count() <= patched.len() / 2);
        }
        let _ = text(&bytes);
        let _ = first_language(&bytes);
    }
}
