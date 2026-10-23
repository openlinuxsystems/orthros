// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Address spaces and frames, and mapping one into the other.
//!
//! An address space keeps a list of regions: runs of pages, each mapping
//! part of one frame or ring. A frame remembers which address spaces map
//! it, so that when its last capability goes (and its memory returns to
//! the memory object it came from) it is unmapped from every one of them
//! first: a program never keeps a mapping of memory the kernel has handed
//! to someone else.
//!
//! A program never sees what a frame's memory held before: frames are
//! zeroed before anything maps them. Retyping records new frames under the
//! capability store's lock, and the thread that retyped them zeroes them
//! after letting go of it, in steps of [`ZERO_STEP`] pages, before its
//! call returns; a thread that maps one sooner zeroes what is left itself
//! ([`FrameObject::zero`]). Deleting a frame part-way waits for the step
//! under way and stops the rest, before its memory can go back. So a large
//! retype holds up no other change to the store.
//!
//! Since Phase 8 a region can also be anonymous memory (`anon`): pages the
//! kernel makes when a program first touches them, zeroed or filled from
//! a file, charged to the space's resource domain, and shared after a
//! fork until one side writes to them. `futex` keeps the threads that
//! wait on words of an address space.
//!
//! Lock order: the capability store, then a frame's zeroing step, then its
//! list of address spaces, then an address space's regions (then the
//! architecture's page-table lock, then the frame allocator's).

mod anon;
pub mod futex;

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::address_space::PAGE_SIZE;
use radix_abi::capability::Rights as AbiRights;
use radix_abi::errors::Kernel;
use radix_cap::ObjectId;
use radix_hal::{MapPageError, UserAccess, UserMode};
use radix_ipc::range::user_pages;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::Mutex;

pub use anon::{Anon, Fault, FileSource, Placement};

use super::ring::RingRef;
use super::{Result, error};
use crate::Arch;
use crate::caps::Store;
use crate::domains::DomainRef;

/// An address space, shared.
pub type SpaceRef = Arc<SpaceObject, Kmalloc>;
/// A frame, shared.
pub type FrameRef = Arc<FrameObject, Kmalloc>;

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

/// What a region maps.
#[derive(Clone)]
pub enum Backing {
    /// Pages of a frame.
    Frame(FrameRef),
    /// A ring's shared pages.
    Ring(RingRef),
    /// Anonymous memory, made page by page as it is touched (Phase 8).
    Anon(Anon),
}

impl Backing {
    /// The physical address of page `page` of what it maps: a frame's or a
    /// ring's, which are mapped whole. Anonymous memory has none of its
    /// own (`map` never takes it).
    fn phys(&self, page: u64) -> Option<u64> {
        match self {
            Backing::Frame(frame) => Some(frame.phys + page * PAGE_SIZE),
            Backing::Ring(ring) => Some(ring.phys(page as usize)),
            Backing::Anon(_) => None,
        }
    }

    fn same(&self, other: &Backing) -> bool {
        match (self, other) {
            (Backing::Frame(a), Backing::Frame(b)) => Arc::ptr_eq(a, b),
            (Backing::Ring(a), Backing::Ring(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// A run of mapped pages.
struct Region {
    /// The first page's address.
    start: u64,
    /// How many pages.
    pages: u64,
    /// What they map, from page `offset` of it (for anonymous memory, of
    /// the file it is filled from).
    backing: Backing,
    offset: u64,
}

impl Region {
    fn end(&self) -> u64 {
        self.start + self.pages * PAGE_SIZE
    }
}

/// The index of the first of `regions` that ends after `at`. The list is
/// sorted by address and its regions are disjoint, so a range from `at`
/// can only meet regions from this one on: a binary search, which keeps
/// a fault or an `mmap` cheap in a program with many regions.
fn first_after(regions: &[Region], at: u64) -> usize {
    regions.partition_point(|r| r.end() <= at)
}

/// Whether any of `regions` meets the range from `start` to `end`.
fn overlaps(regions: &[Region], start: u64, end: u64) -> bool {
    regions.get(first_after(regions, start)).is_some_and(|r| r.start < end)
}

/// Puts `region`, which meets none of them, in its place among
/// `regions`, which must have room for it.
fn insert(regions: &mut Vec<Region, Kmalloc>, region: Region) {
    let at = first_after(regions, region.start);
    // The caller reserved the room; the list stays sorted by address.
    let _ = regions.push_within_capacity(region);
    regions[at..].rotate_right(1);
}

/// An address space object.
pub struct SpaceObject {
    arch: <Arch as UserMode>::Space,
    regions: Mutex<Vec<Region, Kmalloc>>,
    /// The domain its anonymous memory is charged to: its process's.
    domain: Option<DomainRef>,
    /// How many pages of anonymous memory it maps, each charged once.
    anon_pages: AtomicU64,
    /// Where anonymous memory the kernel places goes: the highest free
    /// range below this, a random distance below ANONYMOUS_TOP.
    anon_top: u64,
}

/// Pages a step of zeroing a new frame clears: 256 KiB, a few tens of
/// microseconds, which is as long as deleting the frame may wait for it.
pub const ZERO_STEP: u64 = 64;

/// A frame object: 2^order pages of RAM a memory object was retyped into.
pub struct FrameObject {
    phys: u64,
    pages: u64,
    /// Set when its last capability is gone; nothing maps it after.
    closed: AtomicBool,
    /// How many of its pages, from the first, are zero: all of them before
    /// anything maps it.
    zeroed: AtomicU64,
    /// Held while a step of zeroing runs, so that closing waits for the
    /// step under way and no step starts after it.
    zeroing: Mutex<()>,
    /// The address spaces that map it, or did.
    spaces: Mutex<Vec<Weak<SpaceObject, Kmalloc>, Kmalloc>>,
}

impl SpaceObject {
    /// A new, empty address space, whose anonymous memory is charged to
    /// `domain`.
    pub fn new(domain: Option<DomainRef>) -> Option<SpaceRef> {
        let arch = Arch::new_space()?;
        let space = SpaceObject {
            arch,
            regions: Mutex::new(Vec::new_in(kmalloc())),
            domain,
            anon_pages: AtomicU64::new(0),
            anon_top: anon::random_top(),
        };
        Arc::try_new_in(space, kmalloc()).ok()
    }

    /// How many pages of anonymous memory it maps.
    pub fn anon_pages(&self) -> u64 {
        self.anon_pages.load(Ordering::Relaxed)
    }

    /// The architecture's side, which thread contexts load.
    pub fn arch(&self) -> &<Arch as UserMode>::Space {
        &self.arch
    }

    /// Maps `pages` pages of `backing`, from its page `offset`, at
    /// `start`, with `access`. The range must be free.
    pub fn map(
        &self,
        start: u64,
        pages: u64,
        backing: &Backing,
        offset: u64,
        access: UserAccess,
    ) -> Result<()> {
        let end = user_pages(start, pages).ok_or(error(Kernel::InvalidArgument))?;
        let mut regions = self.regions.lock();
        if overlaps(&regions, start, end) {
            return Err(error(Kernel::AlreadyMapped));
        }
        regions.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        for page in 0..pages {
            let virt = start + page * PAGE_SIZE;
            let phys = backing.phys(offset + page).ok_or(error(Kernel::InvalidArgument))?;
            if let Err(err) = Arch::map_page(&self.arch, virt, phys, access) {
                for done in 0..page {
                    Arch::unmap_page(&self.arch, start + done * PAGE_SIZE);
                }
                return Err(error(match err {
                    MapPageError::AlreadyMapped => Kernel::AlreadyMapped,
                    MapPageError::NoMemory => Kernel::NoMemory,
                }));
            }
        }
        insert(&mut regions, Region { start, pages, backing: backing.clone(), offset });
        Ok(())
    }

    /// Unmaps `pages` pages from `start`, every one of which must be
    /// mapped; regions the range cuts through keep their other parts.
    pub fn unmap(&self, start: u64, pages: u64) -> Result<()> {
        let end = user_pages(start, pages).ok_or(error(Kernel::InvalidArgument))?;
        let mut regions = self.regions.lock();
        // Every page must be covered: the regions are sorted and disjoint.
        let mut covered = start;
        for region in &regions[first_after(&regions, start)..] {
            if covered >= end || region.start > covered {
                break;
            }
            covered = region.end();
        }
        if covered < end {
            return Err(error(Kernel::NotMapped));
        }
        self.cut(&mut regions, start, end)
    }

    /// Splits the region that `at` falls inside, if one does, in two at
    /// `at` (a page boundary).
    fn split_at(regions: &mut Vec<Region, Kmalloc>, at: u64) -> Result<()> {
        let index = first_after(regions, at);
        if regions.get(index).is_none_or(|r| r.start >= at) {
            return Ok(());
        }
        regions.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        let region = &mut regions[index];
        let before = (at - region.start) / PAGE_SIZE;
        let tail = Region {
            start: at,
            pages: region.pages - before,
            backing: region.backing.clone(),
            offset: region.offset + before,
        };
        region.pages = before;
        // Room was reserved above; the list stays sorted.
        let _ = regions.push_within_capacity(tail);
        let last = regions.len() - 1;
        regions[index + 1..=last].rotate_right(1);
        Ok(())
    }

    /// Unmaps whatever is mapped from `start` to `end`, with the regions
    /// locked: the regions inside go, those the range cuts through are
    /// split first.
    fn cut(&self, regions: &mut Vec<Region, Kmalloc>, start: u64, end: u64) -> Result<()> {
        Self::split_at(regions, start)?;
        Self::split_at(regions, end)?;
        // After the splits, the regions the range meets lie wholly inside.
        let first = first_after(regions, start);
        let last = first + regions[first..].partition_point(|r| r.start < end);
        for region in &regions[first..last] {
            self.clear_region(region);
        }
        regions[first..].rotate_left(last - first);
        regions.truncate(regions.len() - (last - first));
        Ok(())
    }

    /// Takes away a region's pages: a frame's or ring's are unmapped one by
    /// one, anonymous memory's given back.
    fn clear_region(&self, region: &Region) {
        match region.backing {
            Backing::Anon(_) => self.release_range(region.start, region.end()),
            Backing::Frame(_) | Backing::Ring(_) => {
                for page in 0..region.pages {
                    Arch::unmap_page(&self.arch, region.start + page * PAGE_SIZE);
                }
            }
        }
    }

    /// Unmaps every region that maps `backing`.
    pub fn unmap_backing(&self, backing: &Backing) {
        let mut regions = self.regions.lock();
        regions.retain(|region| {
            if !region.backing.same(backing) {
                return true;
            }
            for page in 0..region.pages {
                Arch::unmap_page(&self.arch, region.start + page * PAGE_SIZE);
            }
            false
        });
    }

    /// Whether `address` is mapped, and with which rights.
    pub fn query(&self, address: u64) -> (bool, AbiRights) {
        let page = address & !(PAGE_SIZE - 1);
        match Arch::query_page(&self.arch, page) {
            None => (false, AbiRights(0)),
            Some((_, access)) => (true, rights_of(access)),
        }
    }
}

impl Drop for SpaceObject {
    fn drop(&mut self) {
        // Unmap first, so nothing the regions held stays mapped while it
        // goes; the tables themselves go with the last handle.
        let regions = core::mem::replace(self.regions.get_mut(), Vec::new_in(kmalloc()));
        for region in regions.iter() {
            self.clear_region(region);
        }
    }
}

impl FrameObject {
    /// A frame for the `pages` pages from `phys`, which are not zeroed yet:
    /// [`FrameObject::zero`] does that, and nothing maps the frame before.
    pub fn new(phys: u64, pages: u64) -> Option<FrameRef> {
        let frame = FrameObject {
            phys,
            pages,
            closed: AtomicBool::new(false),
            zeroed: AtomicU64::new(0),
            zeroing: Mutex::new(()),
            spaces: Mutex::new(Vec::new_in(kmalloc())),
        };
        Arc::try_new_in(frame, kmalloc()).ok()
    }

    /// Zeroes the pages not zeroed yet, a step of [`ZERO_STEP`] pages at a
    /// time, unless the frame closes first. Returns whether every page is
    /// zero. Never called with the capability store's lock held: closing
    /// holds it and waits for the step under way.
    pub fn zero(&self) -> bool {
        loop {
            if self.zeroed.load(Ordering::Acquire) == self.pages {
                return true;
            }
            let _step = self.zeroing.lock();
            if self.closed.load(Ordering::Acquire) {
                return false;
            }
            // Another thread may have taken a step while this one waited.
            let done = self.zeroed.load(Ordering::Relaxed);
            let pages = (self.pages - done).min(ZERO_STEP);
            Arch::zero_frames(self.phys + done * PAGE_SIZE, pages);
            self.zeroed.store(done + pages, Ordering::Release);
        }
    }

    /// Copies `bytes` into the frame from byte `offset`, zeroing it first:
    /// for the kernel's loader, which fills frames before mapping them.
    pub fn write(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        let end = offset.checked_add(bytes.len() as u64).ok_or(error(Kernel::OutOfRange))?;
        if end > self.pages * PAGE_SIZE {
            return Err(error(Kernel::OutOfRange));
        }
        if !self.zero() {
            return Err(error(Kernel::EmptySlot));
        }
        Arch::write_frames(self.phys + offset, bytes);
        Ok(())
    }

    /// How many pages it has.
    pub fn pages(&self) -> u64 {
        self.pages
    }

    /// Maps `pages` of its pages from page `offset` into `space` at
    /// `start`.
    pub fn map_into(
        frame: &FrameRef,
        space: &SpaceRef,
        start: u64,
        offset: u64,
        pages: u64,
        access: UserAccess,
    ) -> Result<()> {
        if offset.checked_add(pages).is_none_or(|end| end > frame.pages) {
            return Err(error(Kernel::OutOfRange));
        }
        // Its old contents are never mapped.
        if !frame.zero() {
            return Err(error(Kernel::EmptySlot));
        }
        // Held across the mapping, so that `close` waits for it and then
        // finds the space in the list.
        let mut spaces = frame.spaces.lock();
        if frame.closed.load(Ordering::Acquire) {
            return Err(error(Kernel::EmptySlot));
        }
        if !spaces.iter().any(|weak| core::ptr::eq(weak.as_ptr(), Arc::as_ptr(space))) {
            spaces.retain(|weak| weak.strong_count() > 0);
            spaces.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
            let _ = spaces.push_within_capacity(Arc::downgrade(space));
        }
        space.map(start, pages, &Backing::Frame(frame.clone()), offset, access)
    }

    /// Its last capability is gone: stops zeroing it, then unmaps it from
    /// every address space. Called with the capability store's lock held,
    /// before its memory can go back to be retyped again.
    pub fn close(frame: &FrameRef) {
        // A step of zeroing under way ends first, and none starts after:
        // nothing writes to the memory once it has gone back.
        {
            let _step = frame.zeroing.lock();
            frame.closed.store(true, Ordering::Release);
        }
        let spaces = {
            // A mapping under way when the frame closed has added its
            // space to the list by the time this lock is taken.
            let mut spaces = frame.spaces.lock();
            core::mem::replace(&mut *spaces, Vec::new_in(kmalloc()))
        };
        let backing = Backing::Frame(frame.clone());
        for space in spaces.iter().filter_map(Weak::upgrade) {
            space.unmap_backing(&backing);
        }
    }
}

/// The access a set of rights asks for: read is needed, write and execute
/// never together.
pub fn access_of(rights: AbiRights) -> Option<UserAccess> {
    let (read, write, execute) = (
        rights.contains(AbiRights::READ),
        rights.contains(AbiRights::WRITE),
        rights.contains(AbiRights::EXECUTE),
    );
    if !read
        || rights.bits() & !(AbiRights::READ.0 | AbiRights::WRITE.0 | AbiRights::EXECUTE.0) != 0
    {
        return None;
    }
    match (write, execute) {
        (false, false) => Some(UserAccess::Read),
        (true, false) => Some(UserAccess::ReadWrite),
        (false, true) => Some(UserAccess::ReadExecute),
        (true, true) => None,
    }
}

/// The rights a mapping's access amounts to.
pub fn rights_of(access: UserAccess) -> AbiRights {
    match access {
        UserAccess::Read => AbiRights::READ,
        UserAccess::ReadWrite => AbiRights::READ.union(AbiRights::WRITE),
        UserAccess::ReadExecute => AbiRights::READ.union(AbiRights::EXECUTE),
    }
}

/// The store's rights for the ABI's.
pub fn cap_rights(rights: AbiRights) -> radix_cap::Rights {
    radix_cap::Rights::from_bits_truncate(rights.bits() as u16)
}

/// Makes the frame object for a frame `store` just retyped: `id`, and
/// its extent's first page frame and pages. Called with the store locked,
/// so the object is recorded before any capability to it can be used; it
/// is zeroed later ([`FrameObject::zero`]), and returned for that.
pub fn frame_made(store: &Store, id: ObjectId, base: u64, pages: u64) -> Result<FrameRef> {
    let frame = FrameObject::new(base * PAGE_SIZE, pages).ok_or(error(Kernel::NoMemory))?;
    super::objects::insert(store, id, super::objects::Object::Frame(frame.clone()))
        .map_err(|_| error(Kernel::NoMemory))?;
    Ok(frame)
}
