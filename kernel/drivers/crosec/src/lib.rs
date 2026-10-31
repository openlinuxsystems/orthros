// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Chromium embedded controller (EC) of Framework's machines (Phase
//! 11e, ADR 0207): the microcontroller that runs their fans, power LEDs
//! and, on the Framework Desktop, the ARGB header, and that the host
//! drives through the Chromium EC's host commands.
//!
//! On a PC the EC answers on the LPC bus or eSPI at fixed I/O ports:
//!
//! - **The memory map**, 255 bytes the EC keeps current and the host only
//!   reads, at 0x900, or at 0xe00 on Framework's EC (its ACPI device
//!   `FRMWC004`, where Chromebooks' is `GOOG0004`): the temperature
//!   sensors (16 bytes from 0, then 8 from 0x18), the fans' speeds (four
//!   16-bit RPMs from 0x10), "EC" at 0x20, and the host command flags at
//!   0x27, which say whether the EC takes version 3 of the protocol.
//! - **Host commands**, version 3: the host writes a request (an 8-byte
//!   header with a checksum that makes its bytes sum to 0, then the
//!   parameters) at 0x800, then 0xda to the command port 0x204, waits
//!   while the status there says the EC is busy, reads the result from
//!   the data port 0x200, then the response (a header, then the data)
//!   from 0x800.
//!
//! These come from the Chromium EC's `include/ec_commands.h`
//! (BSD-3-Clause), in Framework's fork of the EC, with Framework's own
//! commands (from 0x3e00): `EC_CMD_FAN_CONFIGURATION` gives each fan of
//! the Framework Desktop its curve, a duty that rises from its minimum at
//! one temperature of the sensor it follows to its maximum at another.
//! Linux's `cros_ec_lpcs` (GPL-2.0) is the reference for the order of the
//! steps and for which machines map the memory where; no code is taken.
//!
//! The crate has no `unsafe` code: the kernel reaches the ports for it
//! ([`Platform`]). Its tests run it against a simulated EC.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use core::fmt;

/// What the EC needs from the kernel.
pub trait Platform {
    /// Reads an I/O port.
    fn in8(&self, port: u16) -> u8;
    /// Writes an I/O port.
    fn out8(&self, port: u16, value: u8);
    /// Waits at least `us` microseconds; it may sleep.
    fn delay_us(&self, us: u64);
}

/// The data port: the result of the last command.
const HOST_DATA: u16 = 0x200;
/// The command port: the status when read.
const HOST_CMD: u16 = 0x204;
/// Where requests and responses go.
const HOST_PACKET: u16 = 0x800;
/// The most a request or response takes, header and all.
const PACKET_SIZE: usize = 0x100;

/// Where the memory map usually is.
pub const MEMMAP: u16 = 0x900;
/// Where Framework's EC (`FRMWC004`) has it.
pub const FRAMEWORK_MEMMAP: u16 = 0xe00;

/// Offsets in the memory map.
const MEMMAP_TEMP_SENSOR: u16 = 0x00;
const MEMMAP_FAN: u16 = 0x10;
const MEMMAP_TEMP_SENSOR_B: u16 = 0x18;
const MEMMAP_ID: u16 = 0x20;
const MEMMAP_THERMAL_VERSION: u16 = 0x23;
const MEMMAP_HOST_CMD_FLAGS: u16 = 0x27;
/// The host command flags: version 3 of the protocol.
const HOST_CMD_FLAG_VERSION_3: u8 = 0x02;

/// The command port's status: the host's write is not yet taken, or the
/// EC is still working on it.
const STATUS_FROM_HOST: u8 = 0x02;
const STATUS_PROCESSING: u8 = 0x04;
const STATUS_BUSY: u8 = STATUS_FROM_HOST | STATUS_PROCESSING;
/// Written to the command port: a version 3 request is at 0x800.
const PROTOCOL_3: u8 = 0xda;
/// The request and response headers' version, and their length.
const PACKET_VERSION: u8 = 3;
const HEADER: usize = 8;

/// Commands.
const CMD_HELLO: u16 = 0x0001;
const CMD_GET_VERSION: u16 = 0x0002;
const CMD_PWM_SET_FAN_DUTY: u16 = 0x0024;
const CMD_LED_CONTROL: u16 = 0x0029;
const CMD_THERMAL_AUTO_FAN_CTRL: u16 = 0x0052;
const CMD_TEMP_SENSOR_GET_INFO: u16 = 0x0070;
const CMD_RGBKBD_SET_COLOR: u16 = 0x013a;
/// Framework's: a fan's curve.
const CMD_FAN_CONFIGURATION: u16 = 0x3e27;

/// The sensors in the memory map: 16, and 8 more from version 2 of its
/// thermal data.
pub const TEMP_SENSORS: u8 = 24;
/// The fans in the memory map.
pub const FANS: u8 = 4;
/// The most LEDs `EC_CMD_RGBKBD_SET_COLOR` reaches.
pub const RGB_LEDS: usize = 128;

/// How long the EC has for a command, as Linux gives it.
const TIMEOUT_US: u64 = 1_000_000;
/// Status reads 10 µs apart before the wait goes to 1 ms steps.
const QUICK_POLLS: u64 = 100;

/// Why there is no EC at the ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Absent {
    /// The memory map does not say "EC".
    NoMap,
    /// The EC does not take version 3 of the protocol.
    OldProtocol,
}

impl fmt::Display for Absent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Absent::NoMap => "its memory map does not say EC",
            Absent::OldProtocol => "it does not take version 3 of the host command protocol",
        })
    }
}

/// Why a command failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The EC stayed busy.
    Timeout,
    /// The EC's result, not success: 1 for a command it does not have, 3
    /// for parameters it does not take, and so on.
    Status(u16),
    /// The response's header or checksum is wrong, or it is too long.
    BadResponse,
    /// The parameters do not fit in a request.
    TooBig,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Timeout => f.write_str("the EC stayed busy"),
            Error::Status(1) => f.write_str("the EC has no such command"),
            Error::Status(3) => f.write_str("the EC does not take the parameters"),
            Error::Status(status) => write!(f, "the EC answered {status}"),
            Error::BadResponse => f.write_str("the EC's response is malformed"),
            Error::TooBig => f.write_str("the request is too big"),
        }
    }
}

/// What a temperature sensor of the memory map says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Temperature {
    /// In thousandths of a degree Celsius.
    Millicelsius(i32),
    /// The EC has no such sensor.
    Absent,
    /// The sensor is there but gives no reading: it failed, is not
    /// powered or not calibrated.
    Fault,
}

impl Temperature {
    /// The memory map's byte: kelvins less 200, or one of four codes.
    pub fn from_byte(byte: u8) -> Temperature {
        match byte {
            0xff => Temperature::Absent,
            0xfc..=0xfe => Temperature::Fault,
            kelvins => Temperature::Millicelsius((i32::from(kelvins) + 200) * 1000 - 273_150),
        }
    }
}

/// What a fan of the memory map says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fan {
    /// Turning at this many revolutions a minute; 0 when stopped or
    /// stalled.
    Rpm(u16),
    /// The EC has no such fan.
    Absent,
    /// The old ECs' word for a stalled fan.
    Stalled,
}

impl Fan {
    /// The memory map's 16 bits.
    pub fn from_word(word: u16) -> Fan {
        match word {
            0xffff => Fan::Absent,
            0xfffe => Fan::Stalled,
            rpm => Fan::Rpm(rpm),
        }
    }
}

/// A Framework Desktop fan's curve (`EC_CMD_FAN_CONFIGURATION`): its duty
/// rises from `min_duty` at `min_temperature` of the sensor it follows to
/// `max_duty` at `max_temperature`; below `min_temperature` the fan
/// stops, unless `always_on`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FanCurve {
    /// The duty at `max_temperature` and above, in percent.
    pub max_duty: u8,
    /// The duty at `min_temperature`, in percent.
    pub min_duty: u8,
    /// In degrees Celsius.
    pub max_temperature: u8,
    /// In degrees Celsius.
    pub min_temperature: u8,
    /// Whether the fan runs at `min_duty` below `min_temperature`, rather
    /// than stopping.
    pub always_on: bool,
    /// The temperature sensor followed, by its number in the memory map.
    pub sensor: u8,
    /// The duty the EC gives it now; not set.
    pub duty: u8,
}

impl FanCurve {
    fn from_bytes(bytes: [u8; 7]) -> FanCurve {
        FanCurve {
            max_duty: bytes[0],
            min_duty: bytes[1],
            max_temperature: bytes[2],
            min_temperature: bytes[3],
            always_on: bytes[4] != 0,
            sensor: bytes[5],
            duty: bytes[6],
        }
    }

    fn to_bytes(self) -> [u8; 7] {
        [
            self.max_duty,
            self.min_duty,
            self.max_temperature,
            self.min_temperature,
            u8::from(self.always_on),
            self.sensor,
            self.duty,
        ]
    }
}

/// The LEDs `EC_CMD_LED_CONTROL` reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Led {
    /// The battery's charge.
    Battery = 0,
    /// The machine's power state, on its power button or case.
    Power = 1,
    /// On the power adapter.
    Adapter = 2,
    /// On the left side.
    Left = 3,
    /// On the right side.
    Right = 4,
    /// The Framework Desktop's second power LED, on its mainboard.
    SecondPower = 7,
}

impl Led {
    /// Every LED, in the EC's order.
    pub const ALL: [Led; 6] =
        [Led::Battery, Led::Power, Led::Adapter, Led::Left, Led::Right, Led::SecondPower];

    /// Its name, as Linux's `leds-cros_ec` names the function.
    pub fn name(self) -> &'static str {
        match self {
            Led::Battery => "battery",
            Led::Power => "power",
            Led::Adapter => "adapter",
            Led::Left => "left",
            Led::Right => "right",
            Led::SecondPower => "power-2",
        }
    }
}

/// An LED's colors, in the EC's order.
pub const LED_COLORS: [&str; 6] = ["red", "green", "blue", "yellow", "white", "amber"];

/// `EC_CMD_LED_CONTROL`'s flags: only ask, or give the LED back to the EC.
const LED_QUERY: u8 = 1 << 0;
const LED_AUTO: u8 = 1 << 1;

/// The image the EC runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Image {
    /// The read-only image, the one the EC starts in.
    ReadOnly,
    /// The read-write image, the one it is updated in.
    ReadWrite,
    /// It does not say.
    Unknown,
}

/// The EC, found at its ports.
pub struct Ec<P> {
    platform: P,
    memmap: u16,
}

impl<P: Platform> Ec<P> {
    /// The EC whose memory map is at port `memmap`, if it says "EC" there
    /// and takes version 3 of the protocol, as Linux checks before using
    /// it.
    pub fn probe(platform: P, memmap: u16) -> Result<Ec<P>, Absent> {
        let ec = Ec { platform, memmap };
        if ec.map(MEMMAP_ID) != b'E' || ec.map(MEMMAP_ID + 1) != b'C' {
            return Err(Absent::NoMap);
        }
        if ec.map(MEMMAP_HOST_CMD_FLAGS) & HOST_CMD_FLAG_VERSION_3 == 0 {
            return Err(Absent::OldProtocol);
        }
        Ok(ec)
    }

    /// The memory map's first port.
    pub fn memmap(&self) -> u16 {
        self.memmap
    }

    fn map(&self, offset: u16) -> u8 {
        self.platform.in8(self.memmap + offset)
    }

    /// What temperature sensor `sensor` (below [`TEMP_SENSORS`]) says.
    pub fn temperature(&self, sensor: u8) -> Temperature {
        let offset = match sensor {
            0..16 => MEMMAP_TEMP_SENSOR + u16::from(sensor),
            // The second block only from version 2 of the thermal data.
            16..TEMP_SENSORS if self.map(MEMMAP_THERMAL_VERSION) >= 2 => {
                MEMMAP_TEMP_SENSOR_B + u16::from(sensor - 16)
            }
            _ => return Temperature::Absent,
        };
        Temperature::from_byte(self.map(offset))
    }

    /// What fan `fan` (below [`FANS`]) says.
    pub fn fan(&self, fan: u8) -> Fan {
        if fan >= FANS {
            return Fan::Absent;
        }
        let at = MEMMAP_FAN + 2 * u16::from(fan);
        Fan::from_word(u16::from_le_bytes([self.map(at), self.map(at + 1)]))
    }

    /// Sends `command`, of version `version`, with `params`, and puts its
    /// response's data in `response`: returns how many bytes it took.
    /// Data beyond `response` is left unread.
    pub fn command(
        &mut self,
        command: u16,
        version: u8,
        params: &[u8],
        response: &mut [u8],
    ) -> Result<usize, Error> {
        if HEADER + params.len() > PACKET_SIZE {
            return Err(Error::TooBig);
        }
        let len = params.len() as u16;
        let mut header = [PACKET_VERSION, 0, 0, 0, version, 0, 0, 0];
        header[2..4].copy_from_slice(&command.to_le_bytes());
        header[6..8].copy_from_slice(&len.to_le_bytes());
        header[1] = sum(&header).wrapping_add(sum(params)).wrapping_neg();
        self.wait()?;
        for (at, &byte) in header.iter().chain(params).enumerate() {
            self.platform.out8(HOST_PACKET + at as u16, byte);
        }
        self.platform.out8(HOST_CMD, PROTOCOL_3);
        self.wait()?;
        let result = self.platform.in8(HOST_DATA);
        if result != 0 {
            return Err(Error::Status(u16::from(result)));
        }
        let mut header = [0; HEADER];
        for (at, byte) in header.iter_mut().enumerate() {
            *byte = self.platform.in8(HOST_PACKET + at as u16);
        }
        let status = u16::from_le_bytes([header[2], header[3]]);
        let len = usize::from(u16::from_le_bytes([header[4], header[5]]));
        if header[0] != PACKET_VERSION || HEADER + len > PACKET_SIZE {
            return Err(Error::BadResponse);
        }
        if status != 0 {
            return Err(Error::Status(status));
        }
        let mut checksum = sum(&header);
        for at in 0..len {
            let byte = self.platform.in8(HOST_PACKET + (HEADER + at) as u16);
            checksum = checksum.wrapping_add(byte);
            if let Some(slot) = response.get_mut(at) {
                *slot = byte;
            }
        }
        if checksum != 0 {
            return Err(Error::BadResponse);
        }
        Ok(len.min(response.len()))
    }

    /// Waits while the EC is busy.
    fn wait(&self) -> Result<(), Error> {
        let mut waited = 0;
        let mut polls = 0;
        while self.platform.in8(HOST_CMD) & STATUS_BUSY != 0 {
            if waited >= TIMEOUT_US {
                return Err(Error::Timeout);
            }
            let step = if polls < QUICK_POLLS { 10 } else { 1000 };
            self.platform.delay_us(step);
            waited += step;
            polls += 1;
        }
        Ok(())
    }

    /// `EC_CMD_HELLO`: the EC must answer what it was given plus
    /// 0x01020304.
    pub fn hello(&mut self) -> Result<(), Error> {
        let given: u32 = 0xa0b0_c0d0;
        let mut out = [0; 4];
        self.command(CMD_HELLO, 0, &given.to_le_bytes(), &mut out)?;
        if u32::from_le_bytes(out) != given.wrapping_add(0x0102_0304) {
            return Err(Error::BadResponse);
        }
        Ok(())
    }

    /// `EC_CMD_GET_VERSION`: writes the version of the image the EC runs
    /// to `out`, and says which image that is.
    pub fn version(&mut self, out: &mut dyn fmt::Write) -> Result<Image, Error> {
        let mut response = [0; 100];
        let len = self.command(CMD_GET_VERSION, 0, &[], &mut response)?;
        if len < 100 {
            return Err(Error::BadResponse);
        }
        let image =
            match u32::from_le_bytes([response[96], response[97], response[98], response[99]]) {
                1 | 3 => Image::ReadOnly,
                2 | 4 => Image::ReadWrite,
                _ => Image::Unknown,
            };
        let string = if image == Image::ReadOnly { &response[..32] } else { &response[32..64] };
        let _ = out.write_str(text(string));
        Ok(image)
    }

    /// `EC_CMD_TEMP_SENSOR_GET_INFO`: writes temperature sensor `sensor`'s
    /// name to `out`.
    pub fn sensor_name(&mut self, sensor: u8, out: &mut dyn fmt::Write) -> Result<(), Error> {
        let mut response = [0; 33];
        let len = self.command(CMD_TEMP_SENSOR_GET_INFO, 0, &[sensor], &mut response)?;
        let _ = out.write_str(text(&response[..len.min(32)]));
        Ok(())
    }

    /// `EC_CMD_PWM_SET_FAN_DUTY` (version 1): runs fan `fan` at `percent`
    /// of its duty, which takes it from the EC's thermal control until
    /// [`Ec::auto_fan`].
    pub fn set_fan_duty(&mut self, fan: u8, percent: u8) -> Result<(), Error> {
        let mut params = [0; 5];
        params[..4].copy_from_slice(&u32::from(percent.min(100)).to_le_bytes());
        params[4] = fan;
        self.command(CMD_PWM_SET_FAN_DUTY, 1, &params, &mut []).map(|_| ())
    }

    /// `EC_CMD_THERMAL_AUTO_FAN_CTRL` (version 1): gives fan `fan` back to
    /// the EC's thermal control.
    pub fn auto_fan(&mut self, fan: u8) -> Result<(), Error> {
        self.command(CMD_THERMAL_AUTO_FAN_CTRL, 1, &[fan], &mut []).map(|_| ())
    }

    /// Framework's `EC_CMD_FAN_CONFIGURATION`: fan `fan`'s curve.
    pub fn fan_curve(&mut self, fan: u8) -> Result<FanCurve, Error> {
        self.fan_configuration(fan, None)
    }

    /// Framework's `EC_CMD_FAN_CONFIGURATION`: sets fan `fan`'s curve, and
    /// returns it as the EC keeps it.
    pub fn set_fan_curve(&mut self, fan: u8, curve: FanCurve) -> Result<FanCurve, Error> {
        self.fan_configuration(fan, Some(curve))
    }

    fn fan_configuration(&mut self, fan: u8, set: Option<FanCurve>) -> Result<FanCurve, Error> {
        let mut params = [0; 9];
        params[0] = fan;
        if let Some(curve) = set {
            params[1] = 1;
            params[2..].copy_from_slice(&curve.to_bytes());
        }
        let mut response = [0; 7];
        if self.command(CMD_FAN_CONFIGURATION, 0, &params, &mut response)? < 7 {
            return Err(Error::BadResponse);
        }
        Ok(FanCurve::from_bytes(response))
    }

    /// `EC_CMD_LED_CONTROL`, asking only: the brightest each of `led`'s
    /// colors goes ([`LED_COLORS`]), 0 for those it lacks. Fails for an
    /// LED the EC does not have.
    pub fn led_range(&mut self, led: Led) -> Result<[u8; 6], Error> {
        self.led_control(led, LED_QUERY, [0; 6])
    }

    /// `EC_CMD_LED_CONTROL`: lights `led` at `brightness` in each of its
    /// colors, which takes it from the EC until [`Ec::auto_led`].
    pub fn set_led(&mut self, led: Led, brightness: [u8; 6]) -> Result<(), Error> {
        self.led_control(led, 0, brightness).map(|_| ())
    }

    /// `EC_CMD_LED_CONTROL`: gives `led` back to the EC, which lights it
    /// for the machine's state.
    pub fn auto_led(&mut self, led: Led) -> Result<(), Error> {
        self.led_control(led, LED_AUTO, [0; 6]).map(|_| ())
    }

    fn led_control(&mut self, led: Led, flags: u8, brightness: [u8; 6]) -> Result<[u8; 6], Error> {
        let mut params = [0; 8];
        params[0] = led as u8;
        params[1] = flags;
        params[2..].copy_from_slice(&brightness);
        let mut range = [0; 6];
        self.command(CMD_LED_CONTROL, 1, &params, &mut range)?;
        Ok(range)
    }

    /// `EC_CMD_RGBKBD_SET_COLOR`: gives the LEDs from `first` on the
    /// colors `colors` (red, green, blue), in as many requests as they
    /// take. On the Framework Desktop these are the ARGB header's LEDs.
    pub fn set_rgb(&mut self, first: u8, colors: &[[u8; 3]]) -> Result<(), Error> {
        if usize::from(first) + colors.len() > RGB_LEDS {
            return Err(Error::TooBig);
        }
        const PER_REQUEST: usize = (PACKET_SIZE - HEADER - 2) / 3;
        let mut params = [0; 2 + 3 * PER_REQUEST];
        for (chunk, part) in colors.chunks(PER_REQUEST).enumerate() {
            params[0] = first + (chunk * PER_REQUEST) as u8;
            params[1] = part.len() as u8;
            for (at, color) in part.iter().enumerate() {
                params[2 + 3 * at..5 + 3 * at].copy_from_slice(color);
            }
            self.command(CMD_RGBKBD_SET_COLOR, 0, &params[..2 + 3 * part.len()], &mut [])?;
        }
        Ok(())
    }
}

/// The bytes' sum.
fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |sum, &byte| sum.wrapping_add(byte))
}

/// A string the EC wrote: to its first NUL, and only while it is UTF-8.
fn text(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
    match core::str::from_utf8(&bytes[..end]) {
        Ok(text) => text,
        Err(error) => core::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or(""),
    }
}
