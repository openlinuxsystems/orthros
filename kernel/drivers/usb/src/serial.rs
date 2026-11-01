// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Serial ports on USB (Phase 11g, ADR 0246): the Abstract Control Model
//! of the *Universal Serial Bus Communications Class Subclass
//! Specification for PSTN Devices, Revision 1.2*, which modems, boards
//! such as the Arduino and the Raspberry Pi Pico, and many adapters speak
//! ([`acm`]); and FTDI's chips, which have requests of their own, as
//! FTDI's application notes AN232B-04 and AN232B-05 and the chips' data
//! sheets describe them ([`ftdi`]). QEMU's `usb-serial` is an FTDI chip.
//!
//! Either way a port is a pair of bulk endpoints, one each way, that carry
//! the bytes; [`ports`] finds them in a configuration. What a terminal's
//! settings say of the line, its speed, its character size, its parity and
//! stop bits and its flow control, is a [`Line`], read from a Linux
//! `c_cflag` ([`Line::from_cflag`]) and sent to the device as its
//! protocol asks.

use crate::{Configuration, DeviceDescriptor, EndpointDescriptor, TransferType, class};

/// The bits of a Linux `c_cflag` that say how the line goes.
pub mod cflag {
    /// The speed's code (`CBAUD`), with [`BOTHER`] among its bits.
    pub const CBAUD: u32 = 0o10017;
    /// The speed is the one `c_ospeed` holds, not a code.
    pub const BOTHER: u32 = 0o10000;
    /// The character size (`CSIZE`): `CS5` is 0, `CS6`, `CS7`, `CS8`.
    pub const CSIZE: u32 = 0o60;
    /// Two stop bits.
    pub const CSTOPB: u32 = 0o100;
    /// A parity bit is sent and checked.
    pub const PARENB: u32 = 0o400;
    /// Odd parity (with [`CMSPAR`], mark).
    pub const PARODD: u32 = 0o1000;
    /// The modem lines are lowered on the last close.
    pub const HUPCL: u32 = 0o2000;
    /// The modem's carrier is not waited for.
    pub const CLOCAL: u32 = 0o4000;
    /// Mark or space parity.
    pub const CMSPAR: u32 = 0o10000000000;
    /// RTS and CTS flow control.
    pub const CRTSCTS: u32 = 0o20000000000;
}

/// Linux's speed codes, by code, and the bits per second each is.
const RATES: [(u32, u32); 31] = [
    (0o0, 0),
    (0o1, 50),
    (0o2, 75),
    (0o3, 110),
    (0o4, 134),
    (0o5, 150),
    (0o6, 200),
    (0o7, 300),
    (0o10, 600),
    (0o11, 1200),
    (0o12, 1800),
    (0o13, 2400),
    (0o14, 4800),
    (0o15, 9600),
    (0o16, 19200),
    (0o17, 38400),
    (0o10001, 57600),
    (0o10002, 115_200),
    (0o10003, 230_400),
    (0o10004, 460_800),
    (0o10005, 500_000),
    (0o10006, 576_000),
    (0o10007, 921_600),
    (0o10010, 1_000_000),
    (0o10011, 1_152_000),
    (0o10012, 1_500_000),
    (0o10013, 2_000_000),
    (0o10014, 2_500_000),
    (0o10015, 3_000_000),
    (0o10016, 3_500_000),
    (0o10017, 4_000_000),
];

/// Linux's B9600, the speed a serial port starts at.
pub const B9600: u32 = 0o15;

/// The bits per second of Linux's speed code `code`, if it is one.
pub fn rate_of(code: u32) -> Option<u32> {
    RATES.iter().find(|&&(c, _)| c == code).map(|&(_, rate)| rate)
}

/// A parity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parity {
    /// No parity bit.
    None,
    /// The bit makes the ones odd.
    Odd,
    /// The bit makes the ones even.
    Even,
    /// The bit is always 1.
    Mark,
    /// The bit is always 0.
    Space,
}

/// How many stop bits end a character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// One.
    One,
    /// One and a half, which a 5-bit character has instead of two.
    OneAndHalf,
    /// Two.
    Two,
}

/// How a line goes, as a terminal's settings say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    /// Bits per second; 0 hangs the line up (B0), lowering DTR and RTS.
    pub rate: u32,
    /// Bits in a character, 5 to 8.
    pub data_bits: u8,
    /// Its parity.
    pub parity: Parity,
    /// Its stop bits.
    pub stop: Stop,
    /// RTS and CTS flow control.
    pub rts_cts: bool,
}

impl Line {
    /// The line a Linux `c_cflag` says, with `ospeed` the speed in bits per
    /// second if the cflag's code is [`cflag::BOTHER`]; a code Linux does
    /// not have is 9600.
    pub fn from_cflag(flags: u32, ospeed: u32) -> Line {
        let code = flags & cflag::CBAUD;
        let rate = if code == cflag::BOTHER { ospeed } else { rate_of(code).unwrap_or(9600) };
        let data_bits = 5 + ((flags & cflag::CSIZE) >> 4) as u8;
        let parity = match (
            flags & cflag::PARENB != 0,
            flags & cflag::CMSPAR != 0,
            flags & cflag::PARODD != 0,
        ) {
            (false, ..) => Parity::None,
            (true, false, true) => Parity::Odd,
            (true, false, false) => Parity::Even,
            (true, true, true) => Parity::Mark,
            (true, true, false) => Parity::Space,
        };
        let stop = match (flags & cflag::CSTOPB != 0, data_bits) {
            (false, _) => Stop::One,
            (true, 5) => Stop::OneAndHalf,
            (true, _) => Stop::Two,
        };
        Line { rate, data_bits, parity, stop, rts_cts: flags & cflag::CRTSCTS != 0 }
    }
}

/// What carries a port's settings and modem lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// The Abstract Control Model, through the communications interface
    /// `control`.
    Acm {
        /// The communications interface's number.
        control: u8,
    },
    /// An FTDI chip's vendor requests, for its port `index` (its wIndex).
    Ftdi {
        /// The chip.
        chip: ftdi::Chip,
        /// The port's wIndex: 0 on a chip of one port, else 1 and on.
        index: u16,
    },
}

/// A serial port of a device's configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Port {
    /// What carries its settings.
    pub protocol: Protocol,
    /// The interface whose endpoints carry its bytes.
    pub data: u8,
    /// Its bulk IN endpoint, which brings what the line received.
    pub bulk_in: EndpointDescriptor,
    /// Its bulk OUT endpoint, which takes what the line sends.
    pub bulk_out: EndpointDescriptor,
}

impl Port {
    /// The driver Linux names for it, as `lsusb -t` shows.
    pub fn driver(&self) -> &'static str {
        match self.protocol {
            Protocol::Acm { .. } => "cdc_acm",
            Protocol::Ftdi { .. } => "ftdi_sio",
        }
    }

    /// The prefix of its terminal's name, as Linux names it: `ttyACM` or
    /// `ttyUSB`.
    pub fn prefix(&self) -> &'static str {
        match self.protocol {
            Protocol::Acm { .. } => "ttyACM",
            Protocol::Ftdi { .. } => "ttyUSB",
        }
    }
}

/// The first bulk IN and bulk OUT endpoints of the first setting of the
/// interface `number` of `config`.
fn bulk_pair(
    config: &Configuration<'_>,
    number: u8,
) -> Option<(EndpointDescriptor, EndpointDescriptor)> {
    let interface = config
        .interfaces()
        .find(|i| i.descriptor.number == number && i.descriptor.alternate == 0)?;
    let bulk = |into: bool| {
        interface.endpoints().find(|e| e.transfer_type() == TransferType::Bulk && e.is_in() == into)
    };
    let (input, output) = (bulk(true)?, bulk(false)?);
    (input.max_packet() > 0 && output.max_packet() > 0).then_some((input, output))
}

/// Writes the serial ports of `device`'s configuration `config` to `out`,
/// as many as fit; says how many there are.
pub fn ports(device: &DeviceDescriptor, config: &Configuration<'_>, out: &mut [Port]) -> usize {
    let mut count = 0;
    let mut add = |port: Port| {
        if let Some(slot) = out.get_mut(count) {
            *slot = port;
        }
        count += 1;
    };
    if let Some(chip) = ftdi::chip(device) {
        for interface in config.interfaces().filter(|i| i.descriptor.alternate == 0) {
            let number = interface.descriptor.number;
            if let Some((bulk_in, bulk_out)) = bulk_pair(config, number) {
                let index = if chip.ports() > 1 { u16::from(number) + 1 } else { 0 };
                add(Port {
                    protocol: Protocol::Ftdi { chip, index },
                    data: number,
                    bulk_in,
                    bulk_out,
                });
            }
        }
        return count;
    }
    for interface in config.interfaces() {
        let d = interface.descriptor;
        if d.alternate != 0 || d.class != class::COMMUNICATIONS || d.subclass != acm::SUBCLASS {
            continue;
        }
        // The union names the data interface; else the call management
        // descriptor does; else it is the next one, as Linux guesses.
        let mut data = None;
        let mut managed = None;
        for (kind, bytes) in interface.descriptors() {
            if kind != acm::CS_INTERFACE || bytes.len() < 4 {
                continue;
            }
            match bytes[2] {
                acm::UNION if bytes.len() >= 5 => data = data.or(Some(bytes[4])),
                acm::CALL_MANAGEMENT if bytes.len() >= 5 => managed = Some(bytes[4]),
                _ => {}
            }
        }
        let data = data.or(managed).unwrap_or(d.number.wrapping_add(1));
        if let Some((bulk_in, bulk_out)) = bulk_pair(config, data) {
            add(Port { protocol: Protocol::Acm { control: d.number }, data, bulk_in, bulk_out });
        }
    }
    count
}

/// The Abstract Control Model (CDC PSTN 1.2).
pub mod acm {
    use super::{Line, Parity, Stop};
    use crate::{CLASS, Setup, TO_INTERFACE};

    /// The communications interface's subclass (CDC 1.2, table 4).
    pub const SUBCLASS: u8 = 0x02;
    /// The class-specific descriptor type of an interface's functional
    /// descriptors (CDC 1.2, table 12).
    pub const CS_INTERFACE: u8 = 0x24;
    /// Functional descriptor subtypes (CDC 1.2, table 13): call management
    /// (bDataInterface at byte 4) and the union (its first subordinate
    /// interface at byte 4).
    pub const CALL_MANAGEMENT: u8 = 0x01;
    /// The union.
    pub const UNION: u8 = 0x06;

    /// The model's requests (PSTN 1.2, table 13).
    pub mod request {
        /// SEND_ENCAPSULATED_COMMAND.
        pub const SEND_ENCAPSULATED_COMMAND: u8 = 0x00;
        /// SET_LINE_CODING.
        pub const SET_LINE_CODING: u8 = 0x20;
        /// GET_LINE_CODING.
        pub const GET_LINE_CODING: u8 = 0x21;
        /// SET_CONTROL_LINE_STATE.
        pub const SET_CONTROL_LINE_STATE: u8 = 0x22;
        /// SEND_BREAK.
        pub const SEND_BREAK: u8 = 0x23;
    }

    /// SET_CONTROL_LINE_STATE's bits: DTR, and RTS.
    pub const DTR: u16 = 1;
    /// RTS.
    pub const RTS: u16 = 2;

    /// The bytes of a line coding (PSTN 1.2, table 17).
    pub const LINE_CODING_SIZE: usize = 7;

    /// The line coding of `line` (PSTN 1.2, table 17): dwDTERate,
    /// bCharFormat, bParityType, bDataBits.
    pub fn line_coding(line: &Line) -> [u8; LINE_CODING_SIZE] {
        let mut out = [0u8; LINE_CODING_SIZE];
        out[..4].copy_from_slice(&line.rate.to_le_bytes());
        out[4] = match line.stop {
            Stop::One => 0,
            Stop::OneAndHalf => 1,
            Stop::Two => 2,
        };
        out[5] = match line.parity {
            Parity::None => 0,
            Parity::Odd => 1,
            Parity::Even => 2,
            Parity::Mark => 3,
            Parity::Space => 4,
        };
        out[6] = line.data_bits;
        out
    }

    impl Setup {
        /// SET_LINE_CODING to the communications interface `interface`,
        /// with [`line_coding`]'s seven bytes.
        pub fn set_line_coding(interface: u8) -> Setup {
            Setup {
                request_type: CLASS | TO_INTERFACE,
                request: request::SET_LINE_CODING,
                value: 0,
                index: u16::from(interface),
                length: LINE_CODING_SIZE as u16,
            }
        }

        /// SET_CONTROL_LINE_STATE: raises the lines `lines` ([`DTR`],
        /// [`RTS`]) and lowers the others.
        pub fn set_control_lines(interface: u8, lines: u16) -> Setup {
            Setup {
                request_type: CLASS | TO_INTERFACE,
                request: request::SET_CONTROL_LINE_STATE,
                value: lines,
                index: u16::from(interface),
                length: 0,
            }
        }
    }
}

/// FTDI's chips.
pub mod ftdi {
    use super::{Line, Parity, Stop};
    use crate::{DeviceDescriptor, Setup, TO_DEVICE, VENDOR};

    /// FTDI's vendor ID.
    pub const VENDOR_ID: u16 = 0x0403;
    /// The product IDs of the chips as they leave FTDI: FT232AM, BM and R
    /// (and QEMU's), FT2232, FT4232H, FT232H and the FT-X series.
    pub const PRODUCTS: [u16; 5] = [0x6001, 0x6010, 0x6011, 0x6014, 0x6015];

    /// The chips, as bcdDevice tells them apart.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Chip {
        /// FT8U232AM.
        Am,
        /// FT232BM, and FT245BM.
        Bm,
        /// FT2232C, D and L: two ports.
        Ft2232c,
        /// FT232R and FT245R.
        Ft232r,
        /// FT2232H: two ports, high speed.
        Ft2232h,
        /// FT4232H: four ports, high speed.
        Ft4232h,
        /// FT232H: high speed.
        Ft232h,
        /// The FT-X series (FT230X, FT231X and others).
        FtX,
    }

    impl Chip {
        /// How many ports it has.
        pub fn ports(self) -> u8 {
            match self {
                Chip::Ft2232c | Chip::Ft2232h => 2,
                Chip::Ft4232h => 4,
                _ => 1,
            }
        }

        /// Whether it has the 120 MHz clock of the H series.
        pub fn high_speed(self) -> bool {
            matches!(self, Chip::Ft2232h | Chip::Ft4232h | Chip::Ft232h)
        }

        /// Its name.
        pub fn name(self) -> &'static str {
            match self {
                Chip::Am => "FT8U232AM",
                Chip::Bm => "FT232BM",
                Chip::Ft2232c => "FT2232C",
                Chip::Ft232r => "FT232R",
                Chip::Ft2232h => "FT2232H",
                Chip::Ft4232h => "FT4232H",
                Chip::Ft232h => "FT232H",
                Chip::FtX => "FT-X",
            }
        }
    }

    /// The chip `device` is, if it is one of FTDI's.
    pub fn chip(device: &DeviceDescriptor) -> Option<Chip> {
        if device.vendor != VENDOR_ID || !PRODUCTS.contains(&device.product) {
            return None;
        }
        Some(match device.release {
            0x0200 => Chip::Am,
            0x0500 => Chip::Ft2232c,
            0x0600 => Chip::Ft232r,
            0x0700 => Chip::Ft2232h,
            0x0800 => Chip::Ft4232h,
            0x0900 => Chip::Ft232h,
            0x1000 => Chip::FtX,
            _ => match device.product {
                0x6010 => Chip::Ft2232h,
                0x6011 => Chip::Ft4232h,
                0x6014 => Chip::Ft232h,
                0x6015 => Chip::FtX,
                _ => Chip::Bm,
            },
        })
    }

    /// The vendor requests (AN232B-04 and the D2XX programmer's guide).
    pub mod request {
        /// Resets the port ([`super::RESET_SIO`]) or purges a buffer.
        pub const RESET: u8 = 0;
        /// Sets DTR and RTS.
        pub const MODEM_CONTROL: u8 = 1;
        /// Sets the flow control.
        pub const FLOW_CONTROL: u8 = 2;
        /// Sets the baud rate's divisor.
        pub const BAUD_RATE: u8 = 3;
        /// Sets the character size, parity and stop bits, and the break.
        pub const DATA: u8 = 4;
        /// Sets how long the chip holds a short packet back, in ms.
        pub const LATENCY_TIMER: u8 = 9;
    }

    /// RESET's wValue: the whole port.
    pub const RESET_SIO: u16 = 0;
    /// MODEM_CONTROL's wValue bits: DTR and RTS, each with the bit that
    /// says it is set (8 above it).
    pub const DTR: u16 = 0x0101;
    /// RTS.
    pub const RTS: u16 = 0x0202;
    /// FLOW_CONTROL's wIndex high byte: RTS and CTS.
    pub const FLOW_RTS_CTS: u16 = 0x0100;

    /// The two bytes of modem and line status that start each packet the
    /// chip sends.
    pub const STATUS_SIZE: usize = 2;

    /// A vendor request to the chip's port `index`.
    fn vendor(request: u8, value: u16, index: u16) -> Setup {
        Setup { request_type: VENDOR | TO_DEVICE, request, value, index, length: 0 }
    }

    /// RESET of port `index`.
    pub fn reset(index: u16) -> Setup {
        vendor(request::RESET, RESET_SIO, index)
    }

    /// MODEM_CONTROL: DTR and RTS raised if `up`, else lowered.
    pub fn modem_control(index: u16, up: bool) -> Setup {
        let lines = if up { DTR | RTS } else { DTR & 0xff00 | RTS & 0xff00 };
        vendor(request::MODEM_CONTROL, lines, index)
    }

    /// FLOW_CONTROL: RTS and CTS if `rts_cts`, else none.
    pub fn flow_control(index: u16, rts_cts: bool) -> Setup {
        vendor(request::FLOW_CONTROL, 0, index | if rts_cts { FLOW_RTS_CTS } else { 0 })
    }

    /// DATA: `line`'s character size, parity and stop bits.
    pub fn data(index: u16, line: &Line) -> Setup {
        let parity = match line.parity {
            Parity::None => 0,
            Parity::Odd => 1,
            Parity::Even => 2,
            Parity::Mark => 3,
            Parity::Space => 4,
        };
        let stop = match line.stop {
            Stop::One => 0,
            Stop::OneAndHalf => 1,
            Stop::Two => 2,
        };
        vendor(request::DATA, u16::from(line.data_bits) | parity << 8 | stop << 11, index)
    }

    /// BAUD_RATE: the divisor nearest `rate` bits per second for `chip`,
    /// or `None` if the chip cannot go that fast or slow.
    pub fn baud_rate(chip: Chip, index: u16, rate: u32) -> Option<Setup> {
        let encoded = divisor(chip, rate)?;
        // The divisor's high bits go in wIndex, above the port's number
        // on the chips with several.
        let high = (encoded >> 16) as u16;
        let index = if chip.ports() > 1 { index & 0xff | high << 8 } else { index | high };
        Some(vendor(request::BAUD_RATE, encoded as u16, index))
    }

    /// The code of each eighth of a divisor's fraction (AN232B-05).
    const EIGHTHS: [u32; 8] = [0, 3, 2, 4, 1, 5, 6, 7];

    /// The divisor of `rate` as the chip takes it (AN232B-05): 14 bits of
    /// its whole part, then three of eighths, of a 3 MHz clock (48 MHz
    /// over 16), or with bit 17 set of the H series' 12 MHz (120 MHz over
    /// 10). The AM chip has only halves and quarters and one eighth.
    pub fn divisor(chip: Chip, rate: u32) -> Option<u32> {
        if rate == 0 {
            return None;
        }
        let (clock, flag) = if chip.high_speed() && rate >= 1200 {
            (12_000_000u64, 1u32 << 17)
        } else {
            (3_000_000u64, 0)
        };
        if u64::from(rate) > clock {
            return None;
        }
        let mut eighths = (clock * 8 + u64::from(rate) / 2) / u64::from(rate);
        if chip == Chip::Am {
            // Rounded to what it has: 0, 1/8, 1/4 and 1/2.
            let whole = eighths & !7;
            eighths = whole
                + match eighths & 7 {
                    0 => 0,
                    1 => 1,
                    2 | 3 => 2,
                    4 | 5 => 4,
                    _ => 8,
                };
        }
        // Divisors below 2 are only 1 (encoded 0) and 1.5 (encoded 1).
        let encoded = match eighths {
            0..=9 => 0,
            10..=13 => 1,
            14..=15 => 2,
            _ if eighths >> 3 > 0x3fff => return None,
            _ => (eighths >> 3) as u32 | EIGHTHS[(eighths & 7) as usize] << 14,
        };
        Some(encoded | flag)
    }

    /// The rate in bits per second the encoded divisor `encoded` gives,
    /// as the chip reads it: the inverse of [`divisor`].
    pub fn rate_of_divisor(chip: Chip, encoded: u32) -> u32 {
        let clock: u64 =
            if chip.high_speed() && encoded & 1 << 17 != 0 { 12_000_000 } else { 3_000_000 };
        let whole = u64::from(encoded & 0x3fff);
        let code = (encoded >> 14) & 7;
        let eighth = EIGHTHS.iter().position(|&c| c == code).unwrap_or(0) as u64;
        match (whole, eighth) {
            (0, 0) => clock as u32,
            (1, 0) => (clock * 2 / 3) as u32,
            _ => ((clock * 8 + (whole * 8 + eighth) / 2) / (whole * 8 + eighth)) as u32,
        }
    }

    /// Copies the data of a transfer the chip sent, `came`, to `out`, the
    /// two status bytes at the start of each packet of `max_packet`
    /// bytes left out; says how many bytes it copied.
    pub fn strip(came: &[u8], max_packet: usize, out: &mut [u8]) -> usize {
        let mut done = 0;
        for packet in came.chunks(max_packet.max(STATUS_SIZE + 1)) {
            let data = packet.get(STATUS_SIZE..).unwrap_or(&[]);
            let n = data.len().min(out.len() - done);
            out[done..done + n].copy_from_slice(&data[..n]);
            done += n;
        }
        done
    }
}
