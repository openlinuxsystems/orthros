// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Address spaces and frames, and mapping one into the other.
//!
//! An address space keeps a list of regions: runs of pages, each mapping
//! part of one frame or ring. A frame remembers which address spaces map
//! it, so that when its last capability goes (and its memory returns to
//! the memory object it came from) it is unmapped from every one of them
//! first: a program never keeps a mapping of memory the kernel has handed
//! to someone else. Frames are zeroed when they are made, so a program
//! never sees what the memory held before.
//!
//! Lock order: a frame's state, then an address space's regions (then the
//! architecture's page-table lock).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_abi::address_space::PAGE_SIZE;
use radix_abi::capability::Rights as AbiRights;
use radix_abi::errors::Kernel;
use radix_cap::ObjectId;
use radix_hal::{MapPageError, UserAccess, UserMode};
use radix_ipc::range::user_pages;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::Mutex;

use super::ring::RingRef;
use super::{Result, error};
use crate::Arch;
use crate::caps::Store;

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
}

impl Backing {
    /// The physical address of page `page` of what it maps.
    fn phys(&self, page: u64) -> u64 {
        match self {
            Backing::Frame(frame) => frame.phys + page * PAGE_SIZE,
            Backing::Ring(ring) => ring.phys(page as usize),
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
    /// What they map, from page `offset` of it.
    backing: Backing,
    offset: u64,
}

impl Region {
    fn end(&self) -> u64 {
        self.start + self.pages * PAGE_SIZE
    }
}

/// An address space object.
pub struct SpaceObject {
    arch: <Arch as UserMode>::Space,
    regions: Mutex<Vec<Region, Kmalloc>>,
}

/// A frame object: 2^order pages of RAM a memory object was retyped into.
pub struct FrameObject {
    phys: u64,
    pages: u64,
    /// Set when its last capability is gone; nothing maps it after.
    closed: AtomicBool,
    /// The address spaces that map it, or did.
    spaces: Mutex<Vec<Weak<SpaceObject, Kmalloc>, Kmalloc>>,
}

impl SpaceObject {
    /// A new, empty address space.
    pub fn new() -> Option<SpaceRef> {
        let arch = Arch::new_space()?;
        let space = SpaceObject { arch, regions: Mutex::new(Vec::new_in(kmalloc())) };
        Arc::try_new_in(space, kmalloc()).ok()
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
        if regions.iter().any(|r| r.start < end && start < r.end()) {
            return Err(error(Kernel::AlreadyMapped));
        }
        regions.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        for page in 0..pages {
            let virt = start + page * PAGE_SIZE;
            if let Err(err) = Arch::map_page(&self.arch, virt, backing.phys(offset + page), access)
            {
                for done in 0..page {
                    Arch::unmap_page(&self.arch, start + done * PAGE_SIZE);
                }
                return Err(error(match err {
                    MapPageError::AlreadyMapped => Kernel::AlreadyMapped,
                    MapPageError::NoMemory => Kernel::NoMemory,
                }));
            }
        }
        let region = Region { start, pages, backing: backing.clone(), offset };
        let at = regions.iter().position(|r| r.start > start).unwrap_or(regions.len());
        // Room was reserved above; the list stays sorted by address.
        let _ = regions.push_within_capacity(region);
        regions[at..].rotate_right(1);
        Ok(())
    }

    /// Unmaps `pages` pages from `start`, every one of which must be
    /// mapped; regions the range cuts through keep their other parts.
    pub fn unmap(&self, start: u64, pages: u64) -> Result<()> {
        let end = user_pages(start, pages).ok_or(error(Kernel::InvalidArgument))?;
        let mut regions = self.regions.lock();
        // Every page must be covered: the regions are sorted and disjoint.
        let mut covered = start;
        for region in regions.iter().filter(|r| r.start < end && start < r.end()) {
            if region.start > covered {
                break;
            }
            covered = covered.max(region.end());
        }
        if covered < end {
            return Err(error(Kernel::NotMapped));
        }
        // A region cut in the middle becomes two.
        regions.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        let mut index = 0;
        while index < regions.len() {
            let region = &regions[index];
            if region.end() <= start || region.start >= end {
                index += 1;
                continue;
            }
            let (cut_start, cut_end) = (region.start.max(start), region.end().min(end));
            for virt in (cut_start..cut_end).step_by(PAGE_SIZE as usize) {
                Arch::unmap_page(&self.arch, virt);
            }
            let before = (cut_start - region.start) / PAGE_SIZE;
            let after = (region.end() - cut_end) / PAGE_SIZE;
            let tail = (after > 0).then(|| Region {
                start: cut_end,
                pages: after,
                backing: region.backing.clone(),
                offset: region.offset + (cut_end - region.start) / PAGE_SIZE,
            });
            if before > 0 {
                regions[index].pages = before;
                index += 1;
            } else {
                regions.remove(index);
            }
            if let Some(tail) = tail {
                // Room was reserved above; only one region can be cut in
                // the middle.
                let _ = regions.push_within_capacity(tail);
                let last = regions.len() - 1;
                regions[index..=last].rotate_right(1);
                index += 1;
            }
        }
        Ok(())
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
        let regions = self.regions.get_mut();
        for region in regions.iter() {
            for page in 0..region.pages {
                Arch::unmap_page(&self.arch, region.start + page * PAGE_SIZE);
            }
        }
        regions.clear();
    }
}

impl FrameObject {
    /// A frame for the `pages` pages from `phys`; zeroes them.
    pub fn new(phys: u64, pages: u64) -> Option<FrameRef> {
        Arch::zero_frames(phys, pages);
        let frame = FrameObject {
            phys,
            pages,
            closed: AtomicBool::new(false),
            spaces: Mutex::new(Vec::new_in(kmalloc())),
        };
        Arc::try_new_in(frame, kmalloc()).ok()
    }

    /// Its first page's physical address.
    pub fn phys(&self) -> u64 {
        self.phys
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

    /// Its last capability is gone: unmaps it from every address space.
    pub fn close(frame: &FrameRef) {
        let spaces = {
            let mut spaces = frame.spaces.lock();
            frame.closed.store(true, Ordering::Release);
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

/// Makes frame objects for the frames `store` just retyped: `ids` and
/// their extents (first page frame, pages). Called with the store locked,
/// so no capability to them is used before they are zeroed and recorded.
pub fn frames_made(store: &Store, ids: &[(ObjectId, u64, u64)]) -> Result<()> {
    for &(id, base, pages) in ids {
        let frame = FrameObject::new(base * PAGE_SIZE, pages).ok_or(error(Kernel::NoMemory))?;
        super::objects::insert(store, id, super::objects::Object::Frame(frame))
            .map_err(|_| error(Kernel::NoMemory))?;
    }
    Ok(())
}
