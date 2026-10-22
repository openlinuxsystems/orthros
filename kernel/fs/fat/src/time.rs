// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! FAT's timestamps: a date and a time packed into 16 bits each, in
//! two-second steps from 1980 to 2107, with no time zone. Radix keeps
//! the clock in UTC and so writes and reads them as UTC.
//!
//! The calendar arithmetic is Howard Hinnant's `days_from_civil` and
//! `civil_from_days` (public domain, <https://howardhinnant.github.io/date_algorithms.html>),
//! as `radix-hal`'s clock uses.

const NS: u64 = 1_000_000_000;

/// Days from 1970-01-01 to the date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A FAT date and time for `ns` nanoseconds since 1970, clamped to the
/// years FAT can hold.
pub fn to_fat(ns: u64) -> (u16, u16) {
    let seconds = ns / NS;
    let days = (seconds / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    if year < 1980 {
        return (0x21, 0);
    }
    if year > 2107 {
        return (0xff9f, 0xbf7d);
    }
    let in_day = seconds % 86_400;
    let (h, m, s) = (in_day / 3600, (in_day / 60) % 60, in_day % 60);
    let date = (((year - 1980) as u16) << 9) | ((month as u16) << 5) | day as u16;
    let time = ((h as u16) << 11) | ((m as u16) << 5) | (s / 2) as u16;
    (date, time)
}

/// Nanoseconds since 1970 for a FAT date and time; 0 for a date of 0
/// (never set).
pub fn from_fat(date: u16, time: u16) -> u64 {
    if date == 0 {
        return 0;
    }
    let year = 1980 + i64::from(date >> 9);
    let month = u32::from((date >> 5) & 0xf).clamp(1, 12);
    let day = u32::from(date & 0x1f).clamp(1, 31);
    let days = days_from_civil(year, month, day);
    let (h, m, s) =
        (u64::from(time >> 11), u64::from((time >> 5) & 0x3f), u64::from(time & 0x1f) * 2);
    let seconds = days as u64 * 86_400 + h * 3600 + m * 60 + s;
    seconds * NS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_in_two_second_steps() {
        // 2026-10-22 00:42:22 UTC.
        let ns = 1_792_629_742 * NS;
        let (date, time) = to_fat(ns);
        assert_eq!(date >> 9, 46);
        assert_eq!((date >> 5) & 0xf, 10);
        assert_eq!(date & 0x1f, 22);
        assert_eq!(from_fat(date, time), ns);
        assert_eq!(to_fat(0), (0x21, 0));
        assert_eq!(from_fat(0, 0), 0);
        assert_eq!(from_fat(0x21, 0), 315_532_800 * NS);
    }
}
