// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods of the clock capability (Phase 12, radix-clock(2), ADR
//! 0194): reading, setting and slewing the time of day, which the
//! architecture keeps on the kernel's monotonic clock
//! (`radix_hal::clock::WallClock`), and writing it into the machine's
//! real-time clock. Each change is logged, with who made it, as Linux
//! logs a clock step.

use radix_abi::clock::{
    self as abi, SLEW_MAX_NS, SetArgs, SlewArgs, SlewResults, StateArgs, StateResults,
    WriteRtcArgs,
};
use radix_abi::errors::Kernel;
use radix_cap::{Capability, ObjectKind, Rights};
use radix_hal::Timer;
use radix_hal::clock::civil_from_days;
use radix_klog::info;

use super::methods::{Answer, answer};
use super::{Result, UserRef, cap_error, error};
use crate::Arch;

/// The earliest time `set` takes: 1980-01-01, the earliest a FAT
/// timestamp holds, so that a clock set to 0 by mistake is refused.
const EARLIEST_NS: u64 = 315_532_800 * 1_000_000_000;
/// The latest: 2500-01-01, well within what nanoseconds since 1970 in
/// 64 bits hold (to 2554).
const LATEST_NS: u64 = 16_725_225_600 * 1_000_000_000;

/// Calls the method `label` on `cap`, a clock capability, for `user`:
/// kept out of line, so that [`super::methods::call`]'s frame does not
/// carry its own.
#[inline(never)]
pub fn method(user: &UserRef, cap: &Capability, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let check = |rights| cap.check(ObjectKind::Clock, rights).map_err(cap_error);
    match label {
        abi::STATE => {
            StateArgs::decode(args)?;
            check(Rights::READ)?;
            let results = StateResults {
                now_ns: <Arch as Timer>::wall_clock_ns().unwrap_or(0),
                slew_ns: <Arch as Timer>::wall_clock_slew_ns(),
                rtc_seconds: <Arch as Timer>::rtc_seconds().unwrap_or(0),
            };
            answer(label, StateResults::WORDS, results.encode())
        }
        abi::SET => {
            let a = SetArgs::decode(args)?;
            check(Rights::CONTROL)?;
            if !(EARLIEST_NS..=LATEST_NS).contains(&a.ns) {
                return Err(error(Kernel::InvalidArgument));
            }
            let before = <Arch as Timer>::wall_clock_ns();
            <Arch as Timer>::set_wall_clock_ns(a.ns);
            let written = <Arch as Timer>::write_rtc(rounded_seconds(a.ns));
            let name = user.process.name();
            let (y, mo, d) = civil_from_days((a.ns / 86_400_000_000_000) as i64);
            let s = a.ns / 1_000_000_000 % 86_400;
            let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
            let rtc = if written { ", and the real-time clock" } else { "" };
            match before {
                Some(before) => info!(
                    "Time: {} set the clock to {y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC ({:+} ms){rtc}",
                    name.as_str(),
                    (i128::from(a.ns) - i128::from(before)) / 1_000_000
                ),
                None => info!(
                    "Time: {} set the clock to {y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC{rtc}",
                    name.as_str()
                ),
            }
            answer(label, 0, [0; 8])
        }
        abi::SLEW => {
            let a = SlewArgs::decode(args)?;
            check(Rights::CONTROL)?;
            if a.delta_ns.unsigned_abs() > SLEW_MAX_NS {
                return Err(error(Kernel::InvalidArgument));
            }
            let left = <Arch as Timer>::slew_wall_clock(a.delta_ns)
                .ok_or(error(Kernel::NotSupported))?;
            answer(label, SlewResults::WORDS, SlewResults { left_ns: left }.encode())
        }
        abi::WRITE_RTC => {
            WriteRtcArgs::decode(args)?;
            check(Rights::CONTROL)?;
            let now = <Arch as Timer>::wall_clock_ns().ok_or(error(Kernel::NotSupported))?;
            if !<Arch as Timer>::write_rtc(rounded_seconds(now)) {
                return Err(error(Kernel::NotSupported));
            }
            answer(label, 0, [0; 8])
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// `ns` in whole seconds, to the nearest: what the real-time clock,
/// which counts seconds, is written with.
fn rounded_seconds(ns: u64) -> u64 {
    ns.saturating_add(500_000_000) / 1_000_000_000
}
