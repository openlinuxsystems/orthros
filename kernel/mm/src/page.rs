// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Per-frame metadata.
//!
//! Every frame of RAM the allocator manages has a 16-byte [`PageInfo`] in an
//! array that the architecture sets aside when it hands memory over (one
//! array per region of RAM). The buddy allocator keeps its free lists in
//! these records, so it never touches the frames themselves; the slab
//! allocator keeps each slab's bookkeeping in the record of the slab's first
//! frame.
//!
//! The fields are atomics, but each record has one owner at a time: the
//! zone's lock while the frame is free, and whoever allocated it after
//! that. Relaxed loads and stores are plain moves on x86-64, and atomics let
//! two locks share the array without `unsafe`.

#![forbid(unsafe_code)]

use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::Relaxed;

/// Size of a frame (a page) in bytes.
pub const PAGE_SIZE: usize = 4096;
/// log2 of [`PAGE_SIZE`].
pub const PAGE_SHIFT: u32 = 12;
/// The end of a list of frames.
pub(crate) const NIL: u32 = u32::MAX;

/// What a frame is, as far as the frame allocator knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// Not managed: a hole, firmware memory, the kernel image, or memory
    /// handed out before the allocator started.
    Reserved = 0,
    /// The first frame of a free block.
    Free = 1,
    /// The first frame of an allocated block.
    Allocated = 2,
    /// Any later frame of a free or allocated block.
    Tail = 3,
}

impl State {
    fn from_bits(bits: u32) -> State {
        match bits & 0xff {
            1 => State::Free,
            2 => State::Allocated,
            3 => State::Tail,
            _ => State::Reserved,
        }
    }
}

/// Who holds an allocated block, for accounting and for checking frees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Owner {
    /// Kernel code that called the frame allocator directly.
    Kernel = 0,
    /// A page table.
    PageTable = 1,
    /// A slab of the kernel heap.
    Slab = 2,
    /// A heap allocation too large for a slab.
    Heap = 3,
    /// A page of a mapped area (stacks and other mappings with guard pages).
    Area = 4,
    /// The allocator's own per-CPU caches.
    PerCpu = 5,
    /// Memory handed to the capability store as a memory object
    /// (`radix-cap`), from which frames and other objects are retyped.
    Memory = 6,
}

impl Owner {
    /// Every owner, in the order of their numbers.
    pub const ALL: [Owner; 7] = [
        Owner::Kernel,
        Owner::PageTable,
        Owner::Slab,
        Owner::Heap,
        Owner::Area,
        Owner::PerCpu,
        Owner::Memory,
    ];

    fn from_bits(bits: u32) -> Owner {
        Owner::ALL.get(bits as usize & 0xff).copied().unwrap_or(Owner::Kernel)
    }

    /// Lower-case name for reports.
    pub fn name(self) -> &'static str {
        match self {
            Owner::Kernel => "kernel",
            Owner::PageTable => "page tables",
            Owner::Slab => "heap slabs",
            Owner::Heap => "large heap blocks",
            Owner::Area => "mapped areas",
            Owner::PerCpu => "per-CPU caches",
            Owner::Memory => "memory objects",
        }
    }
}

/// The metadata of one frame.
///
/// `word` packs the state (bits 0-7), the block order (8-15), the owner
/// (16-23) and an owner-defined byte (24-31: a slab's size class). `next`
/// and `prev` link the frame into a free list, or a slab into its cache's
/// list of partly used slabs; `aux` is owner-defined.
#[derive(Debug, Default)]
#[repr(C)]
pub struct PageInfo {
    next: AtomicU32,
    prev: AtomicU32,
    word: AtomicU32,
    aux: AtomicU32,
}

impl PageInfo {
    /// A record for a frame the allocator does not manage. All-zero bytes
    /// are this same value, so zeroed memory is a valid array of records.
    pub const fn new() -> PageInfo {
        PageInfo {
            next: AtomicU32::new(0),
            prev: AtomicU32::new(0),
            word: AtomicU32::new(0),
            aux: AtomicU32::new(0),
        }
    }

    /// The frame's state.
    pub fn state(&self) -> State {
        State::from_bits(self.word.load(Relaxed))
    }

    /// The order of the block this frame heads.
    pub fn order(&self) -> u32 {
        (self.word.load(Relaxed) >> 8) & 0xff
    }

    /// The owner of the allocated block this frame heads.
    pub fn owner(&self) -> Owner {
        Owner::from_bits(self.word.load(Relaxed) >> 16)
    }

    /// The owner-defined byte (a slab's size class).
    pub fn tag(&self) -> u8 {
        (self.word.load(Relaxed) >> 24) as u8
    }

    pub(crate) fn set(&self, state: State, order: u32, owner: Owner, tag: u8) {
        let word = state as u32
            | (order & 0xff) << 8
            | u32::from(owner as u8) << 16
            | u32::from(tag) << 24;
        self.word.store(word, Relaxed);
    }

    pub(crate) fn set_state(&self, state: State) {
        let word = self.word.load(Relaxed);
        self.word.store((word & !0xff) | state as u32, Relaxed);
    }

    pub(crate) fn next(&self) -> u32 {
        self.next.load(Relaxed)
    }

    pub(crate) fn prev(&self) -> u32 {
        self.prev.load(Relaxed)
    }

    pub(crate) fn set_next(&self, pfn: u32) {
        self.next.store(pfn, Relaxed);
    }

    pub(crate) fn set_prev(&self, pfn: u32) {
        self.prev.store(pfn, Relaxed);
    }

    pub(crate) fn aux(&self) -> u32 {
        self.aux.load(Relaxed)
    }

    pub(crate) fn set_aux(&self, value: u32) {
        self.aux.store(value, Relaxed);
    }
}

/// A run of frames with metadata: frame `base + i` is described by
/// `pages[i]`.
#[derive(Clone, Copy, Debug)]
pub struct Region<'a> {
    /// The first frame number.
    pub base: u64,
    /// One record per frame.
    pub pages: &'a [PageInfo],
}

impl<'a> Region<'a> {
    /// One past the last frame number.
    pub fn end(&self) -> u64 {
        self.base + self.pages.len() as u64
    }

    /// Whether frame `pfn` is in the region.
    pub fn contains(&self, pfn: u64) -> bool {
        pfn >= self.base && pfn < self.end()
    }

    /// The record of frame `pfn`.
    pub fn info(&self, pfn: u64) -> Option<&'a PageInfo> {
        let index = usize::try_from(pfn.checked_sub(self.base)?).ok()?;
        self.pages.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_the_word() {
        let info = PageInfo::new();
        assert_eq!(info.state(), State::Reserved);
        info.set(State::Allocated, 9, Owner::Slab, 17);
        assert_eq!(
            (info.state(), info.order(), info.owner(), info.tag()),
            (State::Allocated, 9, Owner::Slab, 17)
        );
        info.set_state(State::Tail);
        assert_eq!((info.state(), info.order(), info.owner()), (State::Tail, 9, Owner::Slab));
        assert_eq!(core::mem::size_of::<PageInfo>(), 16);
    }

    #[test]
    fn regions_find_their_frames() {
        let pages: Vec<PageInfo> = (0..4).map(|_| PageInfo::new()).collect();
        let region = Region { base: 10, pages: &pages };
        assert!(region.info(9).is_none());
        assert!(region.info(13).is_some());
        assert!(region.info(14).is_none());
        assert_eq!(region.end(), 14);
    }
}
