// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Calendar arithmetic for the wall clock, and the fixed-point scale that
//! turns a counter's ticks into nanoseconds.

/// Days from 1970-01-01 to `year`-`month`-`day` in the proleptic Gregorian
/// calendar (negative before 1970), by Howard Hinnant's `days_from_civil`
/// algorithm (public domain).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of the day `days` after 1970-01-01: (year, month, day).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Days in `month` (1 to 12) of `year`.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year.rem_euclid(4) == 0
            && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0) =>
        {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Seconds since 1970-01-01 00:00:00 UTC of a calendar date and time, if
/// it is valid and not before 1970.
pub fn unix_seconds(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<u64> {
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + i64::from(hour * 3600 + minute * 60 + second);
    u64::try_from(seconds).ok()
}

/// The scale [`ticks_to_ns`] takes for a counter running at `hz`:
/// nanoseconds per tick times 2^32, rounded down.
pub fn ns_per_tick(hz: u64) -> u64 {
    ((1_000_000_000u128 << 32) / u128::from(hz.max(1))) as u64
}

/// Nanoseconds in `ticks` of a counter whose scale is `ns_per_tick`.
pub fn ticks_to_ns(ticks: u64, ns_per_tick: u64) -> u64 {
    ((u128::from(ticks) * u128::from(ns_per_tick)) >> 32) as u64
}

/// The first tick count at which [`ticks_to_ns`] reaches `ns`, so that a
/// timer armed for it never fires before the clock reads `ns`. Scaling
/// `ns` by the counter's rate instead lands early, more so the larger `ns`
/// is, because the scale is rounded down.
pub fn first_tick_at(ns: u64, ns_per_tick: u64) -> u64 {
    let ticks = (u128::from(ns) << 32).div_ceil(u128::from(ns_per_tick.max(1)));
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(unix_seconds(2026, 10, 22, 0, 17, 56), Some(1_792_628_276));
        assert_eq!(unix_seconds(1969, 12, 31, 23, 59, 59), None);
        assert_eq!(unix_seconds(2026, 13, 1, 0, 0, 0), None);
    }

    #[test]
    fn days_past_the_end_of_the_month_are_refused() {
        assert_eq!(unix_seconds(2024, 2, 29, 0, 0, 0), Some(1_709_164_800));
        assert_eq!(unix_seconds(2000, 2, 29, 0, 0, 0), Some(951_782_400));
        assert_eq!(unix_seconds(2025, 2, 29, 0, 0, 0), None);
        assert_eq!(unix_seconds(2100, 2, 29, 0, 0, 0), None);
        assert_eq!(unix_seconds(2026, 4, 31, 0, 0, 0), None);
        assert_eq!(unix_seconds(2026, 12, 31, 23, 59, 59), Some(1_798_761_599));
        assert_eq!(unix_seconds(2026, 1, 0, 0, 0, 0), None);
        for month in 1..=12 {
            let last = days_in_month(2026, month);
            let next = if month == 12 { (2027, 1) } else { (2026, month + 1) };
            assert_eq!(days_from_civil(2026, month, last) + 1, days_from_civil(next.0, next.1, 1));
        }
    }

    #[test]
    fn a_timer_at_the_first_tick_never_fires_early() {
        for hz in [
            1_000_000,
            14_318_180,
            1_000_000_000,
            2_419_200_000,
            2_995_200_000,
            3_000_000_000,
            4_294_967_311,
        ] {
            let scale = ns_per_tick(hz);
            for ns in [0, 1, 999, 1_000_000_007, 3 * 86_400 * 1_000_000_000 + 12_345, u64::MAX >> 8]
            {
                let tick = first_tick_at(ns, scale);
                assert!(ticks_to_ns(tick, scale) >= ns, "{hz} Hz, {ns} ns");
                assert!(tick == 0 || ticks_to_ns(tick - 1, scale) < ns, "{hz} Hz, {ns} ns");
            }
        }
        // Scaling by the rate instead fires 60 us early a day after boot.
        let (hz, ns) = (2_995_200_001u64, 86_400 * 1_000_000_000u64);
        let scaled = (u128::from(ns) * u128::from(hz) / 1_000_000_000) as u64;
        assert!(ticks_to_ns(scaled, ns_per_tick(hz)) < ns);
    }

    #[test]
    fn round_trip() {
        for days in (-800_000..800_000).step_by(37) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
