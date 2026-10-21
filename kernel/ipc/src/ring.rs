// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A ring's shared memory and the kernel's accounting of its queues
//! (radix-ring(2), ADR 0034).
//!
//! The program writes the submission queue's tail and the completion
//! queue's head; the kernel never trusts either. It keeps its own copy of
//! the submission head and the completion tail, masks every index, and
//! takes a submission entry only after reserving room for its completion,
//! counting the completions still unread at the head the program last
//! wrote. So whatever the program writes, the kernel writes inside the
//! ring and never more completions than the queue holds unread; a program
//! that writes nonsense only confuses itself.

use radix_abi::address_space::PAGE_SIZE;
use radix_abi::ring::{Cqe, ENTRIES_MAX, Sqe};

/// Where a ring's parts lie in its shared memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Submission entries: a power of two from 1 to ENTRIES_MAX.
    pub entries: u32,
    /// Completion entries: twice as many.
    pub completions: u32,
    /// Offset of the submission entries; the header has the first page.
    pub sqes: u32,
    /// Offset of the completion entries, on a page boundary.
    pub cqes: u32,
    /// Size of the whole mapping, a whole number of pages.
    pub size: u32,
}

impl Layout {
    /// The layout of a ring with `entries` submission entries, if that is
    /// a power of two from 1 to ENTRIES_MAX.
    pub const fn new(entries: u32) -> Option<Layout> {
        if entries == 0 || entries > ENTRIES_MAX || !entries.is_power_of_two() {
            return None;
        }
        let page = PAGE_SIZE as u32;
        let completions = entries * 2;
        let sqes = page;
        let cqes = sqes + (entries * Sqe::SIZE as u32).next_multiple_of(page);
        let size = cqes + (completions * Cqe::SIZE as u32).next_multiple_of(page);
        Some(Layout { entries, completions, sqes, cqes, size })
    }

    /// How many pages the mapping takes.
    pub const fn pages(&self) -> u32 {
        self.size / PAGE_SIZE as u32
    }

    /// The offset of submission entry `index` (masked).
    pub const fn sqe(&self, index: u32) -> u32 {
        self.sqes + (index & (self.entries - 1)) * Sqe::SIZE as u32
    }

    /// The offset of completion entry `index` (masked).
    pub const fn cqe(&self, index: u32) -> u32 {
        self.cqes + (index & (self.completions - 1)) * Cqe::SIZE as u32
    }
}

/// The kernel's own view of a ring's two queues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Queues {
    entries: u32,
    completions: u32,
    /// The next submission entry the kernel takes.
    sq_head: u32,
    /// Where the kernel writes its next completion.
    cq_tail: u32,
    /// Entries taken whose completions are not written yet.
    reserved: u32,
}

/// The program's submission tail claims more entries than the queue holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overrun;

impl Queues {
    /// Empty queues for `layout`.
    pub const fn new(layout: &Layout) -> Queues {
        Queues {
            entries: layout.entries,
            completions: layout.completions,
            sq_head: 0,
            cq_tail: 0,
            reserved: 0,
        }
    }

    /// The submission head, which the kernel publishes.
    pub const fn sq_head(&self) -> u32 {
        self.sq_head
    }

    /// The completion tail, which the kernel publishes.
    pub const fn cq_tail(&self) -> u32 {
        self.cq_tail
    }

    /// Entries taken whose completions are still to come.
    pub const fn reserved(&self) -> u32 {
        self.reserved
    }

    /// How many entries the program has submitted that the kernel has not
    /// taken, from the tail it wrote.
    pub const fn available(&self, sq_tail: u32) -> Result<u32, Overrun> {
        let pending = sq_tail.wrapping_sub(self.sq_head);
        if pending <= self.entries { Ok(pending) } else { Err(Overrun) }
    }

    /// How many completions are written and not yet read, from the head
    /// the program wrote. A head that makes no sense (ahead of the tail,
    /// or further behind than the queue is long) counts as nothing read.
    pub const fn unread(&self, cq_head: u32) -> u32 {
        let unread = self.cq_tail.wrapping_sub(cq_head);
        if unread <= self.completions { unread } else { self.completions }
    }

    /// Room for completions nobody has claimed.
    pub const fn room(&self, cq_head: u32) -> u32 {
        self.completions.saturating_sub(self.unread(cq_head)).saturating_sub(self.reserved)
    }

    /// Takes the next submission entry, reserving room for its completion,
    /// and returns its index (masked); `None` when the completion queue
    /// has no room for another.
    pub const fn take(&mut self, cq_head: u32) -> Option<u32> {
        if self.room(cq_head) == 0 {
            return None;
        }
        let index = self.sq_head & (self.entries - 1);
        self.sq_head = self.sq_head.wrapping_add(1);
        self.reserved += 1;
        Some(index)
    }

    /// The index (masked) to write the completion of a taken entry at;
    /// `None` if no entry is waiting for one.
    pub const fn complete(&mut self) -> Option<u32> {
        if self.reserved == 0 {
            return None;
        }
        self.reserved -= 1;
        Some(self.push())
    }

    /// The index (masked) to write a completion that no entry of this ring
    /// reserved, such as a message from another ring; `None` when there is
    /// no room for it.
    pub const fn post(&mut self, cq_head: u32) -> Option<u32> {
        if self.room(cq_head) == 0 {
            return None;
        }
        Some(self.push())
    }

    const fn push(&mut self) -> u32 {
        let index = self.cq_tail & (self.completions - 1);
        self.cq_tail = self.cq_tail.wrapping_add(1);
        index
    }
}
