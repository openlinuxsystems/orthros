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
    // The bulk IN endpoint's companion: bursts of 16.
    assert_eq!((endpoints[0].max_burst, endpoints[1].max_burst), (15, 0));
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
        for usb3 in [false, true] {
            if let Some(hub) = hub::HubDescriptor::parse(&bytes, usb3) {
                prop_assert!(hub.usable_ports() <= hub::MAX_PORTS);
                prop_assert!(hub.report_size() <= 32);
            }
            if let Some(port) = hub::PortStatus::parse(&bytes, usb3) {
                prop_assert!(port.changes().count() <= 6);
            }
        }
    }

    /// Routes built port by port are as deep as the ports given, and
    /// below each hub on the way.
    #[test]
    fn routes_nest(ports in proptest::collection::vec(1u8..=15, 0..7)) {
        let mut route = 0;
        let mut hubs = Vec::new();
        for &port in &ports {
            match hub::child_route(route, port) {
                Some(next) => {
                    hubs.push(route);
                    route = next;
                }
                None => prop_assert_eq!(hub::depth(route), hub::MAX_DEPTH),
            }
        }
        prop_assert_eq!(hub::depth(route), ports.len().min(hub::MAX_DEPTH));
        for (i, &above) in hubs.iter().enumerate() {
            prop_assert!(hub::is_below(route, above));
            prop_assert!(!hub::is_below(above, route));
            if i > 0 {
                prop_assert!(hub::is_below(above, hubs[i - 1]));
                prop_assert!(!hub::is_below(hubs[i - 1], above));
            }
        }
        prop_assert!(!hub::is_below(route, route));
    }
}

/// QEMU's `usb-hub`: a full-speed hub of eight ports, each powered alone
/// with its own over-current report, good 2 ms after power.
const QEMU_HUB: [u8; 9] = [9, 0x29, 8, 0x0a, 0x00, 0x01, 0x00, 0x00, 0xff];

#[test]
fn a_hub_descriptor_says_its_ports() {
    let hub = hub::HubDescriptor::parse(&QEMU_HUB, false).unwrap();
    assert_eq!((hub.ports, hub.characteristics, hub.power_good_ms), (8, 0x0a, 2));
    assert_eq!((hub.usable_ports(), hub.report_size(), hub.think_time()), (8, 2, 0));
    // Not a USB 3 hub's.
    assert_eq!(hub::HubDescriptor::parse(&QEMU_HUB, true), None);
    // A USB 3 hub of four ports, good after 100 ms.
    let usb3 = [12, 0x2a, 4, 0x09, 0x00, 50, 0, 0, 0, 0, 0, 0];
    let hub = hub::HubDescriptor::parse(&usb3, true).unwrap();
    assert_eq!((hub.ports, hub.power_good_ms, hub.report_size()), (4, 100, 1));
    // A big hub: only the ports a route string reaches; think time from
    // bits 5 and 6.
    let big = [9, 0x29, 20, 0x60, 0x00, 1, 0, 0, 0];
    let hub = hub::HubDescriptor::parse(&big, false).unwrap();
    assert_eq!((hub.usable_ports(), hub.report_size(), hub.think_time()), (15, 3, 3));
    assert_eq!(hub::HubDescriptor::parse(&QEMU_HUB[..6], false), None);
}

#[test]
fn port_statuses_read_by_the_hubs_layout() {
    use hub::{PortStatus, feature};
    // A USB 2 hub's port: connected, enabled, powered, low speed; the
    // connection and reset changed.
    let port = PortStatus::parse(&[0x03, 0x03, 0x11, 0x00], false).unwrap();
    assert!(port.connected() && port.enabled() && port.powered() && !port.resetting());
    assert_eq!(port.speed(), Speed::Low);
    assert!(port.connect_changed() && port.reset_changed() && !port.over_current_changed());
    let changes: Vec<u16> = port.changes().collect();
    assert_eq!(changes, [feature::C_PORT_CONNECTION, feature::C_PORT_RESET]);
    let high = PortStatus::parse(&[0x03, 0x05, 0, 0], false).unwrap();
    assert_eq!(high.speed(), Speed::High);
    let full = PortStatus::parse(&[0x01, 0x01, 0, 0], false).unwrap();
    assert_eq!((full.speed(), full.enabled()), (Speed::Full, false));
    // A USB 3 hub's port: power is bit 9, and bit 8 is the link state's;
    // a warm reset's end and a link change.
    let port = PortStatus::parse(&[0x03, 0x02, 0x60, 0x00], true).unwrap();
    assert!(port.connected() && port.powered() && port.reset_changed());
    assert_eq!(port.speed(), Speed::Super);
    let changes: Vec<u16> = port.changes().collect();
    assert_eq!(changes, [feature::C_BH_PORT_RESET, feature::C_PORT_LINK_STATE]);
    let unpowered = PortStatus::parse(&[0x00, 0x01, 0, 0], true).unwrap();
    assert!(!unpowered.powered());
    assert_eq!(PortStatus::parse(&[1, 2, 3], false), None);
}

#[test]
fn hub_requests_go_to_the_hub_or_its_ports() {
    assert_eq!(Setup::hub_descriptor(false, 71).to_u64(), 0x0047_0000_2900_06a0);
    assert_eq!(Setup::hub_descriptor(true, 12).to_u64(), 0x000c_0000_2a00_06a0);
    assert_eq!(Setup::port_status(3).to_u64(), 0x0004_0003_0000_00a3);
    assert_eq!(
        Setup::set_port_feature(2, hub::feature::PORT_RESET).to_u64(),
        0x0000_0002_0004_0323
    );
    assert_eq!(
        Setup::clear_port_feature(2, hub::feature::C_PORT_CONNECTION).to_u64(),
        0x0000_0002_0010_0123
    );
    assert_eq!(Setup::set_hub_depth(1).to_u64(), 0x0000_0000_0001_0c20);
    assert_eq!(Setup::hub_status().to_u64(), 0x0004_0000_0000_00a0);
    assert_eq!(
        Setup::clear_hub_feature(hub::feature::C_HUB_OVER_CURRENT).to_u64(),
        0x0000_0000_0001_0120
    );
    assert_eq!(Setup::clear_halt(0x81).to_u64(), 0x0000_0081_0000_0102);
}

#[test]
fn routes_name_the_ports_below_the_root_port() {
    assert_eq!(hub::depth(0), 0);
    assert_eq!(hub::child_route(0, 3), Some(0x3));
    assert_eq!(hub::child_route(0x3, 8), Some(0x83));
    assert_eq!(hub::depth(0x83), 2);
    assert!(hub::is_below(0x83, 0x3) && hub::is_below(0x83, 0));
    assert!(!hub::is_below(0x84, 0x3) && !hub::is_below(0x3, 0x3));
    assert_eq!(hub::child_route(0x3, 16), None);
    assert_eq!(hub::child_route(0x3, 0), None);
    assert_eq!(hub::child_route(0x1_1111, 1), None);
}

#[test]
fn a_hid_interface_says_how_long_its_report_descriptor_is() {
    let bytes = configuration();
    let config = Configuration::parse(&bytes).unwrap();
    let keyboard = config.interfaces().next().unwrap();
    let (_, hid) = keyboard.descriptors().find(|&(kind, _)| kind == descriptor::HID).unwrap();
    assert_eq!(
        hid::HidDescriptor::parse(hid),
        Some(hid::HidDescriptor { version: 0x0111, country: 0, report_length: 63 })
    );
    // A physical descriptor listed first, then the report descriptor.
    let two = [12, 0x21, 0x10, 0x01, 33, 2, 0x23, 9, 0, 0x22, 0x40, 0x01];
    assert_eq!(
        hid::HidDescriptor::parse(&two),
        Some(hid::HidDescriptor { version: 0x0110, country: 33, report_length: 0x140 })
    );
    // Short, of another type, or without a report descriptor.
    assert_eq!(hid::HidDescriptor::parse(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63]), None);
    assert_eq!(hid::HidDescriptor::parse(&[9, 0x29, 0x11, 0x01, 0, 1, 0x22, 63, 0]), None);
    assert_eq!(hid::HidDescriptor::parse(&[9, 0x21, 0x11, 0x01, 0, 1, 0x23, 63, 0]), None);
    assert_eq!(hid::HidDescriptor::parse(&[9, 0x21, 0x11, 0x01, 0, 4, 0x23, 63, 0]), None);
}

#[test]
fn hid_requests_go_to_the_interface() {
    assert_eq!(Setup::report_descriptor(1, 63).to_u64(), 0x003f_0001_2200_0681);
    assert_eq!(Setup::set_idle(0).to_u64(), 0x0000_0000_0000_0a21);
    assert_eq!(Setup::set_protocol(2, false).to_u64(), 0x0000_0002_0000_0b21);
    assert_eq!(Setup::set_protocol(2, true).to_u64(), 0x0000_0002_0001_0b21);
    assert_eq!(Setup::set_report(0, hid::REPORT_OUTPUT, 1, 2).to_u64(), 0x0002_0000_0201_0921);
}

/// A CDC-ACM device's descriptor and configuration, as a board's: the
/// communications interface with its functional descriptors and its
/// notification endpoint, then the data interface with its bulk pair.
fn acm_device() -> (DeviceDescriptor, Vec<u8>) {
    let device = DeviceDescriptor::parse(&[
        18, 1, 0x00, 0x02, 2, 0, 0, 64, 0x8a, 0x2e, 0x0a, 0x00, 0x00, 0x01, 1, 2, 3, 1,
    ])
    .unwrap();
    let mut bytes = std::vec![9, 2, 0, 0, 2, 1, 0, 0x80, 50];
    bytes.extend([9, 4, 0, 0, 1, 2, 2, 1, 0]);
    bytes.extend([5, 0x24, 0x00, 0x10, 0x01]);
    bytes.extend([5, 0x24, 0x01, 0x00, 0x01]);
    bytes.extend([4, 0x24, 0x02, 0x02]);
    bytes.extend([5, 0x24, 0x06, 0x00, 0x01]);
    bytes.extend([7, 5, 0x83, 3, 8, 0, 16]);
    bytes.extend([9, 4, 1, 0, 2, 0x0a, 0, 0, 0]);
    bytes.extend([7, 5, 0x81, 2, 0x40, 0, 0]);
    bytes.extend([7, 5, 0x02, 2, 0x40, 0, 0]);
    let len = bytes.len() as u16;
    bytes[2..4].copy_from_slice(&len.to_le_bytes());
    (device, bytes)
}

/// An FTDI chip with `interfaces` vendor-specific interfaces, each with a
/// bulk pair, of release `release`, as QEMU's `usb-serial` (one, 0x0400).
fn ftdi_device(product: u16, release: u16, interfaces: u8) -> (DeviceDescriptor, Vec<u8>) {
    let [p0, p1] = product.to_le_bytes();
    let [r0, r1] = release.to_le_bytes();
    let device =
        DeviceDescriptor::parse(&[18, 1, 0, 2, 0, 0, 0, 8, 0x03, 0x04, p0, p1, r0, r1, 1, 2, 3, 1])
            .unwrap();
    let mut bytes = std::vec![9, 2, 0, 0, interfaces, 1, 0, 0x80, 50];
    for i in 0..interfaces {
        bytes.extend([9, 4, i, 0, 2, 0xff, 0xff, 0xff, 2]);
        bytes.extend([7, 5, 0x81 + 2 * i, 2, 0x40, 0, 0]);
        bytes.extend([7, 5, 0x02 + 2 * i, 2, 0x40, 0, 0]);
    }
    let len = bytes.len() as u16;
    bytes[2..4].copy_from_slice(&len.to_le_bytes());
    (device, bytes)
}

#[test]
fn acm_port_is_found_through_its_union() {
    let (device, bytes) = acm_device();
    let config = Configuration::parse(&bytes).unwrap();
    let mut ports = [None; 2].map(|_: Option<()>| serial::Port {
        protocol: serial::Protocol::Acm { control: 9 },
        data: 0,
        bulk_in: EndpointDescriptor::default(),
        bulk_out: EndpointDescriptor::default(),
    });
    assert_eq!(serial::ports(&device, &config, &mut ports), 1);
    let port = ports[0];
    assert_eq!(port.protocol, serial::Protocol::Acm { control: 0 });
    assert_eq!((port.data, port.bulk_in.address, port.bulk_out.address), (1, 0x81, 0x02));
    assert_eq!((port.driver(), port.prefix()), ("cdc_acm", "ttyACM"));
    // Not FTDI's, and a keyboard has none.
    assert_eq!(serial::ftdi::chip(&device), None);
    let keyboard = DeviceDescriptor::parse(&KEYBOARD).unwrap();
    let config = configuration();
    assert_eq!(serial::ports(&keyboard, &Configuration::parse(&config).unwrap(), &mut ports), 0);
}

#[test]
fn acm_without_a_union_takes_the_next_interface() {
    let (device, mut bytes) = acm_device();
    // The union and the call management descriptors become unknown ones.
    for at in 0..bytes.len() - 2 {
        if bytes[at + 1] == 0x24 && matches!(bytes[at + 2], 0x01 | 0x06) {
            bytes[at + 2] = 0x0f;
        }
    }
    let config = Configuration::parse(&bytes).unwrap();
    let mut ports = [serial::Port {
        protocol: serial::Protocol::Acm { control: 9 },
        data: 0,
        bulk_in: EndpointDescriptor::default(),
        bulk_out: EndpointDescriptor::default(),
    }];
    assert_eq!(serial::ports(&device, &config, &mut ports), 1);
    assert_eq!(ports[0].data, 1);
}

#[test]
fn ftdi_ports_are_each_interface() {
    let blank = serial::Port {
        protocol: serial::Protocol::Acm { control: 0 },
        data: 0,
        bulk_in: EndpointDescriptor::default(),
        bulk_out: EndpointDescriptor::default(),
    };
    let (qemu, bytes) = ftdi_device(0x6001, 0x0400, 1);
    let mut ports = [blank; 4];
    let config = Configuration::parse(&bytes).unwrap();
    assert_eq!(serial::ports(&qemu, &config, &mut ports), 1);
    let chip = serial::ftdi::Chip::Bm;
    assert_eq!(ports[0].protocol, serial::Protocol::Ftdi { chip, index: 0 });
    assert_eq!((ports[0].driver(), ports[0].prefix()), ("ftdi_sio", "ttyUSB"));
    let (dual, bytes) = ftdi_device(0x6010, 0x0700, 2);
    let config = Configuration::parse(&bytes).unwrap();
    assert_eq!(serial::ports(&dual, &config, &mut ports), 2);
    let chip = serial::ftdi::Chip::Ft2232h;
    assert_eq!(ports[1].protocol, serial::Protocol::Ftdi { chip, index: 2 });
    assert_eq!((ports[1].bulk_in.address, ports[1].bulk_out.address), (0x83, 0x04));
    // Another vendor's product of the same number is not FTDI's.
    let mut other = qemu;
    other.vendor = 0x1234;
    assert_eq!(serial::ftdi::chip(&other), None);
}

#[test]
fn lines_are_read_from_cflag() {
    use serial::{B9600, Line, Parity, Stop, cflag};
    // A serial port's start: B9600 | CS8 | CREAD | HUPCL | CLOCAL.
    let start = Line::from_cflag(B9600 | cflag::CSIZE | 0o200 | cflag::HUPCL | cflag::CLOCAL, 0);
    assert_eq!(
        start,
        Line { rate: 9600, data_bits: 8, parity: Parity::None, stop: Stop::One, rts_cts: false }
    );
    // 7E2 at 115200 with RTS and CTS.
    let flags = 0o10002 | 0o40 | cflag::PARENB | cflag::CSTOPB | cflag::CRTSCTS;
    let line = Line::from_cflag(flags, 0);
    assert_eq!(
        line,
        Line { rate: 115_200, data_bits: 7, parity: Parity::Even, stop: Stop::Two, rts_cts: true }
    );
    // Mark parity, five bits and a half stop bit more, and a speed of its own.
    let line = Line::from_cflag(
        cflag::BOTHER | cflag::PARENB | cflag::PARODD | cflag::CMSPAR | cflag::CSTOPB,
        250_000,
    );
    assert_eq!(
        (line.rate, line.data_bits, line.parity, line.stop),
        (250_000, 5, Parity::Mark, Stop::OneAndHalf)
    );
    assert_eq!(Line::from_cflag(0, 0).rate, 0);
    assert_eq!(serial::acm::line_coding(&start), [0x80, 0x25, 0, 0, 0, 0, 8]);
    let coding = serial::acm::line_coding(&Line::from_cflag(flags, 0));
    assert_eq!(coding, [0x00, 0xc2, 0x01, 0x00, 2, 2, 7]);
    let setup = Setup::set_line_coding(3);
    assert_eq!((setup.request_type, setup.request, setup.index, setup.length), (0x21, 0x20, 3, 7));
    let setup = Setup::set_control_lines(3, serial::acm::DTR | serial::acm::RTS);
    assert_eq!((setup.request_type, setup.request, setup.value), (0x21, 0x22, 3));
}

#[test]
fn ftdi_divisors_are_as_ftdi_gives_them() {
    use serial::ftdi::{Chip, baud_rate, divisor, rate_of_divisor};
    // AN232B-05's: 9600 is 312.5 (0x4138), 115200 is 26 (0x1a), and the
    // two below 2.
    assert_eq!(divisor(Chip::Bm, 9600), Some(0x4138));
    assert_eq!(divisor(Chip::Bm, 115_200), Some(0x1a));
    assert_eq!(divisor(Chip::Bm, 3_000_000), Some(0));
    assert_eq!(divisor(Chip::Bm, 2_000_000), Some(1));
    assert_eq!(divisor(Chip::Bm, 0), None);
    assert_eq!(divisor(Chip::Bm, 4_000_000), None);
    // Too slow for 14 bits.
    assert_eq!(divisor(Chip::Bm, 100), None);
    // The H series' 12 MHz, with bit 17.
    assert_eq!(divisor(Chip::Ft232h, 115_200), Some(0x2_c068));
    assert_eq!(divisor(Chip::Ft232h, 12_000_000), Some(0x2_0000));
    assert_eq!(divisor(Chip::Ft232h, 300), divisor(Chip::Bm, 300));
    for chip in [Chip::Am, Chip::Bm, Chip::Ft232r, Chip::Ft2232h] {
        for rate in [300, 1200, 9600, 19200, 38400, 57600, 115_200, 230_400, 460_800, 921_600] {
            let got = rate_of_divisor(chip, divisor(chip, rate).unwrap());
            let off = (f64::from(got) - f64::from(rate)).abs() / f64::from(rate);
            assert!(off < 0.03, "{chip:?} at {rate}: {got}");
        }
    }
    // A single port's divisor's high bits in wIndex; a dual one's above
    // the port's number.
    let setup = baud_rate(Chip::Bm, 0, 9600).unwrap();
    assert_eq!((setup.request_type, setup.request, setup.value, setup.index), (0x40, 3, 0x4138, 0));
    let setup = baud_rate(Chip::Ft2232h, 2, 115_200).unwrap();
    assert_eq!((setup.value, setup.index), (0xc068, 0x0202));
}

#[test]
fn ftdi_requests_and_packets() {
    use serial::ftdi;
    let line = serial::Line::from_cflag(0o10002 | 0o40 | serial::cflag::PARENB, 0);
    assert_eq!(ftdi::data(1, &line).value, 7 | 2 << 8);
    assert_eq!(ftdi::modem_control(0, true).value, 0x0303);
    assert_eq!(ftdi::modem_control(0, false).value, 0x0300);
    assert_eq!(ftdi::flow_control(1, true).index, 0x0101);
    assert_eq!(ftdi::reset(1).request, ftdi::request::RESET);
    // A packet of eight bytes and a short one, each with its status.
    let came = [1, 0x60, b'a', b'b', b'c', b'd', b'e', b'f', 1, 0x60, b'g', b'h'];
    let mut out = [0u8; 16];
    assert_eq!(ftdi::strip(&came, 8, &mut out), 8);
    assert_eq!(&out[..8], b"abcdefgh");
    // Only the status: nothing came.
    assert_eq!(ftdi::strip(&[1, 0x60], 64, &mut out), 0);
    let mut small = [0u8; 3];
    assert_eq!(ftdi::strip(&came, 8, &mut small), 3);
}

proptest! {
    #[test]
    fn ftdi_divisors_round_trip(rate in 200u32..2_000_000, h in any::<bool>()) {
        let chip = if h { serial::ftdi::Chip::Ft232h } else { serial::ftdi::Chip::Ft232r };
        if let Some(encoded) = serial::ftdi::divisor(chip, rate) {
            let got = serial::ftdi::rate_of_divisor(chip, encoded);
            let off = (f64::from(got) - f64::from(rate)).abs() / f64::from(rate);
            prop_assert!(off <= 0.2, "{} {}", rate, got);
        }
    }

    #[test]
    fn serial_ports_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
        if let Some(config) = Configuration::parse(&bytes) {
            let (device, _) = ftdi_device(0x6001, 0x0400, 1);
            let (acm, _) = acm_device();
            let mut ports = [serial::Port {
                protocol: serial::Protocol::Acm { control: 0 },
                data: 0,
                bulk_in: EndpointDescriptor::default(),
                bulk_out: EndpointDescriptor::default(),
            }; 2];
            let _ = serial::ports(&device, &config, &mut ports);
            let _ = serial::ports(&acm, &config, &mut ports);
        }
    }
}
