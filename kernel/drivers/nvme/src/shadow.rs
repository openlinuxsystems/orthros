// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shadow doorbells (the Doorbell Buffer Config command, NVM Express Base
//! Specification 2.0), which a controller that a hypervisor emulates may
//! offer (OACS bit 8): its I/O queues' doorbells also in a page of
//! memory, which the host writes without leaving the virtual machine,
//! and a second page of *event indexes*, in which the controller says, for
//! each, the entry it wants to hear of by register.
//!
//! The pages are laid out as the doorbell registers are: submission queue
//! *y*'s tail at 2*y* strides, completion queue *y*'s head one stride on.
//! The driver writes a queue's new tail or head to its shadow doorbell,
//! then reads its event index, and writes the doorbell register too only
//! if the move passed that entry ([`passes`]); otherwise the controller
//! reads the shadow doorbell when it next looks at the queue. The admin
//! queue keeps its registers alone.

use radix_hw::{Dma, device_barrier};

use crate::command::PAGE;

/// Where the event indexes are: the page after the shadow doorbells.
const EVENTS: usize = PAGE as usize;

/// The shadow doorbells and event indexes of one controller, in
/// [`Shadow::PAGES`] pages of DMA memory, which the controller uses for as
/// long as it is enabled: they are kept for the kernel's life
/// (`&'static`).
pub struct Shadow {
    memory: Dma,
}

impl core::fmt::Debug for Shadow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shadow").field("memory", &self.memory).finish()
    }
}

impl Shadow {
    /// Pages of DMA memory the shadow doorbells take, with their event
    /// indexes.
    pub const PAGES: usize = 2;
    /// How many bytes the doorbells take at most: a page.
    pub(crate) const ROOM: usize = EVENTS;

    /// Shadow doorbells in `memory`, [`Shadow::PAGES`] pages; `None` if
    /// it is smaller.
    pub fn new(memory: Dma) -> Option<Shadow> {
        (memory.len() >= Shadow::PAGES * PAGE as usize).then_some(Shadow { memory })
    }

    /// The physical addresses of the shadow doorbells and of the event
    /// indexes, which the command names.
    pub(crate) fn addresses(&self) -> (u64, u64) {
        (self.memory.phys(), self.memory.phys() + EVENTS as u64)
    }

    /// Zeroes the doorbells and event indexes, for a controller that
    /// starts afresh: every queue is empty, and the controller has asked
    /// for nothing yet.
    pub(crate) fn clear(&self) {
        self.memory.zero();
    }

    /// Moves the shadow doorbell at `offset` to `value`, and says whether
    /// the doorbell register must be written too: whether the move passed
    /// the entry the controller's event index names.
    pub(crate) fn update(&self, offset: usize, value: u16) -> bool {
        let old = self.memory.read32(offset) as u16;
        self.memory.write32(offset, u32::from(value));
        // The new value before the event index: the controller writes its
        // event index before it reads the shadow doorbell, so at least
        // one of the two sees the other's write.
        device_barrier();
        let event = self.memory.read32(EVENTS + offset) as u16;
        passes(old, value, event)
    }
}

/// Whether a doorbell moved from `old` to `new` passed `event`: whether
/// `event` is one of the entries from `old` up to `new`, without `new`,
/// going on round past the end of the queue (entries are below 2^15).
pub fn passes(old: u16, new: u16, event: u16) -> bool {
    event.wrapping_sub(old) < new.wrapping_sub(old)
}

#[cfg(kani)]
mod proofs {
    use super::passes;
    use crate::ENTRIES;

    /// The check is exactly "the event index is among the entries the
    /// move passed", counted one by one round a queue of [`ENTRIES`].
    #[kani::proof]
    #[kani::unwind(66)]
    fn passes_is_the_entries_passed() {
        let old: u16 = kani::any_where(|&old| old < ENTRIES);
        let new: u16 = kani::any_where(|&new| new < ENTRIES);
        let event: u16 = kani::any_where(|&event| event < ENTRIES);
        let mut at = old;
        let mut passed = false;
        while at != new {
            passed |= at == event;
            at = (at + 1) % ENTRIES;
        }
        assert_eq!(passes(old, new, event), passed);
    }
}
