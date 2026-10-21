//! Health tests for the entropy sources.
//!
//! The kernel mixes every source into the seed whether or not it passes,
//! since adding input to a hash cannot make the seed more predictable. The
//! tests decide what the boot log reports: which sources the seed actually
//! rests on, and a warning when none of them looks healthy.

/// The test for a hardware generator's 64-bit words (RDSEED, RDRAND): a
/// word that is all zeros, all ones, or equal to the word before it fails.
/// A working generator does any of that with probability about 2^-62, and
/// broken ones are known to do exactly that (some AMD CPUs returned all
/// ones after resuming from suspend).
#[derive(Clone, Copy, Debug, Default)]
pub struct WordTest {
    previous: Option<u64>,
    good: u32,
    bad: u32,
    missed: u32,
}

impl WordTest {
    /// A test that has seen nothing.
    pub const fn new() -> WordTest {
        WordTest { previous: None, good: 0, bad: 0, missed: 0 }
    }

    /// Checks one word, and returns whether it passed.
    pub fn check(&mut self, word: u64) -> bool {
        let ok = word != 0 && word != u64::MAX && self.previous != Some(word);
        self.previous = Some(word);
        if ok {
            self.good += 1;
        } else {
            self.bad += 1;
        }
        ok
    }

    /// Counts a request the generator did not answer, even after retries.
    pub fn miss(&mut self) {
        self.missed += 1;
    }

    /// Whether the source passed: `wanted` good words and no bad or missing
    /// ones.
    pub fn passed(&self, wanted: u32) -> bool {
        self.good >= wanted && self.bad == 0 && self.missed == 0
    }
}

/// Stuck samples in a row that fail the jitter test.
pub const MAX_STUCK_RUN: u32 = 32;

/// The test for timing jitter. Each sample is the time a small piece of
/// work took. As in the `jitterentropy` library, a sample is *stuck* when
/// it, its difference from the previous one or the difference of those
/// differences is zero: then the timing changed in no unpredictable way.
/// The source passes when at most half of its samples are stuck and fewer
/// than [`MAX_STUCK_RUN`] are stuck in a row.
#[derive(Clone, Copy, Debug, Default)]
pub struct JitterTest {
    last: u64,
    last_change: u64,
    samples: u32,
    stuck: u32,
    run: u32,
    longest_run: u32,
}

impl JitterTest {
    /// A test that has seen nothing.
    pub const fn new() -> JitterTest {
        JitterTest { last: 0, last_change: 0, samples: 0, stuck: 0, run: 0, longest_run: 0 }
    }

    /// Adds one sample: the time-stamp counter's advance over the work.
    pub fn sample(&mut self, delta: u64) {
        let change = delta.wrapping_sub(self.last);
        let change_of_change = change.wrapping_sub(self.last_change);
        self.last = delta;
        self.last_change = change;
        self.samples += 1;
        if delta == 0 || change == 0 || change_of_change == 0 {
            self.stuck += 1;
            self.run += 1;
            self.longest_run = self.longest_run.max(self.run);
        } else {
            self.run = 0;
        }
    }

    /// Samples taken.
    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// Samples that were stuck.
    pub fn stuck(&self) -> u32 {
        self.stuck
    }

    /// Whether the source passed.
    pub fn passed(&self) -> bool {
        self.samples > 0 && self.stuck <= self.samples / 2 && self.longest_run < MAX_STUCK_RUN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_fail_when_stuck_or_saturated() {
        let mut test = WordTest::new();
        assert!(test.check(0x1234_5678_9abc_def0));
        assert!(test.check(0x0fed_cba9_8765_4321));
        assert!(test.passed(2));
        assert!(!test.passed(3));
        assert!(!test.check(0x0fed_cba9_8765_4321));
        assert!(!test.passed(2));

        for bad in [0, u64::MAX] {
            let mut test = WordTest::new();
            assert!(!test.check(bad));
            assert!(test.check(7));
            assert!(!test.passed(1));
        }
        let mut test = WordTest::new();
        assert!(test.check(7));
        test.miss();
        assert!(!test.passed(1));
    }

    #[test]
    fn a_steady_clock_fails_and_a_noisy_one_passes() {
        let mut steady = JitterTest::new();
        for _ in 0..1000 {
            steady.sample(100);
        }
        assert!(!steady.passed());
        assert!(steady.stuck() >= 998);

        // A clock that ticks in large steps: most samples see no change.
        let mut coarse = JitterTest::new();
        for i in 0..1000 {
            coarse.sample(if i % 8 == 0 { 1000 } else { 0 });
        }
        assert!(!coarse.passed());

        // Deltas from a simple generator: almost never stuck.
        let mut noisy = JitterTest::new();
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..1000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            noisy.sample(100 + x % 64);
        }
        assert!(noisy.passed(), "{noisy:?}");
        assert_eq!(noisy.samples(), 1000);
    }

    #[test]
    fn a_long_stuck_run_fails() {
        let mut test = JitterTest::new();
        let mut x: u64 = 1;
        for i in 0..1000 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            test.sample(if (500..500 + MAX_STUCK_RUN as usize + 2).contains(&i) {
                0
            } else {
                x >> 58 | 1
            });
        }
        assert!(test.stuck() < 100);
        assert!(!test.passed());
    }

    #[test]
    fn nothing_sampled_fails() {
        assert!(!JitterTest::new().passed());
        assert!(!WordTest::new().passed(1));
    }
}
