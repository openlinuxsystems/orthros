// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A radix-ring(2) ring, from the program's side: it writes submission
//! entries and moves the submission tail, reads completions and moves the
//! completion head, and enters the kernel with `ring_enter` to have the
//! entries carried out.

use core::sync::atomic::{AtomicU32, Ordering};

use radix_abi::Status;
use radix_abi::ring::{Cqe, Opcode, RingHeader, RingIndexes, Sqe};
use radix_abi::syscalls;

use crate::SYS;

/// A ring mapped in this program.
pub struct Ring {
    base: u64,
    cap: u32,
    sq_mask: u32,
    cq_mask: u32,
    sqes: u64,
    cqes: u64,
}

/// A completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    /// The entry's user data.
    pub user_data: u64,
    /// Its status: 0, or an error (radix-errors(2)).
    pub status: u64,
    /// What it returned.
    pub values: [u64; 2],
}

impl Ring {
    /// Makes a ring with `entries` submission entries, maps it at
    /// `address` in the address space in slot `space`, and puts its
    /// capability in slot `to`.
    pub fn setup(space: u32, address: u64, entries: u32, to: u32) -> Result<Ring, Status> {
        syscalls::ring_setup(&SYS, space, address, entries, to)?;
        let ring = Ring { base: address, cap: to, sq_mask: 0, cq_mask: 0, sqes: 0, cqes: 0 };
        let word = |offset: usize| ring.word(offset).load(Ordering::Relaxed);
        Ok(Ring {
            sq_mask: word(RingHeader::OFFSET_SQ + RingIndexes::OFFSET_MASK),
            cq_mask: word(RingHeader::OFFSET_CQ + RingIndexes::OFFSET_MASK),
            sqes: address + u64::from(word(RingHeader::OFFSET_SQES)),
            cqes: address + u64::from(word(RingHeader::OFFSET_CQES)),
            ..ring
        })
    }

    /// Its capability's slot.
    pub fn cap(&self) -> u32 {
        self.cap
    }

    fn word(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: the kernel mapped the ring read-write at `base` and keeps
        // it mapped while its capability exists; the header's words are
        // 4-aligned, shared with the kernel, and only accessed atomically.
        unsafe { &*((self.base + offset as u64) as *const AtomicU32) }
    }

    fn sq(&self, field: usize) -> &AtomicU32 {
        self.word(RingHeader::OFFSET_SQ + field)
    }

    fn cq(&self, field: usize) -> &AtomicU32 {
        self.word(RingHeader::OFFSET_CQ + field)
    }

    /// Queues an entry, if the submission queue has room. The kernel
    /// carries it out at the next `enter`.
    pub fn push(&self, opcode: Opcode, cap: u32, user_data: u64, args: [u64; 6]) -> bool {
        let tail = self.sq(RingIndexes::OFFSET_TAIL).load(Ordering::Relaxed);
        let head = self.sq(RingIndexes::OFFSET_HEAD).load(Ordering::Acquire);
        if tail.wrapping_sub(head) > self.sq_mask {
            return false;
        }
        let entry = self.sqes + u64::from(tail & self.sq_mask) * Sqe::SIZE as u64;
        let mut words = [0u64; 8];
        words[0] = u64::from(opcode.raw()) | (u64::from(cap) << 32);
        words[1] = user_data;
        words[2..].copy_from_slice(&args);
        for (i, word) in words.into_iter().enumerate() {
            // SAFETY: the entry is inside the submission array the kernel
            // mapped (masked index), 8-aligned, and the kernel reads it
            // only after the tail below moves past it.
            unsafe { ((entry + 8 * i as u64) as *mut u64).write_volatile(word) };
        }
        self.sq(RingIndexes::OFFSET_TAIL).store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Hands the kernel up to `submit` entries and waits for
    /// `min_complete` completions (or `timeout_ns`, when not 0). Returns
    /// how many entries it took and how many completions wait.
    pub fn enter(
        &self,
        submit: u32,
        min_complete: u32,
        timeout_ns: u64,
    ) -> Result<(u32, u32), Status> {
        let done = syscalls::ring_enter(&SYS, self.cap, submit, min_complete, timeout_ns)?;
        Ok((done.submitted, done.ready))
    }

    /// Takes the next completion, if one is there.
    pub fn pop(&self) -> Option<Completion> {
        let head = self.cq(RingIndexes::OFFSET_HEAD).load(Ordering::Relaxed);
        let tail = self.cq(RingIndexes::OFFSET_TAIL).load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let entry = self.cqes + u64::from(head & self.cq_mask) * Cqe::SIZE as u64;
        let read = |offset: usize| {
            // SAFETY: the entry is inside the completion array (masked
            // index) and the kernel wrote it before moving the tail, which
            // was read with acquire ordering.
            unsafe { ((entry + offset as u64) as *const u64).read_volatile() }
        };
        let completion = Completion {
            user_data: read(Cqe::OFFSET_USER_DATA),
            status: read(Cqe::OFFSET_STATUS),
            values: [read(Cqe::OFFSET_VALUES), read(Cqe::OFFSET_VALUES + 8)],
        };
        self.cq(RingIndexes::OFFSET_HEAD).store(head.wrapping_add(1), Ordering::Release);
        Some(completion)
    }

    /// The completion queue's tail as the kernel published it, for tests.
    pub fn completion_tail(&self) -> u32 {
        self.cq(RingIndexes::OFFSET_TAIL).load(Ordering::Acquire)
    }

    /// Overwrites the completion head, for tests of what the kernel does
    /// with nonsense.
    pub fn set_completion_head(&self, head: u32) {
        self.cq(RingIndexes::OFFSET_HEAD).store(head, Ordering::Release);
    }

    /// Overwrites the submission tail, for tests of what the kernel does
    /// with nonsense.
    pub fn set_submission_tail(&self, tail: u32) {
        self.sq(RingIndexes::OFFSET_TAIL).store(tail, Ordering::Release);
    }

    /// The submission queue's indexes: head and tail.
    pub fn submission(&self) -> (u32, u32) {
        (
            self.sq(RingIndexes::OFFSET_HEAD).load(Ordering::Acquire),
            self.sq(RingIndexes::OFFSET_TAIL).load(Ordering::Relaxed),
        )
    }
}
