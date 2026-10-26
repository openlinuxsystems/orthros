// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! How long an idle CPU polls before it halts, under a hypervisor.
//!
//! A virtual CPU that halts exits to the hypervisor, and waking it takes
//! an interprocessor interrupt, which is another exit for the CPU that
//! sends it. Together they cost microseconds, far more than most waits for
//! a reply. So an idle virtual CPU first polls for a while, and a waker
//! that sees it polling writes a word instead of sending an interrupt.
//!
//! How long it polls adapts to how long its recent idle periods were, the
//! way Linux's cpuidle-haltpoll governor does it, with the same defaults
//! (its documentation is `Documentation/virt/guest-halt-polling.rst`):
//!
//! - the limit starts at 0, so a CPU that is idle for long never polls;
//! - an idle period that another CPU's wake-up ended after the limit but
//!   within [`MAX_POLL_NS`] means polling longer would have caught it, so
//!   the limit doubles (to at least [`GROW_START_NS`], at most
//!   [`MAX_POLL_NS`]);
//! - an idle period longer than [`MAX_POLL_NS`] means polling only wasted
//!   time, so the limit halves.
//!
//! Unlike Linux's governor, it learns only from wake-ups that other CPUs
//! send. One that comes from this CPU's own interrupt, a disk's or the
//! timer's, needs that interrupt whether the CPU polls or halts, and the
//! hypervisor delivers it sooner to a halted CPU, which it watches, than
//! to a running one, which it must first stop. So an idle period such an
//! interrupt ended halves the limit too, and a CPU that waits only for
//! its devices halts at once, as Linux's do (ADR 0116).
//!
//! A CPU with MWAIT waits on the word in hardware and needs none of this.

/// The longest a CPU polls, and the idle period beyond which polling does
/// not pay.
pub const MAX_POLL_NS: u64 = 200_000;

/// What the limit grows to first.
pub const GROW_START_NS: u64 = 50_000;

/// One CPU's polling limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HaltPoll {
    limit_ns: u64,
}

impl HaltPoll {
    /// A limit of 0: no polling until an idle period shows it would help.
    pub const fn new() -> HaltPoll {
        HaltPoll { limit_ns: 0 }
    }

    /// How long to poll before halting.
    pub const fn limit_ns(&self) -> u64 {
        self.limit_ns
    }

    /// Learns from an idle period that lasted `idle_ns`, polling and halted
    /// together, and that a wake-up from another CPU ended if `called`.
    pub fn record(&mut self, idle_ns: u64, called: bool) {
        if idle_ns > MAX_POLL_NS || !called {
            self.limit_ns /= 2;
        } else if idle_ns > self.limit_ns {
            self.limit_ns = (self.limit_ns * 2).clamp(GROW_START_NS, MAX_POLL_NS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_waits_grow_the_limit_and_long_ones_shrink_it() {
        let mut poll = HaltPoll::new();
        assert_eq!(poll.limit_ns(), 0);
        // Woken soon after halting: poll next time.
        poll.record(10_000, true);
        assert_eq!(poll.limit_ns(), GROW_START_NS);
        // Caught while polling: nothing to learn.
        poll.record(GROW_START_NS - 1, true);
        assert_eq!(poll.limit_ns(), GROW_START_NS);
        // Woken after the limit, within the maximum: double it.
        poll.record(GROW_START_NS + 1, true);
        assert_eq!(poll.limit_ns(), 2 * GROW_START_NS);
        poll.record(MAX_POLL_NS, true);
        assert_eq!(poll.limit_ns(), MAX_POLL_NS);
        poll.record(MAX_POLL_NS, true);
        assert_eq!(poll.limit_ns(), MAX_POLL_NS);
        // Idle for long: polling was wasted, so halve it.
        poll.record(MAX_POLL_NS + 1, true);
        assert_eq!(poll.limit_ns(), MAX_POLL_NS / 2);
        poll.record(u64::MAX, true);
        assert_eq!(poll.limit_ns(), MAX_POLL_NS / 4);
    }

    #[test]
    fn the_limit_stays_within_bounds() {
        let mut poll = HaltPoll::new();
        let mut seed = 0x1234_5678_9abc_def0u64;
        for _ in 0..100_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let before = poll.limit_ns();
            let idle = seed % (3 * MAX_POLL_NS);
            let called = seed & (1 << 40) != 0;
            poll.record(idle, called);
            let after = poll.limit_ns();
            assert!(after <= MAX_POLL_NS);
            if idle > MAX_POLL_NS || !called {
                assert_eq!(after, before / 2);
            } else if idle > before {
                assert!(after > before || after == MAX_POLL_NS);
            } else {
                assert_eq!(after, before);
            }
        }
    }

    #[test]
    fn a_cpu_idle_for_long_stops_polling() {
        let mut poll = HaltPoll::new();
        for _ in 0..3 {
            poll.record(MAX_POLL_NS, true);
        }
        assert_eq!(poll.limit_ns(), MAX_POLL_NS);
        for _ in 0..20 {
            poll.record(10_000_000, true);
        }
        assert_eq!(poll.limit_ns(), 0);
    }

    #[test]
    fn a_cpu_its_own_interrupts_wake_stops_polling() {
        let mut poll = HaltPoll::new();
        for _ in 0..3 {
            poll.record(MAX_POLL_NS, true);
        }
        assert_eq!(poll.limit_ns(), MAX_POLL_NS);
        // A disk's interrupt ends each wait, soon after it began: polling
        // would not have spared it, so the CPU learns to halt.
        poll.record(30_000, false);
        assert_eq!(poll.limit_ns(), MAX_POLL_NS / 2);
        for _ in 0..20 {
            poll.record(30_000, false);
        }
        assert_eq!(poll.limit_ns(), 0);
        // Halting, it stays at 0; another CPU's wake-up starts it again.
        poll.record(10_000, false);
        assert_eq!(poll.limit_ns(), 0);
        poll.record(10_000, true);
        assert_eq!(poll.limit_ns(), GROW_START_NS);
    }
}
