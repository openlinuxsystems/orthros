// SPDX-FileCopyrightText: 2026 The Dukovo contributors
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
//! zone's lock while the frame is free, a CPU's cache while that holds it,
//! and whoever allocated it after that. Relaxed loads and stores are plain
//! moves on x86-64, and atomics let two locks share the array without
//! `unsafe`.

#![forbid(unsafe_code)]

use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};

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
    /// A free frame that a CPU's cache holds: allocated, as far as its
    /// zone knows ([`crate::Frames`]).
    Cached = 4,
}

impl State {
    fn from_bits(bits: u32) -> State {
        match bits & 0xff {
            1 => State::Free,
            2 => State::Allocated,
            3 => State::Tail,
            4 => State::Cached,
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
    /// A page of the page cache: file data and filesystem metadata
    /// (`radix-vfs`, `radix-block`), which can be dropped and read again.
    PageCache = 7,
    /// A page of a program's anonymous memory, which the kernel hands out
    /// when the program first touches it; `aux` counts the address spaces
    /// that map it ([`PageInfo::share_count`]), more than one after a fork
    /// until each has written to it, and the devices it is lent to
    /// ([`PageInfo::pin`]).
    Anon = 8,
}

impl Owner {
    /// Every owner, in the order of their numbers.
    pub const ALL: [Owner; 9] = [
        Owner::Kernel,
        Owner::PageTable,
        Owner::Slab,
        Owner::Heap,
        Owner::Area,
        Owner::PerCpu,
        Owner::Memory,
        Owner::PageCache,
        Owner::Anon,
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
            Owner::PageCache => "page cache",
            Owner::Anon => "anonymous memory",
        }
    }
}

/// The metadata of one frame.
///
/// `word` packs the state (bits 0-7), the block order (8-15), the owner
/// (16-23) and an owner-defined byte (24-31: a slab's size class). `next`
/// and `prev` link the frame into a free list, or a slab into its cache's
/// list of partly used slabs, and hold a page table's lines in use
/// ([`PageInfo::table_lines`]); `aux` is owner-defined.
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

    /// Marks the frame cached if it is a block of one frame allocated to
    /// `owner`, and says whether it was. One atomic change: of two frees of
    /// a frame at once, only one gets it.
    pub(crate) fn cache(&self, owner: Owner) -> bool {
        let word = self.word.load(Relaxed);
        let allocated = State::Allocated as u32 | u32::from(owner as u8) << 16;
        word & 0x00ff_ffff == allocated
            && self
                .word
                .compare_exchange(word, (word & !0xff) | State::Cached as u32, Relaxed, Relaxed)
                .is_ok()
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

    /// For a frame allocated as a page table, which of its 64 lines of 8
    /// entries may hold an entry, a bit each: the architecture's walks of
    /// the table skip the others. `next` and `prev` hold it while the frame
    /// is allocated; only whoever holds the table's lock takes lines out.
    pub fn table_lines(&self) -> u64 {
        u64::from(self.next.load(Relaxed)) | u64::from(self.prev.load(Relaxed)) << 32
    }

    /// Sets the lines [`PageInfo::table_lines`] gives.
    pub fn set_table_lines(&self, lines: u64) {
        self.next.store(lines as u32, Relaxed);
        self.prev.store((lines >> 32) as u32, Relaxed);
    }

    /// Adds `lines` to those [`PageInfo::table_lines`] gives, atomically:
    /// the kernel's own tables gain entries under more than one lock.
    pub fn add_table_lines(&self, lines: u64) {
        for (half, bits) in [(&self.next, lines as u32), (&self.prev, (lines >> 32) as u32)] {
            // Most entries go to a line that holds some already, which
            // needs no locked instruction.
            if half.load(Relaxed) & bits != bits {
                half.fetch_or(bits, Relaxed);
            }
        }
    }

    pub(crate) fn aux(&self) -> u32 {
        self.aux.load(Relaxed)
    }

    pub(crate) fn set_aux(&self, value: u32) {
        self.aux.store(value, Relaxed);
    }

    /// For a page of anonymous memory ([`Owner::Anon`]): how many address
    /// spaces map it. Unlike the other fields it is changed by atomic
    /// read-modify-writes, since two spaces that share a page after a fork
    /// let go of it under locks of their own.
    pub fn share_count(&self) -> u32 {
        self.aux.load(Acquire) & SHARES
    }

    /// Sets the count of a page of anonymous memory just allocated, which
    /// nothing else can see yet.
    pub fn set_share_count(&self, count: u32) {
        self.aux.store(count, Release);
    }

    /// One more address space maps a page of anonymous memory.
    pub fn share(&self) {
        self.aux.fetch_add(1, AcqRel);
    }

    /// One address space fewer maps a page of anonymous memory: returns
    /// what still holds it, spaces and pins. The one that sees 0 frees the
    /// page.
    pub fn unshare(&self) -> u32 {
        self.aux.fetch_sub(1, AcqRel) - 1
    }

    /// Holds a page of anonymous memory that a space maps in place for a
    /// device that reads or writes it, until [`PageInfo::unpin`]: if the
    /// space lets go of it meanwhile, the page stays until then. A pin
    /// does not count as a sharer, so the space still writes to a page
    /// only it maps in place. `false` if the page has as many pins as it
    /// holds already (255).
    pub fn pin(&self) -> bool {
        self.aux
            .try_update(AcqRel, Acquire, |aux| (aux >> PIN_SHIFT != 0xff).then(|| aux + PIN))
            .is_ok()
    }

    /// Lets go of a pin, as [`PageInfo::unshare`] lets go of a space.
    pub fn unpin(&self) -> u32 {
        self.aux.fetch_sub(PIN, AcqRel) - PIN
    }

    /// How many pins a page of anonymous memory has.
    pub fn pins(&self) -> u32 {
        self.aux.load(Acquire) >> PIN_SHIFT
    }
}

/// Where the pins of a page of anonymous memory start in `aux`, above its
/// share count.
const PIN_SHIFT: u32 = 24;

/// One pin.
const PIN: u32 = 1 << PIN_SHIFT;

/// The share count's bits of `aux`.
const SHARES: u32 = PIN - 1;

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
    fn only_a_single_allocated_frame_of_its_owner_is_cached() {
        let info = PageInfo::new();
        info.set(State::Allocated, 1, Owner::Anon, 0);
        assert!(!info.cache(Owner::Anon), "a block of two frames");
        info.set(State::Allocated, 0, Owner::Anon, 7);
        assert!(!info.cache(Owner::Kernel), "another owner's frame");
        assert!(info.cache(Owner::Anon));
        assert_eq!((info.state(), info.owner(), info.tag()), (State::Cached, Owner::Anon, 7));
        assert!(!info.cache(Owner::Anon), "a second free");
    }

    #[test]
    fn pins_hold_a_page_without_sharing_it() {
        let info = PageInfo::new();
        info.set_share_count(1);
        assert!(info.pin() && info.pin());
        assert_eq!((info.share_count(), info.pins()), (1, 2));
        info.share();
        assert_eq!(info.share_count(), 2);
        // The spaces let go first: the pins keep it.
        assert_ne!(info.unshare(), 0);
        assert_ne!(info.unshare(), 0);
        assert_eq!((info.share_count(), info.pins()), (0, 2));
        assert_ne!(info.unpin(), 0);
        assert_eq!(info.unpin(), 0);
        // At most 255 at once.
        info.set_share_count(1);
        for _ in 0..255 {
            assert!(info.pin());
        }
        assert!(!info.pin());
        assert_eq!((info.share_count(), info.pins()), (1, 255));
    }

    #[test]
    fn a_page_tables_lines_fill_both_links() {
        let info = PageInfo::new();
        info.set_table_lines(1 << 3 | 1 << 40);
        info.add_table_lines(1 << 63 | 1 << 3);
        assert_eq!(info.table_lines(), 1 << 3 | 1 << 40 | 1 << 63);
        info.add_table_lines(0);
        assert_eq!(info.table_lines(), 1 << 3 | 1 << 40 | 1 << 63);
        info.set_table_lines(0);
        assert_eq!((info.table_lines(), info.next(), info.prev()), (0, 0, 0));
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
