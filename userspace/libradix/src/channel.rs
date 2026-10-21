// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shared-memory channels: two programs map the same frame and pass
//! fixed-size messages through it, each way in a queue of its own, with a
//! notification each to sleep on when there is nothing to do.
//!
//! The kernel takes part only to wake a program: the messages themselves
//! never pass through it. A queue has one writer and one reader; each
//! moves only its own index (release ordering after writing a message or
//! reading one, acquire before looking at the other's), so neither ever
//! sees half a message. After moving its index each signals the other's
//! notification; a program that finds nothing to do waits on its own, and
//! because a notification keeps a signal that comes before the wait, no
//! wake-up is lost (radix-notification(2)).

use core::sync::atomic::{AtomicU32, Ordering};

use radix_abi::Status;
use radix_abi::notification::WaitFlags;
use radix_abi::syscalls;

use crate::SYS;

/// A message: eight words.
pub type Message = [u64; 8];

/// Messages a queue holds.
pub const QUEUE_LEN: u32 = 16;

/// The bytes of the frame a channel takes: one page.
pub const CHANNEL_BYTES: u64 = 4096;

/// Where the two queues start in the frame.
const QUEUES: [u64; 2] = [0, 2048];
/// Where a queue's parts start: the head, the tail (each on a cache line
/// of its own), and the messages.
const HEAD: u64 = 0;
const TAIL: u64 = 64;
const MESSAGES: u64 = 128;

const _: () = assert!(MESSAGES + QUEUE_LEN as u64 * 64 <= QUEUES[1]);

/// One end of a channel.
pub struct Channel {
    base: u64,
    /// The queue it sends on, and the one it receives from.
    send: u64,
    recv: u64,
    /// The notification it waits on, and the other end's.
    mine: u32,
    theirs: u32,
}

impl Channel {
    /// The end of the channel in the frame mapped at `base`: end 0 or 1,
    /// waiting on notification `mine` and waking the other end through
    /// `theirs`.
    ///
    /// # Safety
    ///
    /// The frame must be mapped read-write at `base`, CHANNEL_BYTES long,
    /// for as long as the channel is used, shared with the other end only,
    /// and start zeroed (as frames do).
    pub unsafe fn new(base: u64, end: usize, mine: u32, theirs: u32) -> Channel {
        let (send, recv) = (QUEUES[end & 1], QUEUES[(end & 1) ^ 1]);
        Channel { base, send: base + send, recv: base + recv, mine, theirs }
    }

    fn index(at: u64) -> &'static AtomicU32 {
        // SAFETY: `new`'s contract keeps the frame mapped; the indexes are
        // 4-aligned and only accessed atomically.
        unsafe { &*(at as *const AtomicU32) }
    }

    /// The address of the frame.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Sends `message`, waiting while the queue is full.
    pub fn send(&self, message: &Message) -> Result<(), Status> {
        let (head, tail) = (Self::index(self.send + HEAD), Self::index(self.send + TAIL));
        let at = tail.load(Ordering::Relaxed);
        while at.wrapping_sub(head.load(Ordering::Acquire)) >= QUEUE_LEN {
            syscalls::wait(&SYS, self.mine, WaitFlags::NONE, 0)?;
        }
        let slot = self.send + MESSAGES + u64::from(at % QUEUE_LEN) * 64;
        for (i, &word) in message.iter().enumerate() {
            // SAFETY: the slot is inside the frame and the reader leaves it
            // alone until the tail moves past it.
            unsafe { ((slot + 8 * i as u64) as *mut u64).write_volatile(word) };
        }
        tail.store(at.wrapping_add(1), Ordering::Release);
        syscalls::notify(&SYS, self.theirs)
    }

    /// Receives a message, waiting while the queue is empty.
    pub fn recv(&self) -> Result<Message, Status> {
        let (head, tail) = (Self::index(self.recv + HEAD), Self::index(self.recv + TAIL));
        let at = head.load(Ordering::Relaxed);
        while tail.load(Ordering::Acquire) == at {
            syscalls::wait(&SYS, self.mine, WaitFlags::NONE, 0)?;
        }
        let slot = self.recv + MESSAGES + u64::from(at % QUEUE_LEN) * 64;
        let mut message = [0u64; 8];
        for (i, word) in message.iter_mut().enumerate() {
            // SAFETY: the slot is inside the frame, and the writer wrote it
            // before moving the tail, read above with acquire ordering.
            *word = unsafe { ((slot + 8 * i as u64) as *const u64).read_volatile() };
        }
        head.store(at.wrapping_add(1), Ordering::Release);
        syscalls::notify(&SYS, self.theirs)?;
        Ok(message)
    }
}
