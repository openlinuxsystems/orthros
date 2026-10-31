// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods of the platform capability (Phase 11, radix-platform(2),
//! ADR 0208): the controls the machine's embedded controller keeps, its
//! fans' duties and curves, its LEDs and its ARGB header, through
//! [`crate::crosec`]; and the firmware's SMBIOS tables whole (ADR 0231).
//!
//! The arguments are checked here before anything reaches the EC, which
//! takes what it is sent as it is: Framework's EC copies a fan curve
//! without looking at it, and divides by its two temperatures' difference.

use radix_abi::Status;
use radix_abi::errors::{File, Kernel};
use radix_abi::platform::{
    self as abi, AutoFanArgs, AutoLedArgs, Controls, DescribeArgs, DescribeResults, FanCurveArgs,
    FanCurveResults, LedArgs, LedResults, SetFanCurveArgs, SetFanDutyArgs, SetLedArgs, SetRgbArgs,
    SmbiosArgs, SmbiosResults, SmbiosTable,
};
use radix_cap::{Capability, ObjectKind, Rights};
use radix_crosec::{Error, FanCurve};
use radix_hal::{Platform, UserMode};

use super::methods::{Answer, answer};
use super::{Result, UserRef, cap_error, error};
use crate::Arch;
use crate::crosec::{self, Failed};

/// The addressable LEDs a request to the EC takes at most, copied from
/// the caller a request at a time.
const RGB_CHUNK: usize = 82;

/// Calls the method `label` on `cap`, a platform capability, for `user`:
/// kept out of line, so that [`super::methods::call`]'s frame, under every
/// method and the deep ones like a mount's, does not carry its own.
#[inline(never)]
pub fn method(user: &UserRef, cap: &Capability, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let check = |rights| cap.check(ObjectKind::Platform, rights).map_err(cap_error);
    match label {
        abi::DESCRIBE => {
            DescribeArgs::decode(args)?;
            check(Rights::READ)?;
            let (controls, fans, sensors, leds) =
                crosec::describe().ok_or(error(Kernel::NotSupported))?;
            let results = DescribeResults {
                controls: Controls(u32::from(controls)),
                fans: u32::from(fans),
                sensors,
                leds,
            };
            answer(label, DescribeResults::WORDS, results.encode())
        }
        abi::SET_FAN_DUTY => {
            let a = SetFanDutyArgs::decode(args)?;
            check(Rights::CONTROL)?;
            let percent = u8::try_from(a.percent)
                .ok()
                .filter(|&percent| percent <= 100)
                .ok_or(error(Kernel::InvalidArgument))?;
            let name = user.process.name();
            crosec::set_fan_duty(a.fan, percent, name.as_str()).map_err(failed)?;
            answer(label, 0, [0; 8])
        }
        abi::AUTO_FAN => {
            let a = AutoFanArgs::decode(args)?;
            check(Rights::CONTROL)?;
            crosec::auto_fan(a.fan, user.process.name().as_str()).map_err(failed)?;
            answer(label, 0, [0; 8])
        }
        abi::FAN_CURVE => {
            let a = FanCurveArgs::decode(args)?;
            check(Rights::READ)?;
            let curve = crosec::fan_curve(a.fan).map_err(failed)?;
            let results = FanCurveResults {
                min_temperature: u32::from(curve.min_temperature),
                max_temperature: u32::from(curve.max_temperature),
                min_duty: u32::from(curve.min_duty),
                max_duty: u32::from(curve.max_duty),
                always_on: curve.always_on,
                sensor: u32::from(curve.sensor),
                duty: u32::from(curve.duty),
            };
            answer(label, FanCurveResults::WORDS, results.encode())
        }
        abi::SET_FAN_CURVE => {
            let a = SetFanCurveArgs::decode(args)?;
            check(Rights::CONTROL)?;
            let byte = |value: u32| u8::try_from(value).map_err(|_| error(Kernel::InvalidArgument));
            let curve = FanCurve {
                max_duty: byte(a.max_duty)?,
                min_duty: byte(a.min_duty)?,
                max_temperature: byte(a.max_temperature)?,
                min_temperature: byte(a.min_temperature)?,
                always_on: a.always_on,
                sensor: u8::try_from(a.sensor).map_err(|_| error(Kernel::OutOfRange))?,
                duty: 0,
            };
            if curve.min_temperature >= curve.max_temperature
                || curve.min_duty > curve.max_duty
                || curve.max_duty > 100
            {
                return Err(error(Kernel::InvalidArgument));
            }
            crosec::set_fan_curve(a.fan, curve).map_err(failed)?;
            answer(label, 0, [0; 8])
        }
        abi::LED => {
            let a = LedArgs::decode(args)?;
            check(Rights::READ)?;
            let [red, green, blue, yellow, white, amber] =
                crosec::led_range(a.led.raw()).map_err(failed)?.map(u32::from);
            let results = LedResults { red, green, blue, yellow, white, amber };
            answer(label, LedResults::WORDS, results.encode())
        }
        abi::SET_LED => {
            let a = SetLedArgs::decode(args)?;
            check(Rights::CONTROL)?;
            let mut brightness = [0; 6];
            for (to, from) in
                brightness.iter_mut().zip([a.red, a.green, a.blue, a.yellow, a.white, a.amber])
            {
                *to = u8::try_from(from).map_err(|_| error(Kernel::InvalidArgument))?;
            }
            crosec::set_led(a.led.raw(), Some(brightness)).map_err(failed)?;
            answer(label, 0, [0; 8])
        }
        abi::AUTO_LED => {
            let a = AutoLedArgs::decode(args)?;
            check(Rights::CONTROL)?;
            crosec::set_led(a.led.raw(), None).map_err(failed)?;
            answer(label, 0, [0; 8])
        }
        abi::SET_RGB => {
            let a = SetRgbArgs::decode(args)?;
            check(Rights::CONTROL)?;
            set_rgb(a.first, a.colors.address, a.colors.len)?;
            answer(label, 0, [0; 8])
        }
        abi::SMBIOS => {
            let a = SmbiosArgs::decode(args)?;
            check(Rights::READ)?;
            let tables = Arch::smbios().ok_or(error(Kernel::NotSupported))?;
            let bytes = match a.table {
                SmbiosTable::EntryPoint => tables.entry,
                SmbiosTable::Structures => tables.table,
            };
            let part = &bytes[..bytes.len().min(a.buffer.len as usize)];
            if !radix_ipc::range::user_bytes(a.buffer.address, part.len() as u64)
                || !Arch::copy_to_user(a.buffer.address, part)
            {
                return Err(error(Kernel::BadAddress));
            }
            let results = SmbiosResults { length: bytes.len() as u64 };
            answer(label, SmbiosResults::WORDS, results.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// `set_rgb`: the `len` bytes at `address` of the caller's, three for
/// each LED from `first`.
fn set_rgb(first: u32, address: u64, len: u64) -> Result<()> {
    if !len.is_multiple_of(3) {
        return Err(error(Kernel::InvalidArgument));
    }
    let count = len / 3;
    if u64::from(first) + count > u64::from(abi::RGB_LEDS) {
        return Err(error(Kernel::OutOfRange));
    }
    if !radix_ipc::range::user_bytes(address, len) {
        return Err(error(Kernel::BadAddress));
    }
    let mut colors = [[0u8; 3]; RGB_CHUNK];
    let mut done = 0;
    while done < count as usize {
        let part = (count as usize - done).min(RGB_CHUNK);
        let bytes = colors[..part].as_flattened_mut();
        if !Arch::copy_from_user(bytes, address + 3 * done as u64) {
            return Err(error(Kernel::BadAddress));
        }
        // Below RGB_LEDS, checked above.
        crosec::set_rgb((first as usize + done) as u8, &colors[..part]).map_err(failed)?;
        done += part;
    }
    Ok(())
}

/// The status for a control that failed.
fn failed(why: Failed) -> Status {
    match why {
        Failed::NotSupported | Failed::Ec(Error::Status(radix_crosec::STATUS_INVALID_COMMAND)) => {
            error(Kernel::NotSupported)
        }
        Failed::OutOfRange => error(Kernel::OutOfRange),
        Failed::Ec(Error::Status(radix_crosec::STATUS_INVALID_PARAM) | Error::TooBig) => {
            error(Kernel::InvalidArgument)
        }
        Failed::Ec(Error::Timeout) => error(Kernel::TimedOut),
        Failed::Ec(Error::Status(_) | Error::BadResponse) => File::Io.status(0),
    }
}
