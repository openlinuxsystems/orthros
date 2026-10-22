// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Nice levels, weights and real-time priorities.
//!
//! A fair thread's share of a CPU is its weight over the sum of the
//! weights it competes with. Nice 0 weighs [`NICE_0_WEIGHT`], and each
//! nice level is worth 1.25 times the next one, so one level more or less
//! moves about 10 % of a CPU between two otherwise equal threads. The
//! table is computed here from that ratio (1024 / 1.25^nice, rounded), not
//! copied from another kernel.

/// The most favoured nice level.
pub const NICE_MIN: i8 = -20;
/// The least favoured nice level.
pub const NICE_MAX: i8 = 19;
/// The weight of a nice-0 thread, and the unit of virtual time: a nice-0
/// thread's virtual runtime advances at the rate of real time.
pub const NICE_0_WEIGHT: u64 = 1024;

/// The lowest real-time priority.
pub const RT_PRIO_MIN: u8 = 1;
/// The highest real-time priority.
pub const RT_PRIO_MAX: u8 = 99;

/// A resource domain's default CPU weight (as cgroup v2's `cpu.weight`).
pub const DOMAIN_WEIGHT_DEFAULT: u32 = 100;
/// The largest domain CPU weight.
pub const DOMAIN_WEIGHT_MAX: u32 = 10_000;

const fn pow(base: u128, exp: u32) -> u128 {
    let mut value = 1;
    let mut i = 0;
    while i < exp {
        value *= base;
        i += 1;
    }
    value
}

const fn table() -> [u32; 40] {
    let mut table = [0u32; 40];
    let mut i = 0;
    while i < 40 {
        let nice = i as i32 + NICE_MIN as i32;
        // 1.25 is 5/4: weight = 1024 * (5/4)^-nice.
        let (num, den) = if nice <= 0 {
            (NICE_0_WEIGHT as u128 * pow(5, (-nice) as u32), pow(4, (-nice) as u32))
        } else {
            (NICE_0_WEIGHT as u128 * pow(4, nice as u32), pow(5, nice as u32))
        };
        table[i] = ((num + den / 2) / den) as u32;
        i += 1;
    }
    table
}

/// The weight of each nice level, from [`NICE_MIN`] to [`NICE_MAX`].
pub const WEIGHTS: [u32; 40] = table();

/// Whether `nice` is a nice level.
pub const fn nice_valid(nice: i8) -> bool {
    nice >= NICE_MIN && nice <= NICE_MAX
}

/// The weight of nice level `nice` (clamped to the valid range).
pub const fn weight(nice: i8) -> u64 {
    let nice = if nice < NICE_MIN {
        NICE_MIN
    } else if nice > NICE_MAX {
        NICE_MAX
    } else {
        nice
    };
    WEIGHTS[(nice as i32 - NICE_MIN as i32) as usize] as u64
}

/// Whether `prio` is a real-time priority.
pub const fn rt_prio_valid(prio: u8) -> bool {
    prio >= RT_PRIO_MIN && prio <= RT_PRIO_MAX
}

/// The queue weight of a resource domain with CPU weight `weight` (1 to
/// 10,000): the default weight, 100, weighs as much as a nice-0 thread.
pub const fn domain_weight(weight: u32) -> u64 {
    let weight = if weight == 0 {
        1
    } else if weight > DOMAIN_WEIGHT_MAX {
        DOMAIN_WEIGHT_MAX
    } else {
        weight
    };
    let scaled = weight as u64 * NICE_0_WEIGHT / DOMAIN_WEIGHT_DEFAULT as u64;
    if scaled < 2 { 2 } else { scaled }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_zero_is_the_unit_and_each_level_is_a_quarter() {
        assert_eq!(weight(0), NICE_0_WEIGHT);
        assert_eq!(weight(1), 819);
        assert_eq!(weight(-1), 1280);
        assert_eq!(weight(NICE_MAX), 15);
        assert_eq!(weight(NICE_MIN), 88818);
        for pair in WEIGHTS.windows(2) {
            let (more, less) = (u64::from(pair[0]), u64::from(pair[1]));
            assert!(more > less);
            assert!((more * 4).abs_diff(less * 5) <= 5, "{more} {less}");
        }
        assert_eq!(weight(-100), weight(NICE_MIN));
        assert_eq!(weight(100), weight(NICE_MAX));
    }

    #[test]
    fn domain_weights_scale_from_the_default() {
        assert_eq!(domain_weight(100), 1024);
        assert_eq!(domain_weight(300), 3072);
        assert_eq!(domain_weight(1), 10);
        assert_eq!(domain_weight(0), 10);
        assert_eq!(domain_weight(20_000), 102_400);
    }
}
