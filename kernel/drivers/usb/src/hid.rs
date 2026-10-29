// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! HID interfaces, as the *Device Class Definition for Human Interface
//! Devices (HID), Version 1.11* puts them on USB.
//!
//! A HID interface (class 3) has a HID descriptor among its own
//! ([`HidDescriptor`]), which says how long its report descriptor is; the
//! host reads that with [`Setup::report_descriptor`], and `radix-hid`
//! makes sense of it. The device sends its Input reports on an interrupt
//! IN endpoint; the host sends Output reports, such as the keyboard's
//! lights, with [`Setup::set_report`]. [`Setup::set_idle`] asks it to
//! send a report only when something changed, and
//! [`Setup::set_protocol`] picks the fixed boot reports that a keyboard
//! or mouse of the boot subclass also knows, for a host that cannot read
//! its report descriptor.

use crate::{CLASS, DEVICE_TO_HOST, STANDARD, Setup, TO_INTERFACE};

/// The class's requests (HID 1.11, 7.2).
pub mod request {
    /// GET_REPORT.
    pub const GET_REPORT: u8 = 0x01;
    /// GET_IDLE.
    pub const GET_IDLE: u8 = 0x02;
    /// GET_PROTOCOL.
    pub const GET_PROTOCOL: u8 = 0x03;
    /// SET_REPORT.
    pub const SET_REPORT: u8 = 0x09;
    /// SET_IDLE.
    pub const SET_IDLE: u8 = 0x0a;
    /// SET_PROTOCOL.
    pub const SET_PROTOCOL: u8 = 0x0b;
}

/// The report descriptor's descriptor type (HID 1.11, 7.1).
pub const REPORT_DESCRIPTOR: u8 = 0x22;

/// The boot interface subclass (HID 1.11, 4.2): the interface also knows
/// the boot protocol.
pub const SUBCLASS_BOOT: u8 = 1;
/// The boot protocols (HID 1.11, 4.3): a keyboard's and a mouse's.
pub const PROTOCOL_KEYBOARD: u8 = 1;
/// A mouse's.
pub const PROTOCOL_MOUSE: u8 = 2;

/// The report types of GET_REPORT and SET_REPORT (HID 1.11, 7.2.1).
pub const REPORT_INPUT: u8 = 1;
/// An Output report.
pub const REPORT_OUTPUT: u8 = 2;
/// A Feature report.
pub const REPORT_FEATURE: u8 = 3;

/// What a HID descriptor says (HID 1.11, 6.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidDescriptor {
    /// bcdHID: the version of the HID specification it follows.
    pub version: u16,
    /// bCountryCode: the country its keys are made for, if it says (0
    /// when it does not).
    pub country: u8,
    /// How long its report descriptor is.
    pub report_length: u16,
}

impl HidDescriptor {
    /// Reads a HID descriptor from `bytes`, as an interface's descriptors
    /// hold it: its fixed six bytes, then a type and a length for each of
    /// its class descriptors, the first of which must be the report
    /// descriptor's.
    pub fn parse(bytes: &[u8]) -> Option<HidDescriptor> {
        let len = usize::from(*bytes.first()?).min(bytes.len());
        if len < 9 || bytes[1] != crate::descriptor::HID {
            return None;
        }
        let count = usize::from(bytes[5]);
        let report_length = (0..count)
            .map(|i| 6 + 3 * i)
            .take_while(|&at| at + 3 <= len)
            .find(|&at| bytes[at] == REPORT_DESCRIPTOR)
            .map(|at| u16::from_le_bytes([bytes[at + 1], bytes[at + 2]]))?;
        Some(HidDescriptor {
            version: u16::from_le_bytes([bytes[2], bytes[3]]),
            country: bytes[4],
            report_length,
        })
    }
}

impl Setup {
    /// GET_DESCRIPTOR of the report descriptor of interface `interface`:
    /// its first `length` bytes (HID 1.11, 7.1.1).
    pub fn report_descriptor(interface: u8, length: u16) -> Setup {
        Setup {
            request_type: DEVICE_TO_HOST | STANDARD | TO_INTERFACE,
            request: crate::request::GET_DESCRIPTOR,
            value: u16::from(REPORT_DESCRIPTOR) << 8,
            index: u16::from(interface),
            length,
        }
    }

    /// SET_IDLE: interface `interface` sends its reports only when they
    /// change, not again and again while nothing does (HID 1.11, 7.2.4).
    pub fn set_idle(interface: u8) -> Setup {
        Setup {
            request_type: CLASS | TO_INTERFACE,
            request: request::SET_IDLE,
            value: 0,
            index: u16::from(interface),
            length: 0,
        }
    }

    /// SET_PROTOCOL: interface `interface` sends boot reports, or with
    /// `report` the reports its report descriptor describes (HID 1.11,
    /// 7.2.6).
    pub fn set_protocol(interface: u8, report: bool) -> Setup {
        Setup {
            request_type: CLASS | TO_INTERFACE,
            request: request::SET_PROTOCOL,
            value: u16::from(report),
            index: u16::from(interface),
            length: 0,
        }
    }

    /// SET_REPORT: sends interface `interface` a report of type `kind`
    /// (REPORT_OUTPUT for the lights) and ID `id`, `length` bytes with
    /// the ID (HID 1.11, 7.2.2).
    pub fn set_report(interface: u8, kind: u8, id: u8, length: u16) -> Setup {
        Setup {
            request_type: CLASS | TO_INTERFACE,
            request: request::SET_REPORT,
            value: u16::from(kind) << 8 | u16::from(id),
            index: u16::from(interface),
            length,
        }
    }
}
