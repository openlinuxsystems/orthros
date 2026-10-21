//! The buddy allocator of one zone.
//!
//! A zone hands out blocks of 2^order contiguous frames, order 0 (4 KiB) to
//! [`MAX_ORDER`] (4 MiB). Every block is naturally aligned: a block of
//! order `k` starts at a frame number that is a multiple of 2^k. Its
//! *buddy* is the other half of the aligned block of order `k + 1` that
//! contains it, at frame `pfn ^ 2^k`. Allocating splits a larger free
//! block in halves until one has the asked-for order; freeing merges a
//! block with its buddy, repeatedly, while the buddy is free and of the
//! same order. So no two free buddies of the same order ever exist side by
//! side, which [`Buddy::check`] verifies.
//!
//! The free lists are doubly linked through the [`PageInfo`] records of the
//! blocks' first frames, so the allocator never reads or writes the memory
//! it manages: everything here is safe code, which the host tests and the
//! Kani proofs exercise directly.

#![forbid(unsafe_code)]

use core::fmt;

use crate::page::{NIL, Owner, PageInfo, Region, State};

/// The largest block order: 2^10 frames, 4 MiB.
#[cfg(not(kani))]
pub const MAX_ORDER: u32 = 10;
/// The largest block order. The Kani proofs use zones of 4 frames, where no
/// block is larger than order 2, so they check the same code with the
/// orders above that left out, which keeps the model small.
#[cfg(kani)]
pub const MAX_ORDER: u32 = 2;
/// Number of block orders.
pub const ORDERS: usize = MAX_ORDER as usize + 1;
/// Most regions of RAM one zone can span.
pub const ZONE_REGIONS: usize = 32;

/// Why a block could not be freed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeError {
    /// The order is above [`MAX_ORDER`].
    BadOrder,
    /// The frame number is not a multiple of the block size.
    Misaligned,
    /// The zone does not manage the frame.
    NotManaged,
    /// The block is already free: a double free.
    AlreadyFree,
    /// The frame does not start an allocated block.
    NotABlock,
    /// The block was allocated with another order.
    WrongOrder {
        /// The order it was allocated with.
        allocated: u32,
    },
    /// The block belongs to another owner.
    WrongOwner {
        /// Its owner.
        owner: Owner,
    },
}

impl fmt::Display for FreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FreeError::BadOrder => write!(f, "the order is above {MAX_ORDER}"),
            FreeError::Misaligned => f.write_str("the address is not aligned to the block size"),
            FreeError::NotManaged => f.write_str("the frame allocator does not manage it"),
            FreeError::AlreadyFree => f.write_str("it is already free (a double free)"),
            FreeError::NotABlock => f.write_str("it does not start an allocated block"),
            FreeError::WrongOrder { allocated } => {
                write!(f, "it was allocated as a block of order {allocated}")
            }
            FreeError::WrongOwner { owner } => write!(f, "it belongs to the {}", owner.name()),
        }
    }
}

/// Why a region could not be added to a zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionError {
    /// The zone already spans [`ZONE_REGIONS`] regions.
    TooMany,
    /// The region overlaps one the zone has, or frame numbers do not fit in
    /// 32 bits (above 16 TiB).
    Invalid,
}

/// The free lists of one zone.
pub struct Buddy<'a> {
    regions: [Option<Region<'a>>; ZONE_REGIONS],
    region_count: usize,
    /// First free block of each order, or [`NIL`].
    heads: [u32; ORDERS],
    /// Free blocks of each order.
    counts: [u32; ORDERS],
    /// Free frames.
    free: u64,
    /// Frames ever released to the zone.
    managed: u64,
}

impl<'a> Buddy<'a> {
    /// A zone without memory.
    pub const fn new() -> Buddy<'a> {
        Buddy {
            regions: [None; ZONE_REGIONS],
            region_count: 0,
            heads: [NIL; ORDERS],
            counts: [0; ORDERS],
            free: 0,
            managed: 0,
        }
    }

    /// Adds a region of frame records. Its frames stay reserved until
    /// [`Buddy::release`] gives them to the zone.
    pub fn add_region(&mut self, region: Region<'a>) -> Result<(), RegionError> {
        if region.end() > u64::from(NIL) || region.pages.is_empty() {
            return Err(RegionError::Invalid);
        }
        let regions = &self.regions[..self.region_count];
        if regions.iter().flatten().any(|r| r.base < region.end() && region.base < r.end()) {
            return Err(RegionError::Invalid);
        }
        if self.region_count == ZONE_REGIONS {
            return Err(RegionError::TooMany);
        }
        // Keep the regions sorted by frame number.
        let at = regions.iter().flatten().take_while(|r| r.base < region.base).count();
        self.regions.copy_within(at..self.region_count, at + 1);
        self.regions[at] = Some(region);
        self.region_count += 1;
        Ok(())
    }

    /// The regions, sorted.
    pub fn regions(&self) -> impl Iterator<Item = &Region<'a>> {
        self.regions[..self.region_count].iter().flatten()
    }

    /// The record of frame `pfn`, if the zone has one.
    pub fn info(&self, pfn: u64) -> Option<&'a PageInfo> {
        self.regions().find(|r| r.contains(pfn)).and_then(|r| r.info(pfn))
    }

    /// Whether the zone has a record for frame `pfn`.
    pub fn contains(&self, pfn: u64) -> bool {
        self.regions().any(|r| r.contains(pfn))
    }

    /// Free frames.
    pub fn free_pages(&self) -> u64 {
        self.free
    }

    /// Frames released to the zone.
    pub fn managed_pages(&self) -> u64 {
        self.managed
    }

    /// Free blocks of `order`.
    pub fn free_blocks(&self, order: u32) -> u32 {
        self.counts.get(order as usize).copied().unwrap_or(0)
    }

    /// Whether a block of `order` (or larger) is free.
    pub fn can_allocate(&self, order: u32) -> bool {
        (order as usize..ORDERS).any(|k| self.heads[k] != NIL)
    }

    /// Gives every reserved frame in `start..end` to the zone, in the
    /// largest aligned blocks that fit, and returns how many frames that
    /// was. Frames the zone has no record for, or already manages, are
    /// skipped.
    pub fn release(&mut self, start: u64, end: u64) -> u64 {
        let mut released = 0;
        let mut pfn = start;
        while pfn < end {
            let Some(region) = self.regions().find(|r| r.end() > pfn).copied() else { break };
            pfn = pfn.max(region.base);
            let run_end = end.min(region.end());
            while pfn < run_end {
                let reserved =
                    |f: u64| region.info(f).is_some_and(|i| i.state() == State::Reserved);
                if !reserved(pfn) {
                    pfn += 1;
                    continue;
                }
                let mut order = pfn.trailing_zeros().min(MAX_ORDER);
                while order > 0
                    && (pfn + (1 << order) > run_end || !(pfn..pfn + (1 << order)).all(reserved))
                {
                    order -= 1;
                }
                for tail in pfn + 1..pfn + (1 << order) {
                    if let Some(info) = region.info(tail) {
                        info.set(State::Tail, 0, Owner::Kernel, 0);
                    }
                }
                if let Some(info) = region.info(pfn) {
                    info.set(State::Allocated, order, Owner::Kernel, 0);
                }
                self.managed += 1 << order;
                // Cannot fail: the block was marked allocated just above.
                let _ = self.free(pfn, order, Owner::Kernel);
                released += 1 << order;
                pfn += 1 << order;
            }
        }
        released
    }

    /// Takes a free block of `order` for `owner` and returns its first
    /// frame, or `None` if no block that large is free. `tag` is stored
    /// with the block for its owner.
    pub fn alloc(&mut self, order: u32, owner: Owner, tag: u8) -> Option<u64> {
        let found = (0..ORDERS).find(|&k| k >= order as usize && self.heads[k] != NIL)?;
        let pfn = u64::from(self.heads[found]);
        self.remove(pfn, found as u32);
        let mut current = found as u32;
        while current > order {
            current -= 1;
            self.push(pfn + (1 << current), current);
        }
        self.info(pfn)?.set(State::Allocated, order, owner, tag);
        Some(pfn)
    }

    /// Returns the block of `order` at `pfn`, which `owner` allocated, and
    /// merges it with its free buddies. A free that [`Buddy::check_free`]
    /// refuses changes nothing.
    pub fn free(&mut self, pfn: u64, order: u32, owner: Owner) -> Result<(), FreeError> {
        self.check_free(pfn, order, owner)?;
        let (mut head, mut order) = (pfn, order);
        while order < MAX_ORDER {
            let buddy = head ^ (1 << order);
            let Some(buddy_info) = self.info(buddy) else { break };
            if buddy_info.state() != State::Free || buddy_info.order() != order {
                break;
            }
            self.remove(buddy, order);
            // The higher half's first frame becomes a tail of the merged
            // block.
            let upper = head.max(buddy);
            if let Some(upper_info) = self.info(upper) {
                upper_info.set_state(State::Tail);
            }
            head = head.min(buddy);
            order += 1;
        }
        self.push(head, order);
        Ok(())
    }

    /// Whether [`Buddy::free`] would accept freeing the block of `order` at
    /// `pfn` for `owner`: only if exactly that block is allocated, to that
    /// owner.
    pub fn check_free(&self, pfn: u64, order: u32, owner: Owner) -> Result<(), FreeError> {
        if order > MAX_ORDER {
            return Err(FreeError::BadOrder);
        }
        if !pfn.is_multiple_of(1 << order) {
            return Err(FreeError::Misaligned);
        }
        let info = self.info(pfn).ok_or(FreeError::NotManaged)?;
        match info.state() {
            State::Allocated => {}
            State::Free => return Err(FreeError::AlreadyFree),
            State::Tail | State::Reserved => return Err(FreeError::NotABlock),
        }
        if info.order() != order {
            return Err(FreeError::WrongOrder { allocated: info.order() });
        }
        if info.owner() != owner {
            return Err(FreeError::WrongOwner { owner: info.owner() });
        }
        Ok(())
    }

    /// Puts the block at `pfn` at the front of the list for `order`.
    fn push(&mut self, pfn: u64, order: u32) {
        let Some(info) = self.info(pfn) else { return };
        let head = self.heads[order as usize];
        info.set(State::Free, order, Owner::Kernel, 0);
        info.set_prev(NIL);
        info.set_next(head);
        if let Some(next) = self.info(u64::from(head)).filter(|_| head != NIL) {
            next.set_prev(pfn as u32);
        }
        self.heads[order as usize] = pfn as u32;
        self.counts[order as usize] += 1;
        self.free += 1 << order;
    }

    /// Unlinks the free block at `pfn` from the list for `order`.
    fn remove(&mut self, pfn: u64, order: u32) {
        let Some(info) = self.info(pfn) else { return };
        let (prev, next) = (info.prev(), info.next());
        match self.info(u64::from(prev)).filter(|_| prev != NIL) {
            Some(prev_info) => prev_info.set_next(next),
            None => self.heads[order as usize] = next,
        }
        if let Some(next_info) = self.info(u64::from(next)).filter(|_| next != NIL) {
            next_info.set_prev(prev);
        }
        info.set_next(NIL);
        info.set_prev(NIL);
        self.counts[order as usize] -= 1;
        self.free -= 1 << order;
    }

    /// Checks every invariant of the free lists: each listed block is free,
    /// of its list's order, aligned and backed by records whose other
    /// frames are tails; the links run both ways; the counts add up; and no
    /// free block has a free buddy of the same order. Walks every free
    /// frame, so it is for tests and the debugger, not hot paths.
    pub fn check(&self) -> Result<(), &'static str> {
        let mut free = 0u64;
        for order in 0..ORDERS as u32 {
            let mut pfn = self.heads[order as usize];
            let mut prev = NIL;
            let mut count = 0u32;
            // No list holds more blocks than the zone has room for, so this
            // bound stops a walk around a cycle. (A bound that does not
            // depend on the lists also keeps the Kani proofs small.)
            for _ in 0..=self.managed >> order {
                if pfn == NIL {
                    break;
                }
                if count > self.counts[order as usize] {
                    return Err("a free list is longer than its count (a cycle?)");
                }
                let block = u64::from(pfn);
                let info = self.info(block).ok_or("a free list names a frame outside the zone")?;
                if info.state() != State::Free || info.order() != order {
                    return Err("a listed block is not free, or not of its list's order");
                }
                if !block.is_multiple_of(1 << order) {
                    return Err("a free block is not aligned to its size");
                }
                if info.prev() != prev {
                    return Err("a free list's back link is wrong");
                }
                for offset in 1..1 << order {
                    if self.info(block + offset).map(PageInfo::state) != Some(State::Tail) {
                        return Err("a frame inside a free block is not a tail");
                    }
                }
                if order < MAX_ORDER {
                    let buddy = self.info(block ^ (1 << order));
                    if buddy.is_some_and(|b| b.state() == State::Free && b.order() == order) {
                        return Err("two free buddies were not merged");
                    }
                }
                free += 1 << order;
                count += 1;
                prev = pfn;
                pfn = info.next();
            }
            if pfn != NIL {
                return Err("a free list is longer than the zone could hold (a cycle?)");
            }
            if count != self.counts[order as usize] {
                return Err("a free list's count is wrong");
            }
        }
        if free != self.free {
            return Err("the free frame count is wrong");
        }
        if self.free > self.managed {
            return Err("more frames are free than the zone manages");
        }
        Ok(())
    }
}

impl Default for Buddy<'_> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;

    fn records(count: usize) -> &'static [PageInfo] {
        Box::leak((0..count).map(|_| PageInfo::new()).collect::<Vec<_>>().into_boxed_slice())
    }

    fn zone(base: u64, frames: usize) -> Buddy<'static> {
        let mut buddy = Buddy::new();
        buddy.add_region(Region { base, pages: records(frames) }).unwrap();
        buddy
    }

    #[test]
    fn releases_in_maximal_blocks() {
        let mut buddy = zone(0, 4096);
        // 3..2051: blocks of 1, 4, 8, ... up to the 1024-frame blocks.
        assert_eq!(buddy.release(3, 2051), 2048);
        buddy.check().unwrap();
        assert_eq!(buddy.free_pages(), 2048);
        assert_eq!(buddy.free_blocks(MAX_ORDER), 1);
        // Releasing again changes nothing.
        assert_eq!(buddy.release(3, 2051), 0);
    }

    #[test]
    fn splits_and_merges() {
        let mut buddy = zone(1024, 1024);
        buddy.release(1024, 2048);
        assert_eq!(buddy.free_blocks(MAX_ORDER), 1);
        let a = buddy.alloc(0, Owner::Kernel, 0).unwrap();
        assert_eq!(a, 1024);
        // Splitting left one free block of each order from 0 to 9.
        for order in 0..MAX_ORDER {
            assert_eq!(buddy.free_blocks(order), 1, "order {order}");
        }
        buddy.check().unwrap();
        buddy.free(a, 0, Owner::Kernel).unwrap();
        assert_eq!(buddy.free_blocks(MAX_ORDER), 1);
        buddy.check().unwrap();
    }

    #[test]
    fn rejects_bad_frees() {
        let mut buddy = zone(0, 64);
        buddy.release(0, 64);
        let block = buddy.alloc(2, Owner::Slab, 3).unwrap();
        assert_eq!(buddy.info(block).unwrap().tag(), 3);
        assert_eq!(buddy.free(block, 1, Owner::Slab), Err(FreeError::WrongOrder { allocated: 2 }));
        assert_eq!(
            buddy.free(block, 2, Owner::Kernel),
            Err(FreeError::WrongOwner { owner: Owner::Slab })
        );
        assert_eq!(buddy.free(block + 1, 0, Owner::Slab), Err(FreeError::NotABlock));
        assert_eq!(buddy.free(block + 2, 2, Owner::Slab), Err(FreeError::Misaligned));
        assert_eq!(buddy.free(4096, 0, Owner::Slab), Err(FreeError::NotManaged));
        assert_eq!(buddy.free(block, 11, Owner::Slab), Err(FreeError::BadOrder));
        buddy.free(block, 2, Owner::Slab).unwrap();
        assert_eq!(buddy.free(block, 2, Owner::Slab), Err(FreeError::AlreadyFree));
        buddy.check().unwrap();
    }

    #[test]
    fn regions_merge_only_where_frames_touch() {
        let mut buddy = Buddy::new();
        buddy.add_region(Region { base: 0, pages: records(512) }).unwrap();
        buddy.add_region(Region { base: 512, pages: records(512) }).unwrap();
        assert_eq!(
            buddy.add_region(Region { base: 1000, pages: records(4) }),
            Err(RegionError::Invalid)
        );
        buddy.release(0, 1024);
        // The two regions touch, so their blocks merge into one.
        assert_eq!(buddy.free_blocks(MAX_ORDER), 1);
        buddy.check().unwrap();
    }

    #[derive(Clone, Debug)]
    enum Op {
        Alloc(u32),
        Free(usize),
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        prop::collection::vec(
            prop_oneof![(0u32..=MAX_ORDER).prop_map(Op::Alloc), any::<usize>().prop_map(Op::Free)],
            1..200,
        )
    }

    proptest! {
        /// Random allocations and frees never hand out overlapping or
        /// misaligned blocks, never lose a frame, and keep every invariant.
        #[test]
        fn behaves_like_a_set_of_disjoint_blocks(
            ops in ops(),
            holes in prop::collection::vec(0u64..3000, 0..20),
        ) {
            let mut buddy = zone(0, 3000);
            for start in 0..3000u64 {
                if !holes.contains(&start) {
                    buddy.release(start, start + 1);
                }
            }
            buddy.check().unwrap();
            let managed = buddy.managed_pages();
            let mut live: BTreeMap<u64, u32> = BTreeMap::new();
            for op in ops {
                match op {
                    Op::Alloc(order) => {
                        let Some(pfn) = buddy.alloc(order, Owner::Kernel, 0) else { continue };
                        prop_assert!(pfn.is_multiple_of(1 << order));
                        prop_assert!(pfn + (1 << order) <= 3000);
                        for f in pfn..pfn + (1 << order) {
                            prop_assert!(!holes.contains(&f), "handed out a reserved frame");
                        }
                        let overlaps = live.range(..pfn + (1 << order)).next_back()
                            .is_some_and(|(&p, &o)| p + (1 << o) > pfn);
                        prop_assert!(!overlaps, "overlapping blocks");
                        live.insert(pfn, order);
                    }
                    Op::Free(index) if !live.is_empty() => {
                        let pfn = *live.keys().nth(index % live.len()).unwrap();
                        let order = live.remove(&pfn).unwrap();
                        buddy.free(pfn, order, Owner::Kernel).unwrap();
                    }
                    Op::Free(_) => {}
                }
                let held: u64 = live.values().map(|&o| 1u64 << o).sum();
                prop_assert_eq!(buddy.free_pages() + held, managed);
            }
            buddy.check().unwrap();
            for (pfn, order) in live {
                buddy.free(pfn, order, Owner::Kernel).unwrap();
            }
            buddy.check().unwrap();
            prop_assert_eq!(buddy.free_pages(), managed);
        }
    }
}
