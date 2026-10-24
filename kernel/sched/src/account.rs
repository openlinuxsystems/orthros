// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Accounting what the CPUs do (ADR 0092): where a thread's time goes,
//! and the load averages.
//!
//! The kernel measures a thread's CPU time exactly, from the clock at
//! each switch, but not how much of it was spent in user mode: timing
//! every entry into the kernel would cost each system call more than its
//! work. As on Linux, the timer interrupt samples instead: at each tick it
//! counts one for user mode or the kernel, by where it interrupted the
//! running thread, and [`split_time`] shares the exact time out between
//! them in the samples' proportion. Shared out afresh at each look, a part
//! could go back (a thread that ran 100 ms with one user sample has 100 ms
//! of user time; at 110 ms with a kernel sample too, 55), so a thread, a
//! process and a CPU each keep their last [`Split`], which moves the line
//! between the parts so that neither ever falls, as Linux's
//! `cputime_adjust` does.
//!
//! The load averages are Linux's: every [`LOAD_INTERVAL_NS`] the kernel
//! counts the threads that are ready to run or running, and those waiting
//! for a disk, and [`LoadAverage::sample`] folds the count into three
//! averages that decay over 1, 5 and 15 minutes, in fixed point with
//! [`FIXED_1`] as 1.0, by Linux's own constants and rounding, so the
//! numbers read as an administrator expects.

/// The bits of fraction in a load average.
pub const LOAD_SHIFT: u32 = 11;
/// 1.0 as a load average.
pub const FIXED_1: u64 = 1 << LOAD_SHIFT;
/// How often the load is sampled: every 5 seconds, as on Linux.
pub const LOAD_INTERVAL_NS: u64 = 5_000_000_000;
/// How much of each average stays at each sample: FIXED_1 / e^(5 s / 1
/// minute), and for 5 and 15 minutes.
const DECAY: [u64; 3] = [1884, 2014, 2037];

/// Shares `runtime_ns` out between user mode and the kernel as `user`
/// and `system` samples of the timer do: returns the user and the system
/// time, which add up to `runtime_ns`. With no sample yet, all of it is
/// user time, as Linux counts it.
pub fn split_time(runtime_ns: u64, user: u64, system: u64) -> (u64, u64) {
    let samples = u128::from(user) + u128::from(system);
    if samples == 0 {
        return (runtime_ns, 0);
    }
    let user_ns = (u128::from(runtime_ns) * u128::from(user) / samples) as u64;
    (user_ns, runtime_ns - user_ns)
}

/// User and system time as last given, so that neither part of a later
/// split is less ([`Split::update`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Split {
    user_ns: u64,
    system_ns: u64,
}

impl Split {
    /// No time yet.
    pub const fn new() -> Split {
        Split { user_ns: 0, system_ns: 0 }
    }

    /// Shares `runtime_ns` out as [`split_time`] does, then moves the line
    /// between the parts as little as it must so that neither is less than
    /// last time: the kernel's part first, then the user part. Returns the
    /// user and the system time, which add up to `runtime_ns`; a runtime
    /// no longer than last time's gives last time's parts.
    pub fn update(&mut self, runtime_ns: u64, user: u64, system: u64) -> (u64, u64) {
        // The parts add up to a runtime given before, so this cannot wrap.
        if runtime_ns <= self.user_ns + self.system_ns {
            return (self.user_ns, self.system_ns);
        }
        let (mut user_ns, mut system_ns) = split_time(runtime_ns, user, system);
        if system_ns < self.system_ns {
            system_ns = self.system_ns;
            user_ns = runtime_ns - system_ns;
        }
        // Now the user part has more than last time's, or the kernel's
        // stands as it came, at least last time's: taking from it to give
        // the user part last time's leaves it more than before.
        if user_ns < self.user_ns {
            user_ns = self.user_ns;
            system_ns = runtime_ns - user_ns;
        }
        *self = Split { user_ns, system_ns };
        (user_ns, system_ns)
    }
}

/// The 1, 5 and 15 minute load averages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadAverage {
    /// Each average, with FIXED_1 as 1.0.
    pub averages: [u64; 3],
}

impl LoadAverage {
    /// Averages of nothing.
    pub const fn new() -> LoadAverage {
        LoadAverage { averages: [0; 3] }
    }

    /// Folds in a sample of `active` threads: those ready or running, and
    /// those waiting for a disk.
    pub fn sample(&mut self, active: u64) {
        let active = active.saturating_mul(FIXED_1);
        for (average, decay) in self.averages.iter_mut().zip(DECAY) {
            *average = decayed(*average, decay, active);
        }
    }

    /// Each average in hundredths, rounded to the nearest, as Linux's
    /// `/proc/loadavg` prints them.
    pub fn hundredths(&self) -> [u64; 3] {
        self.averages.map(|average| {
            let rounded = average.saturating_add(FIXED_1 / 200);
            let whole = (rounded >> LOAD_SHIFT).saturating_mul(100);
            whole.saturating_add(((rounded & (FIXED_1 - 1)) * 100) >> LOAD_SHIFT)
        })
    }
}

/// Linux's `calc_load`: `load` decayed by `decay` towards `active`, both
/// in fixed point, rounded up while the load rises so that a steady load
/// of n reaches n.
fn decayed(load: u64, decay: u64, active: u64) -> u64 {
    let mut next =
        u128::from(load) * u128::from(decay) + u128::from(active) * u128::from(FIXED_1 - decay);
    if active >= load {
        next += u128::from(FIXED_1 - 1);
    }
    (next / u128::from(FIXED_1)).min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_is_split_in_the_samples_proportion() {
        assert_eq!(split_time(1_000, 0, 0), (1_000, 0));
        assert_eq!(split_time(1_000, 3, 1), (750, 250));
        assert_eq!(split_time(1_000, 0, 5), (0, 1_000));
        assert_eq!(split_time(u64::MAX, u64::MAX, u64::MAX), (u64::MAX / 2, u64::MAX / 2 + 1));
        for runtime in [0, 1, 7, 999_999_937] {
            for user in 0..20 {
                for system in 0..20 {
                    let (u, s) = split_time(runtime, user, system);
                    assert_eq!(u + s, runtime);
                }
            }
        }
    }

    #[test]
    fn a_split_never_goes_back() {
        // The example above: 100 ms with a user sample, then 110 ms with a
        // kernel sample too.
        let mut split = Split::new();
        assert_eq!(split.update(100, 1, 0), (100, 0));
        assert_eq!(split.update(110, 1, 1), (100, 10));
        // Then the kernel's part is held while the user's catches up.
        let mut split = Split::new();
        assert_eq!(split.update(100, 0, 1), (0, 100));
        assert_eq!(split.update(300, 2, 1), (200, 100));
        // A runtime that does not grow changes nothing.
        assert_eq!(split.update(300, 0, 9), (200, 100));
        assert_eq!(split.update(250, 9, 0), (200, 100));
        // Every runtime and samples that grow, a little at a time.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |below: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % below
        };
        for _ in 0..200 {
            let (mut split, mut runtime, mut user, mut system) = (Split::new(), 0, 0, 0);
            let mut last = (0, 0);
            for _ in 0..200 {
                runtime += next(5_000_000);
                user += next(3);
                system += next(3);
                let (u, s) = split.update(runtime, user, system);
                assert_eq!(u + s, runtime.max(last.0 + last.1));
                assert!(u >= last.0 && s >= last.1, "{last:?} then {:?}", (u, s));
                last = (u, s);
            }
        }
    }

    #[test]
    fn a_steady_load_reaches_its_value() {
        let mut load = LoadAverage::new();
        // An hour and a half of two busy threads.
        for _ in 0..1080 {
            load.sample(2);
        }
        assert_eq!(load.hundredths(), [200, 200, 200]);
        // Then idle: the one-minute average falls fastest.
        for _ in 0..12 {
            load.sample(0);
        }
        let [one, five, fifteen] = load.hundredths();
        assert!(one < five && five < fifteen && fifteen < 200, "{one} {five} {fifteen}");
        // After a minute the one-minute average keeps 1/e of the load.
        assert!((70..=76).contains(&one), "{one}");
    }

    #[test]
    fn loads_print_as_linux_prints_them() {
        let load = LoadAverage { averages: [FIXED_1, FIXED_1 / 2, 3 * FIXED_1 + FIXED_1 / 4] };
        assert_eq!(load.hundredths(), [100, 50, 325]);
        assert_eq!(LoadAverage::new().hundredths(), [0, 0, 0]);
    }

    #[test]
    fn huge_loads_do_not_overflow() {
        let mut load = LoadAverage::new();
        for _ in 0..100 {
            load.sample(u64::MAX);
        }
        assert!(load.averages.iter().all(|&a| a > 0));
        let _ = load.hundredths();
    }
}
