// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Notifications: a word of bits that is signalled and waited for, after
//! seL4's notification objects.
//!
//! `notify` sets the bits of the capability's badge (`radix_ipc::notify`)
//! and wakes the threads waiting; `wait` takes the whole word, leaving 0,
//! and sleeps while it is 0. A ring can wait for a notification too (the
//! ring's `wait` operation): it then gets the next signal as a completion
//! instead, and a signal goes to a waiting ring before it reaches the
//! word. Signals that arrive while nobody waits add up in the word, so
//! none is lost, but several of the same bit are one.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::errors::Kernel;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::SpinLock;

use super::ring::RingRef;
use super::{Fifo, Result, error};
use crate::sched::{self, wait};

/// A notification, shared.
pub type NotificationRef = Arc<Notification, Kmalloc>;

/// A notification.
pub struct Notification {
    word: AtomicU64,
    closed: AtomicBool,
    /// Rings waiting for a signal, with the user data of their entries.
    rings: SpinLock<Fifo<(RingRef, u64)>>,
}

impl Notification {
    /// A new notification, with its word 0.
    pub fn new() -> Option<NotificationRef> {
        let notification = Notification {
            word: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            rings: SpinLock::new(Fifo::new()),
        };
        Arc::try_new_in(notification, HEAP.with(Flags::MAY_SLEEP)).ok()
    }

    /// What threads waiting for it wait on.
    fn key(&self) -> usize {
        self as *const Notification as usize
    }

    /// Signals it through a capability with `badge`.
    pub fn notify(&self, badge: u64) {
        let bits = radix_ipc::notify::bits(badge);
        let ring = {
            let mut rings = self.rings.lock();
            match rings.pop_front() {
                Some(ring) => Some(ring),
                None => {
                    // Sequentially consistent, for `poll` (changed_by_rmw).
                    self.word.fetch_or(bits, Ordering::SeqCst);
                    None
                }
            }
        };
        match ring {
            Some((ring, user_data)) => ring.finish(user_data, Ok([bits, 0])),
            None => {
                wait::wake_all(self.key());
                super::posix::poll::changed_by_rmw();
            }
        }
    }

    /// Whether bits are set, and whether its last capability has gone,
    /// for `poll` (Phase 8): a look that takes nothing.
    pub fn peek(&self) -> (bool, bool) {
        (self.word.load(Ordering::SeqCst) != 0, self.closed.load(Ordering::Acquire))
    }

    /// Takes its word, waiting while it is 0: not at all with `poll`, and
    /// until `deadline_ns` (on the kernel's clock) if there is one.
    pub fn wait(&self, poll: bool, deadline_ns: Option<u64>) -> Result<u64> {
        loop {
            let bits = self.word.swap(0, Ordering::Acquire);
            if bits != 0 {
                return Ok(bits);
            }
            if self.closed.load(Ordering::Acquire) {
                return Err(error(Kernel::Gone));
            }
            if poll {
                return Err(error(Kernel::WouldBlock));
            }
            if sched::interrupted() {
                return Err(error(Kernel::Interrupted));
            }
            let ready = || {
                self.word.load(Ordering::Relaxed) != 0
                    || self.closed.load(Ordering::Relaxed)
                    || sched::interrupted()
            };
            match deadline_ns {
                None => wait::wait_until(self.key(), "notification", ready),
                Some(deadline) => {
                    if !wait::wait_until_or_timeout(self.key(), "notification", deadline, ready) {
                        return Err(error(Kernel::TimedOut));
                    }
                }
            }
        }
    }

    /// A ring waits for it: completes the ring's entry with `user_data` at
    /// once if its word is not 0, and at the next signal otherwise. The
    /// ring has room reserved for the completion.
    pub fn wait_ring(&self, ring: &RingRef, user_data: u64) {
        let now = {
            let mut rings = self.rings.lock();
            if self.closed.load(Ordering::Acquire) {
                Err(error(Kernel::Gone))
            } else {
                let bits = self.word.swap(0, Ordering::Acquire);
                if bits != 0 {
                    Ok([bits, 0])
                } else if rings.push_back((ring.clone(), user_data)).is_err() {
                    Err(error(Kernel::NoMemory))
                } else {
                    return;
                }
            }
        };
        ring.finish(user_data, now);
    }

    /// Its last capability is gone: threads and rings waiting for it get
    /// `gone`.
    pub fn close(&self) {
        let rings = {
            let mut rings = self.rings.lock();
            self.closed.store(true, Ordering::Release);
            rings.take()
        };
        for (ring, user_data) in rings {
            ring.finish(user_data, Err(error(Kernel::Gone)));
        }
        wait::wake_all(self.key());
        super::posix::poll::changed();
    }
}
