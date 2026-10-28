// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The rings of events behind `/devices/kbd` and `/devices/mouse`
//! (radix-input(2)).
//!
//! Events are numbered from boot, and the ring keeps the last `N`. A
//! description of the device keeps in its offset one more than the number
//! of the next event it reads, starting with the first after it was
//! opened, so that nobody reads what came before; a reader that falls
//! more than `N` behind is told once that events were lost, and goes on
//! with the oldest kept.

use radix_abi::Status;
use radix_abi::errors::Kernel;
use radix_sync::SpinLock;
use radix_vfs::Out;

use super::FileObject;
use crate::sched::{self, wait};
use crate::user::posix::signal::{Restart, interrupted};

/// The bytes events are copied out through, a batch at a time.
const BATCH_BYTES: usize = 512;

struct Ring<T, const N: usize> {
    events: [T; N],
    next: u64,
}

/// An event as a reader reads it.
pub trait Record: Copy {
    /// Its bytes.
    const SIZE: usize;

    /// Writes its `SIZE` bytes into `out`.
    fn encode(&self, out: &mut [u8]);
}

/// A ring of `N` events of type `T`.
pub struct Events<T: Record, const N: usize> {
    ring: SpinLock<Ring<T, N>>,
}

impl<T: Record, const N: usize> Events<T, N> {
    /// An empty ring, its places holding `empty`.
    pub const fn new(empty: T) -> Self {
        Events { ring: SpinLock::new(Ring { events: [empty; N], next: 0 }) }
    }

    /// Adds `event`; it may be called from an interrupt handler.
    pub fn push(&self, event: T) {
        let mut ring = self.ring.lock_irq();
        let at = (ring.next % N as u64) as usize;
        ring.events[at] = event;
        ring.next += 1;
    }

    /// The number of the next event.
    pub fn next(&self) -> u64 {
        self.ring.lock_irq().next
    }

    /// The offset a new description starts with: the next event's.
    pub fn opened(&self) -> u64 {
        self.next() + 1
    }

    /// Reads as many of the reader's next events as `out` holds whole,
    /// waiting for one unless `non_blocking`, on `key`, which whoever
    /// pushes wakes. Fails with
    /// invalid_argument if `out` cannot hold one, and with gone once if
    /// events were lost before the reader read them, moving on to the
    /// oldest kept.
    pub fn read(
        &self,
        file: &FileObject,
        out: &mut dyn Out,
        non_blocking: bool,
        key: usize,
        reason: &'static str,
    ) -> Result<usize, Status> {
        let size = T::SIZE;
        let room = out.len() / size;
        if room == 0 {
            return Err(Kernel::InvalidArgument.status(0));
        }
        let mut offset = file.offset.lock();
        loop {
            let mut wanted = offset.saturating_sub(1);
            // A batch at a time, copied out with the ring let go of: the
            // copy may wait for a page.
            let mut batch = [0u8; BATCH_BYTES];
            let (count, lost) = {
                let ring = self.ring.lock_irq();
                let oldest = ring.next.saturating_sub(N as u64);
                let lost = wanted < oldest;
                wanted = wanted.max(oldest);
                let count = (ring.next - wanted).min(room.min(BATCH_BYTES / size) as u64) as usize;
                for (i, bytes) in batch.chunks_exact_mut(size).take(count).enumerate() {
                    ring.events[((wanted + i as u64) % N as u64) as usize].encode(bytes);
                }
                (count, lost)
            };
            if lost {
                *offset = wanted + 1;
                return Err(Kernel::Gone.status(0));
            }
            if count > 0 {
                out.put(0, &batch[..count * size]).map_err(super::status)?;
                *offset = wanted + count as u64 + 1;
                return Ok(count * size);
            }
            if non_blocking {
                return Err(Kernel::WouldBlock.status(0));
            }
            wait::wait_until(key, reason, || self.next() > wanted || sched::interrupted());
            if sched::interrupted() && self.next() <= wanted {
                return Err(interrupted(Restart::Always));
            }
        }
    }

    /// Whether an event waits for the reader of `file`.
    pub fn readable(&self, file: &FileObject) -> bool {
        // A read in progress holds the offset: it has nothing to read yet.
        file.offset.try_lock().is_some_and(|offset| self.next() > offset.saturating_sub(1))
    }
}
