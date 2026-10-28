// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB devices as chapter 9 of the *Universal Serial Bus Specification,
//! Revision 2.0* and of the *Universal Serial Bus 3.2 Specification*
//! describes them, whatever host controller they are on (Phase 11, ADR
//! 0146): their speeds, the standard requests every device answers on its
//! default control pipe, and the descriptors it answers with.
//!
//! - [`Speed`]: low, full and high speed (USB 2.0), SuperSpeed and
//!   SuperSpeedPlus (USB 3.2), and the default control pipe's packet size
//!   at each before the device says its own.
//! - [`Setup`]: the eight bytes that start a control transfer, and the
//!   standard requests the kernel makes.
//! - [`DeviceDescriptor`], [`Configuration`] (with its interfaces,
//!   endpoints and the class-specific descriptors among them) and
//!   [`string`]: what a device says it is, read from what it sent, which
//!   may be short, long or wrong; nothing here trusts a length the device
//!   gave without checking it against the bytes that came.
//! - [`class_name`]: words for a device's or an interface's class.
//!
//! The crate knows nothing of the kernel or of a controller, so its tests
//! run on the host.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

#[cfg(test)]
mod tests;

/// How fast a device talks to its port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Speed {
    /// 1.5 Mb/s (USB 1.0 and later): keyboards and mice.
    Low,
    /// 12 Mb/s (USB 1.1 and later).
    Full,
    /// 480 Mb/s (USB 2.0).
    High,
    /// 5 Gb/s (USB 3.0, USB 3.2 Gen 1).
    Super,
    /// 10 Gb/s or more (USB 3.1 Gen 2, USB 3.2 Gen 2 and Gen 2x2).
    SuperPlus,
}

impl Speed {
    /// The packet size of the default control pipe, endpoint 0, before the
    /// device's descriptor says its own: 8 bytes at low and full speed (a
    /// full-speed device may take up to 64, which its descriptor says),
    /// 64 at high speed and 512 at SuperSpeed.
    pub fn default_max_packet(self) -> u16 {
        match self {
            Speed::Low | Speed::Full => 8,
            Speed::High => 64,
            Speed::Super | Speed::SuperPlus => 512,
        }
    }

    /// Whether the device talks USB 3 (SuperSpeed or faster).
    pub fn is_super(self) -> bool {
        self >= Speed::Super
    }
}

impl fmt::Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Speed::Low => "low speed",
            Speed::Full => "full speed",
            Speed::High => "high speed",
            Speed::Super => "SuperSpeed",
            Speed::SuperPlus => "SuperSpeedPlus",
        })
    }
}

/// The standard requests (USB 2.0, table 9-4).
pub mod request {
    /// GET_STATUS.
    pub const GET_STATUS: u8 = 0;
    /// CLEAR_FEATURE.
    pub const CLEAR_FEATURE: u8 = 1;
    /// SET_FEATURE.
    pub const SET_FEATURE: u8 = 3;
    /// GET_DESCRIPTOR.
    pub const GET_DESCRIPTOR: u8 = 6;
    /// GET_CONFIGURATION.
    pub const GET_CONFIGURATION: u8 = 8;
    /// SET_CONFIGURATION.
    pub const SET_CONFIGURATION: u8 = 9;
    /// SET_INTERFACE.
    pub const SET_INTERFACE: u8 = 11;
}

/// Descriptor types (USB 2.0 table 9-5, USB 3.2 table 9-6, and the class
/// specifications' for those the kernel reads).
pub mod descriptor {
    /// A device's descriptor.
    pub const DEVICE: u8 = 1;
    /// A configuration's, with its interfaces' and endpoints' after it.
    pub const CONFIGURATION: u8 = 2;
    /// A string.
    pub const STRING: u8 = 3;
    /// An interface's.
    pub const INTERFACE: u8 = 4;
    /// An endpoint's.
    pub const ENDPOINT: u8 = 5;
    /// The Binary device Object Store (USB 3.2).
    pub const BOS: u8 = 15;
    /// A HID's (HID 1.11).
    pub const HID: u8 = 0x21;
    /// A hub's (USB 2.0 11.23.2.1).
    pub const HUB: u8 = 0x29;
    /// A SuperSpeed hub's (USB 3.2 10.15.2.1).
    pub const SUPERSPEED_HUB: u8 = 0x2a;
    /// A SuperSpeed endpoint's companion, after the endpoint's (USB 3.2).
    pub const SUPERSPEED_ENDPOINT_COMPANION: u8 = 0x30;
}

/// Device and interface classes (usb.org's defined class codes).
pub mod class {
    /// The device's class is in each interface's.
    pub const PER_INTERFACE: u8 = 0x00;
    /// Audio.
    pub const AUDIO: u8 = 0x01;
    /// Communications and CDC control.
    pub const COMMUNICATIONS: u8 = 0x02;
    /// Human interface devices.
    pub const HID: u8 = 0x03;
    /// Physical (force feedback).
    pub const PHYSICAL: u8 = 0x05;
    /// Still images.
    pub const IMAGE: u8 = 0x06;
    /// Printers.
    pub const PRINTER: u8 = 0x07;
    /// Mass storage.
    pub const MASS_STORAGE: u8 = 0x08;
    /// Hubs.
    pub const HUB: u8 = 0x09;
    /// CDC data.
    pub const CDC_DATA: u8 = 0x0a;
    /// Smart card readers.
    pub const SMART_CARD: u8 = 0x0b;
    /// Content security.
    pub const CONTENT_SECURITY: u8 = 0x0d;
    /// Video.
    pub const VIDEO: u8 = 0x0e;
    /// Personal healthcare.
    pub const HEALTHCARE: u8 = 0x0f;
    /// Audio and video.
    pub const AUDIO_VIDEO: u8 = 0x10;
    /// Billboards (USB Type-C alternate modes).
    pub const BILLBOARD: u8 = 0x11;
    /// USB Type-C bridges.
    pub const TYPE_C_BRIDGE: u8 = 0x12;
    /// Diagnostic devices.
    pub const DIAGNOSTIC: u8 = 0xdc;
    /// Wireless controllers, Bluetooth among them.
    pub const WIRELESS: u8 = 0xe0;
    /// Miscellaneous (interface association).
    pub const MISCELLANEOUS: u8 = 0xef;
    /// Application-specific (firmware upgrade, IrDA, test).
    pub const APPLICATION: u8 = 0xfe;
    /// Vendor-specific.
    pub const VENDOR: u8 = 0xff;
}

/// Words for a device's or an interface's class, subclass and protocol.
pub fn class_name(class: u8, subclass: u8, protocol: u8) -> &'static str {
    match (class, subclass, protocol) {
        (class::PER_INTERFACE, ..) => "composite",
        (class::AUDIO, ..) => "audio",
        (class::COMMUNICATIONS, ..) => "communications",
        // HID 1.11, 4.2 and 4.3: the boot interface subclass and its two
        // protocols.
        (class::HID, 1, 1) => "keyboard",
        (class::HID, 1, 2) => "mouse",
        (class::HID, ..) => "HID",
        (class::PHYSICAL, ..) => "physical",
        (class::IMAGE, ..) => "imaging",
        (class::PRINTER, ..) => "printer",
        (class::MASS_STORAGE, ..) => "mass storage",
        (class::HUB, ..) => "hub",
        (class::CDC_DATA, ..) => "CDC data",
        (class::SMART_CARD, ..) => "smart card reader",
        (class::CONTENT_SECURITY, ..) => "content security",
        (class::VIDEO, ..) => "video",
        (class::HEALTHCARE, ..) => "personal healthcare",
        (class::AUDIO_VIDEO, ..) => "audio and video",
        (class::BILLBOARD, ..) => "billboard",
        (class::TYPE_C_BRIDGE, ..) => "USB Type-C bridge",
        (class::DIAGNOSTIC, ..) => "diagnostic",
        (class::WIRELESS, 1, 1) => "Bluetooth",
        (class::WIRELESS, ..) => "wireless controller",
        (class::MISCELLANEOUS, ..) => "miscellaneous",
        (class::APPLICATION, ..) => "application-specific",
        (class::VENDOR, ..) => "vendor-specific",
        _ => "unknown class",
    }
}

/// The eight bytes that start a control transfer (USB 2.0, 9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    /// bmRequestType: direction, type and recipient.
    pub request_type: u8,
    /// bRequest.
    pub request: u8,
    /// wValue.
    pub value: u16,
    /// wIndex.
    pub index: u16,
    /// wLength: how many bytes the data stage moves, at most.
    pub length: u16,
}

/// bmRequestType: the data stage goes from the device to the host.
pub const DEVICE_TO_HOST: u8 = 0x80;
/// bmRequestType's types: standard, class and vendor.
pub const STANDARD: u8 = 0x00;
/// A class's request.
pub const CLASS: u8 = 0x20;
/// A vendor's request.
pub const VENDOR: u8 = 0x40;
/// bmRequestType's recipients: the device, an interface, an endpoint, or
/// something else (a hub's port).
pub const TO_DEVICE: u8 = 0;
/// An interface.
pub const TO_INTERFACE: u8 = 1;
/// An endpoint.
pub const TO_ENDPOINT: u8 = 2;
/// Something else.
pub const TO_OTHER: u8 = 3;

impl Setup {
    /// GET_DESCRIPTOR: the first `length` bytes of the descriptor of type
    /// `kind` and number `index`, in `language` for a string.
    pub fn get_descriptor(kind: u8, index: u8, language: u16, length: u16) -> Setup {
        Setup {
            request_type: DEVICE_TO_HOST | STANDARD | TO_DEVICE,
            request: request::GET_DESCRIPTOR,
            value: u16::from(kind) << 8 | u16::from(index),
            index: language,
            length,
        }
    }

    /// SET_CONFIGURATION: puts the device in the configuration `value`
    /// (bConfigurationValue), or back in the addressed state with 0.
    pub fn set_configuration(value: u8) -> Setup {
        Setup {
            request_type: STANDARD | TO_DEVICE,
            request: request::SET_CONFIGURATION,
            value: u16::from(value),
            index: 0,
            length: 0,
        }
    }

    /// Whether the data stage goes from the device to the host.
    pub fn device_to_host(&self) -> bool {
        self.request_type & DEVICE_TO_HOST != 0
    }

    /// The eight bytes as they go on the wire, little-endian, as one
    /// number: what an xHCI Setup Stage TRB holds.
    pub fn to_u64(&self) -> u64 {
        u64::from(self.request_type)
            | u64::from(self.request) << 8
            | u64::from(self.value) << 16
            | u64::from(self.index) << 32
            | u64::from(self.length) << 48
    }
}

/// The bytes of a device's descriptor.
pub const DEVICE_DESCRIPTOR_SIZE: usize = 18;

/// What a device says it is (USB 2.0, 9.6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceDescriptor {
    /// bcdUSB: the version of USB it implements, in BCD (0x0200 for 2.0).
    pub usb: u16,
    /// Its class, or [`class::PER_INTERFACE`] for a device whose
    /// interfaces each say theirs.
    pub class: u8,
    /// Its subclass.
    pub subclass: u8,
    /// Its protocol.
    pub protocol: u8,
    /// The default control pipe's packet size: bMaxPacketSize0, in bytes
    /// for USB 2 devices and as a power of two for USB 3 ones (see
    /// [`DeviceDescriptor::max_packet0`]).
    pub max_packet0_raw: u8,
    /// Its vendor ID.
    pub vendor: u16,
    /// Its product ID.
    pub product: u16,
    /// Its release, in BCD.
    pub release: u16,
    /// The string that names its maker, 0 for none.
    pub manufacturer_string: u8,
    /// The string that names the product, 0 for none.
    pub product_string: u8,
    /// The string that holds its serial number, 0 for none.
    pub serial_string: u8,
    /// How many configurations it has.
    pub configurations: u8,
}

impl DeviceDescriptor {
    /// Reads a device descriptor from what the device sent: at least its
    /// first eight bytes, which hold bMaxPacketSize0; the rest is 0 if
    /// they did not come. None if what came is not a device descriptor.
    pub fn parse(bytes: &[u8]) -> Option<DeviceDescriptor> {
        if bytes.len() < 8 || bytes[1] != descriptor::DEVICE {
            return None;
        }
        let mut all = [0u8; DEVICE_DESCRIPTOR_SIZE];
        let len = bytes.len().min(DEVICE_DESCRIPTOR_SIZE).min(usize::from(bytes[0]));
        if len < 8 {
            return None;
        }
        all[..len].copy_from_slice(&bytes[..len]);
        let word = |at: usize| u16::from_le_bytes([all[at], all[at + 1]]);
        Some(DeviceDescriptor {
            usb: word(2),
            class: all[4],
            subclass: all[5],
            protocol: all[6],
            max_packet0_raw: all[7],
            vendor: word(8),
            product: word(10),
            release: word(12),
            manufacturer_string: all[14],
            product_string: all[15],
            serial_string: all[16],
            configurations: all[17],
        })
    }

    /// The default control pipe's packet size in bytes, if it is one USB
    /// allows at `speed`: 8 at low speed; 8, 16, 32 or 64 at full speed;
    /// 64 at high speed; 512 (an exponent of 9) at SuperSpeed.
    pub fn max_packet0(&self, speed: Speed) -> Option<u16> {
        let raw = self.max_packet0_raw;
        match speed {
            Speed::Low => (raw == 8).then_some(8),
            Speed::Full => matches!(raw, 8 | 16 | 32 | 64).then_some(u16::from(raw)),
            Speed::High => (raw == 64).then_some(64),
            Speed::Super | Speed::SuperPlus => (raw == 9).then_some(512),
        }
    }
}

/// The first bytes of a configuration's descriptor, which hold its
/// length with its interfaces' and endpoints'.
pub const CONFIGURATION_HEADER_SIZE: usize = 9;

/// A configuration, from its descriptor and the interfaces', endpoints'
/// and classes' descriptors after it (USB 2.0, 9.6.3).
#[derive(Clone, Copy, Debug)]
pub struct Configuration<'a> {
    /// The descriptors, cut to the length the configuration says it has
    /// (or to what came, if less).
    bytes: &'a [u8],
    /// wTotalLength.
    pub total_length: u16,
    /// bNumInterfaces.
    pub interfaces: u8,
    /// bConfigurationValue: what SET_CONFIGURATION takes.
    pub value: u8,
    /// bmAttributes: bit 6 self-powered, bit 5 remote wakeup.
    pub attributes: u8,
    /// bMaxPower: in 2 mA units for USB 2 devices, 8 mA for USB 3 ones.
    pub max_power: u8,
}

impl<'a> Configuration<'a> {
    /// Reads a configuration from what the device sent: its first nine
    /// bytes at least (enough for [`Configuration::total_length`]). None
    /// if they are not a configuration's descriptor.
    pub fn parse(bytes: &'a [u8]) -> Option<Configuration<'a>> {
        if bytes.len() < CONFIGURATION_HEADER_SIZE
            || usize::from(bytes[0]) < CONFIGURATION_HEADER_SIZE
            || bytes[1] != descriptor::CONFIGURATION
        {
            return None;
        }
        let total_length = u16::from_le_bytes([bytes[2], bytes[3]]);
        // Never shorter than the configuration's own descriptor, whatever
        // the device says.
        let len = bytes.len().min(usize::from(total_length)).max(CONFIGURATION_HEADER_SIZE);
        Some(Configuration {
            bytes: &bytes[..len],
            total_length,
            interfaces: bytes[4],
            value: bytes[5],
            attributes: bytes[7],
            max_power: bytes[8],
        })
    }

    /// Every descriptor after the configuration's own, in order: its type
    /// and its bytes (bLength of them). A descriptor shorter than two
    /// bytes, or running past the end, ends the walk.
    pub fn descriptors(&self) -> Descriptors<'a> {
        Descriptors { bytes: self.bytes, at: usize::from(self.bytes[0]) }
    }

    /// The interfaces, each with the descriptors that follow its own up to
    /// the next interface's: its endpoints' and its class's.
    pub fn interfaces(self) -> impl Iterator<Item = Interface<'a>> {
        let mut descriptors = self.descriptors();
        core::iter::from_fn(move || {
            loop {
                let (kind, bytes) = descriptors.next()?;
                if kind != descriptor::INTERFACE {
                    continue;
                }
                let Some(descriptor) = InterfaceDescriptor::parse(bytes) else { continue };
                // The interface's own descriptors run to the next
                // interface's.
                let start = descriptors.clone();
                let count =
                    start.clone().take_while(|&(kind, _)| kind != descriptor::INTERFACE).count();
                for _ in 0..count {
                    descriptors.next();
                }
                return Some(Interface { descriptor, rest: start.take(count) });
            }
        })
    }
}

/// The descriptors in a configuration (see [`Configuration::descriptors`]).
#[derive(Clone, Debug)]
pub struct Descriptors<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Iterator for Descriptors<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<(u8, &'a [u8])> {
        let rest = self.bytes.get(self.at..)?;
        let len = usize::from(*rest.first()?);
        if len < 2 || len > rest.len() {
            self.at = self.bytes.len();
            return None;
        }
        self.at += len;
        Some((rest[1], &rest[..len]))
    }
}

/// An interface of a configuration (see [`Configuration::interfaces`]).
#[derive(Clone, Debug)]
pub struct Interface<'a> {
    /// Its descriptor.
    pub descriptor: InterfaceDescriptor,
    rest: core::iter::Take<Descriptors<'a>>,
}

impl<'a> Interface<'a> {
    /// The descriptors after the interface's own, up to the next
    /// interface's: its endpoints' and its class's.
    pub fn descriptors(&self) -> impl Iterator<Item = (u8, &'a [u8])> + use<'a> {
        self.rest.clone()
    }

    /// Its endpoints.
    pub fn endpoints(&self) -> impl Iterator<Item = EndpointDescriptor> + use<'a> {
        self.descriptors().filter_map(|(kind, bytes)| {
            (kind == descriptor::ENDPOINT).then(|| EndpointDescriptor::parse(bytes)).flatten()
        })
    }
}

/// What an interface says it is (USB 2.0, 9.6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    /// bInterfaceNumber.
    pub number: u8,
    /// bAlternateSetting.
    pub alternate: u8,
    /// How many endpoints it has besides endpoint 0.
    pub endpoints: u8,
    /// Its class.
    pub class: u8,
    /// Its subclass.
    pub subclass: u8,
    /// Its protocol.
    pub protocol: u8,
    /// The string that names it, 0 for none.
    pub string: u8,
}

impl InterfaceDescriptor {
    /// Reads an interface's descriptor (9 bytes).
    pub fn parse(bytes: &[u8]) -> Option<InterfaceDescriptor> {
        if bytes.len() < 9 || bytes[1] != descriptor::INTERFACE {
            return None;
        }
        Some(InterfaceDescriptor {
            number: bytes[2],
            alternate: bytes[3],
            endpoints: bytes[4],
            class: bytes[5],
            subclass: bytes[6],
            protocol: bytes[7],
            string: bytes[8],
        })
    }
}

/// How an endpoint moves data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferType {
    /// Requests and their answers (endpoint 0).
    Control,
    /// A steady stream with a bandwidth reserved and no retries (audio).
    Isochronous,
    /// Bulk data, when the bus has room (disks).
    Bulk,
    /// Small amounts polled at an interval (keyboards, hubs' changes).
    Interrupt,
}

/// What an endpoint is (USB 2.0, 9.6.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointDescriptor {
    /// bEndpointAddress: its number, with bit 7 set for IN (to the host).
    pub address: u8,
    /// bmAttributes: its transfer type in bits 0 and 1.
    pub attributes: u8,
    /// wMaxPacketSize: the packet size in bits 0 to 10 and, for high-speed
    /// periodic endpoints, more transactions per microframe in 11 and 12.
    pub max_packet_raw: u16,
    /// bInterval, which [`TransferType`] and the speed give a meaning.
    pub interval: u8,
}

impl EndpointDescriptor {
    /// Reads an endpoint's descriptor (7 bytes).
    pub fn parse(bytes: &[u8]) -> Option<EndpointDescriptor> {
        if bytes.len() < 7 || bytes[1] != descriptor::ENDPOINT {
            return None;
        }
        Some(EndpointDescriptor {
            address: bytes[2],
            attributes: bytes[3],
            max_packet_raw: u16::from_le_bytes([bytes[4], bytes[5]]),
            interval: bytes[6],
        })
    }

    /// Its number, 1 to 15 (0 is the default control pipe, which has no
    /// descriptor).
    pub fn number(&self) -> u8 {
        self.address & 0x0f
    }

    /// Whether it sends to the host.
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    /// How it moves data.
    pub fn transfer_type(&self) -> TransferType {
        match self.attributes & 3 {
            0 => TransferType::Control,
            1 => TransferType::Isochronous,
            2 => TransferType::Bulk,
            _ => TransferType::Interrupt,
        }
    }

    /// Its packet size in bytes.
    pub fn max_packet(&self) -> u16 {
        self.max_packet_raw & 0x7ff
    }

    /// For a high-speed periodic endpoint, the transactions it takes in a
    /// microframe after the first (0 to 2).
    pub fn extra_transactions(&self) -> u8 {
        ((self.max_packet_raw >> 11) & 3) as u8
    }
}

/// The language a device's strings are read in: the first of those
/// string descriptor 0 lists (USB 2.0, 9.6.7), or US English if it lists
/// none.
pub fn first_language(bytes: &[u8]) -> u16 {
    const US_ENGLISH: u16 = 0x0409;
    if bytes.len() >= 4 && bytes[1] == descriptor::STRING && bytes[0] >= 4 {
        u16::from_le_bytes([bytes[2], bytes[3]])
    } else {
        US_ENGLISH
    }
}

/// Writes the text of a string descriptor (UTF-16LE after its two-byte
/// header), with the spaces around it left out, a character that is not
/// UTF-16 replaced with U+FFFD and control characters left out, so that
/// what a device says cannot break a log line. Writes nothing if it is not
/// a string descriptor.
pub fn string(bytes: &[u8], out: &mut dyn fmt::Write) -> fmt::Result {
    if bytes.len() < 2 || bytes[1] != descriptor::STRING {
        return Ok(());
    }
    let len = bytes.len().min(usize::from(bytes[0]));
    let units =
        bytes.get(2..len).unwrap_or(&[]).as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair));
    // Leading and trailing spaces dropped: a space is written only once a
    // later character shows it is inside the text.
    let mut started = false;
    let mut spaces = 0usize;
    for c in char::decode_utf16(units) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        if c.is_control() || c == '\0' {
            continue;
        }
        if c.is_whitespace() {
            if started {
                spaces += 1;
            }
            continue;
        }
        for _ in 0..core::mem::take(&mut spaces) {
            out.write_char(' ')?;
        }
        started = true;
        out.write_char(c)?;
    }
    Ok(())
}
