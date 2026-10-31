// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! HID over I2C (Microsoft's *HID over I2C Protocol Specification*,
//! 1.00): a touchpad's, a touchscreen's or a keyboard's reports, read
//! over its I2C bus.
//!
//! The firmware names the device (`PNP0C50`) and, through its `_DSM`
//! ([`DSM_UUID`], function 1), the register its HID descriptor is at; a
//! register is two bytes, low first, written before what is read from it
//! or written to it. The descriptor ([`Descriptor`]) says where the rest
//! are: the report descriptor's, the input register, and the command
//! register, which takes the requests RESET and SET_POWER as two bytes
//! after it. The device says it has a report by its interrupt, a GPIO
//! pin or an interrupt line, held low until the report is read: the host
//! then reads the input register, as many bytes as the descriptor allows,
//! with no register written first, and the report's length is its first
//! two bytes, those counted. A length of 0 is not a report: the device
//! says so once it has reset.

use crate::{Error, Message};

/// The UUID of the `_DSM` function that gives the HID descriptor's
/// register, as the 16 bytes of a buffer: 3CDFF6F7-4267-4555-AD05-
/// B30A3D8938DE, its first three fields little-endian.
pub const DSM_UUID: [u8; 16] = [
    0xf7, 0xf6, 0xdf, 0x3c, 0x67, 0x42, 0x55, 0x45, 0xad, 0x05, 0xb3, 0x0a, 0x3d, 0x89, 0x38, 0xde,
];
/// The `_DSM`'s revision.
pub const DSM_REVISION: u64 = 1;
/// Its function that gives the register.
pub const DSM_FUNCTION: u64 = 1;

/// The HID descriptor's length.
pub const DESCRIPTOR_LENGTH: usize = 30;
/// The longest report descriptor read.
pub const MAX_REPORT_DESCRIPTOR: usize = 4096;
/// The longest input report read.
pub const MAX_INPUT: usize = 512;

const OPCODE_RESET: u8 = 0x1;
const OPCODE_SET_POWER: u8 = 0x8;
const POWER_ON: u8 = 0;
const POWER_SLEEP: u8 = 1;

/// A device's HID descriptor: its registers, and how long what is read
/// from them is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    /// The report descriptor's length.
    pub report_length: u16,
    /// Its register.
    pub report_register: u16,
    /// The input register.
    pub input_register: u16,
    /// The longest input report, its two bytes of length counted.
    pub max_input: u16,
    /// The output register.
    pub output_register: u16,
    /// The longest output report.
    pub max_output: u16,
    /// The command register.
    pub command_register: u16,
    /// The data register, where a command's data goes.
    pub data_register: u16,
    /// Its vendor ID.
    pub vendor: u16,
    /// Its product ID.
    pub product: u16,
    /// Its version.
    pub version: u16,
}

/// Why a device could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failed {
    /// A transfer failed.
    Bus(Error),
    /// The HID descriptor's length or version is not 1.00's: as read.
    Descriptor(u16, u16),
    /// The report descriptor is empty or longer than is read.
    ReportLength(u16),
    /// The longest input report is shorter than its length or longer than
    /// is read.
    InputLength(u16),
}

impl From<Error> for Failed {
    fn from(error: Error) -> Failed {
        Failed::Bus(error)
    }
}

impl core::fmt::Display for Failed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failed::Bus(error) => write!(f, "{error}"),
            Failed::Descriptor(length, version) => write!(
                f,
                "its HID descriptor is not HID over I2C 1.00's (length {length}, version \
                 {version:#06x})"
            ),
            Failed::ReportLength(length) => {
                write!(f, "its report descriptor's length ({length}) is not one that is read")
            }
            Failed::InputLength(length) => {
                write!(f, "its longest input report ({length} bytes) is not one that is read")
            }
        }
    }
}

/// One device's transfers: at its address, on its bus.
pub trait Device {
    /// Runs `messages`, joined by repeated starts and ended by a stop.
    fn transfer(&mut self, messages: &mut [Message<'_>]) -> Result<(), Error>;
}

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

impl Descriptor {
    /// Reads the HID descriptor at `register`, and checks it.
    pub fn read(device: &mut dyn Device, register: u16) -> Result<Descriptor, Failed> {
        let mut bytes = [0; DESCRIPTOR_LENGTH];
        device
            .transfer(&mut [Message::Write(&register.to_le_bytes()), Message::Read(&mut bytes)])?;
        Descriptor::parse(&bytes)
    }

    /// The descriptor in `bytes`, if it is one that is used.
    pub fn parse(bytes: &[u8; DESCRIPTOR_LENGTH]) -> Result<Descriptor, Failed> {
        let (length, version) = (word(bytes, 0), word(bytes, 2));
        if usize::from(length) != DESCRIPTOR_LENGTH || version != 0x0100 {
            return Err(Failed::Descriptor(length, version));
        }
        let descriptor = Descriptor {
            report_length: word(bytes, 4),
            report_register: word(bytes, 6),
            input_register: word(bytes, 8),
            max_input: word(bytes, 10),
            output_register: word(bytes, 12),
            max_output: word(bytes, 14),
            command_register: word(bytes, 16),
            data_register: word(bytes, 18),
            vendor: word(bytes, 20),
            product: word(bytes, 22),
            version: word(bytes, 24),
        };
        let report = usize::from(descriptor.report_length);
        if report == 0 || report > MAX_REPORT_DESCRIPTOR {
            return Err(Failed::ReportLength(descriptor.report_length));
        }
        let input = usize::from(descriptor.max_input);
        if !(3..=MAX_INPUT).contains(&input) {
            return Err(Failed::InputLength(descriptor.max_input));
        }
        Ok(descriptor)
    }

    /// Sends a request to the command register.
    fn command(&self, device: &mut dyn Device, low: u8, opcode: u8) -> Result<(), Error> {
        let [a, b] = self.command_register.to_le_bytes();
        device.transfer(&mut [Message::Write(&[a, b, low, opcode])])
    }

    /// Wakes the device, or puts it to sleep (SET_POWER).
    pub fn set_power(&self, device: &mut dyn Device, on: bool) -> Result<(), Error> {
        self.command(device, if on { POWER_ON } else { POWER_SLEEP }, OPCODE_SET_POWER)
    }

    /// Resets the device (RESET): it interrupts once it has, and its input
    /// register then reads a length of 0.
    pub fn reset(&self, device: &mut dyn Device) -> Result<(), Error> {
        self.command(device, 0, OPCODE_RESET)
    }

    /// Reads the report descriptor into `out`, which holds at least
    /// [`report_length`](Self::report_length) bytes; returns them.
    pub fn read_report_descriptor<'a>(
        &self,
        device: &mut dyn Device,
        out: &'a mut [u8],
    ) -> Result<&'a [u8], Failed> {
        let length = usize::from(self.report_length);
        let out = out.get_mut(..length).ok_or(Failed::ReportLength(self.report_length))?;
        device.transfer(&mut [
            Message::Write(&self.report_register.to_le_bytes()),
            Message::Read(out),
        ])?;
        Ok(out)
    }

    /// Reads the input register into `buffer`, which holds at least
    /// [`max_input`](Self::max_input) bytes.
    pub fn read_input<'a>(
        &self,
        device: &mut dyn Device,
        buffer: &'a mut [u8],
    ) -> Result<Input<'a>, Error> {
        let max = usize::from(self.max_input);
        let buffer = buffer.get_mut(..max).ok_or(Error::Invalid)?;
        device.transfer(&mut [Message::Read(buffer)])?;
        let length = usize::from(word(buffer, 0));
        Ok(match length {
            0 => Input::Reset,
            3.. if length <= max => Input::Report(&buffer[2..length]),
            _ => Input::Bad(length),
        })
    }
}

/// What the input register read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input<'a> {
    /// A report: its ID, if the device numbers them, then its data.
    Report(&'a [u8]),
    /// A length of 0: the device has reset, or has nothing.
    Reset,
    /// A length too short or too long: as read.
    Bad(usize),
}
