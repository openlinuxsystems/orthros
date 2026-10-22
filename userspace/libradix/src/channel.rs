// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shared-memory channels: two programs map the same frame and pass
//! fixed-size messages through it, each way in a queue of its own, with a
//! notification each to sleep on when there is nothing to do.
//!
//! The kernel takes part only to wake a program: the messages themselves
//! never pass through it. A queue has one writer and one reader; each
//! moves only its own index (release ordering after writing a message or
//! reading one, acquire before looking at the other's), so neither ever
//! sees half a message.
//!
//! A program that finds nothing to do (no message, or no room) says so in
//! a `sleeping` word of the queue, the reader's or the writer's, looks
//! once more, and waits on its notification. The other end, after moving
//! that queue's other index, signals the notification only if the word is
//! set, so a busy channel makes no system calls at all, and a ping-pong
//! one only for the wake-ups it needs. The two sides follow Dekker's
//! pattern: each stores (the word, or an index) and then loads what the
//! other stores, all sequentially consistent, so at least one of them
//! sees the other's store: either the sleeper sees the moved index and
//! does not wait, or the other end sees the word and signals. A
//! notification keeps a signal that comes before the wait
//! (radix-notification(2)), so no wake-up is lost either way;
//! `userspace/libradix/tla/Channel.tla` checks this.

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
/// Where a queue's parts start: the head and the reader's `sleeping` word,
/// which the reader writes, on one cache line; the tail and the writer's
/// `sleeping` word on the next; then the messages.
const HEAD: u64 = 0;
const READER_SLEEPING: u64 = 8;
const TAIL: u64 = 64;
const WRITER_SLEEPING: u64 = 72;
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
        let full = |order| at.wrapping_sub(head.load(order)) >= QUEUE_LEN;
        self.sleep_while(self.send + WRITER_SLEEPING, full)?;
        let slot = self.send + MESSAGES + u64::from(at % QUEUE_LEN) * 64;
        for (i, &word) in message.iter().enumerate() {
            // SAFETY: the slot is inside the frame and the reader leaves it
            // alone until the tail moves past it.
            unsafe { ((slot + 8 * i as u64) as *mut u64).write_volatile(word) };
        }
        tail.store(at.wrapping_add(1), Ordering::SeqCst);
        self.wake_other(self.send + READER_SLEEPING)
    }

    /// Receives a message, waiting while the queue is empty.
    pub fn recv(&self) -> Result<Message, Status> {
        let (head, tail) = (Self::index(self.recv + HEAD), Self::index(self.recv + TAIL));
        let at = head.load(Ordering::Relaxed);
        self.sleep_while(self.recv + READER_SLEEPING, |order| tail.load(order) == at)?;
        let slot = self.recv + MESSAGES + u64::from(at % QUEUE_LEN) * 64;
        let mut message = [0u64; 8];
        for (i, word) in message.iter_mut().enumerate() {
            // SAFETY: the slot is inside the frame, and the writer wrote it
            // before moving the tail, read above with acquire ordering.
            *word = unsafe { ((slot + 8 * i as u64) as *const u64).read_volatile() };
        }
        head.store(at.wrapping_add(1), Ordering::SeqCst);
        self.wake_other(self.recv + WRITER_SLEEPING)?;
        Ok(message)
    }

    /// Waits on this end's notification for as long as `blocked` (given
    /// the ordering to load the other end's index with) says so, telling
    /// the other end through the `sleeping` word at `word`.
    fn sleep_while(&self, word: u64, blocked: impl Fn(Ordering) -> bool) -> Result<(), Status> {
        let sleeping = Self::index(word);
        while blocked(Ordering::Acquire) {
            sleeping.store(1, Ordering::SeqCst);
            // Look again after saying so: an index the other end moved
            // before it could see the word is seen here.
            let waited = if blocked(Ordering::SeqCst) {
                syscalls::wait(&SYS, self.mine, WaitFlags::NONE, 0).map(|_| ())
            } else {
                Ok(())
            };
            sleeping.store(0, Ordering::Relaxed);
            waited?;
        }
        Ok(())
    }

    /// Signals the other end if its `sleeping` word at `word` says it may
    /// be waiting for the index this end has just moved (sequentially
    /// consistent, so the move comes before the load here).
    fn wake_other(&self, word: u64) -> Result<(), Status> {
        if Self::index(word).load(Ordering::SeqCst) != 0 {
            syscalls::notify(&SYS, self.theirs)?;
        }
        Ok(())
    }
}
