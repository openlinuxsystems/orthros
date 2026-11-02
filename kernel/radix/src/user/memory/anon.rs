// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Anonymous memory (Phase 8, radix-address-space(2)): what POSIX's `mmap`
//! makes.
//!
//! An anonymous region is a run of addresses with an access and nothing
//! behind it yet. A page is made the first time the program touches it
//! ([`SpaceObject::resolve`], from the fault handler or from a copy of the
//! kernel's that faulted): a page from the frame allocator, owned by
//! `Owner::Anon`, zeroed or filled from the file the region maps
//! ([`FileSource`]), and charged to the space's resource domain. The page
//! allocator's record of the page counts the spaces that map it (its
//! share count): one, until a fork.
//!
//! A fork ([`SpaceObject::fork_into`]) gives the child's space an entry
//! for every page the parent's has, and counts it shared. A page of a
//! private region is then mapped read-only in both, whatever the region
//! allows, so the first write to it faults: a page shared by more than one
//! space is copied for the writer, which maps its copy writable and lets
//! go of the original; a page no other space maps any more is made
//! writable again in place. A page goes back to the allocator when the
//! last space that maps it lets go of it.
//!
//! A shared mapping (`MAP_SHARED`) keeps its pages in a [`SharedPages`],
//! which the regions of every space that maps it hold: a fault in any of
//! them maps the mapping's page, made by whichever space touched it first,
//! so a page first touched after a fork is the same page on both sides.
//! The mapping counts as one more sharer of each of its pages and keeps
//! them until the last region that maps it goes; they stay writable
//! wherever the region allows it, and every space sees every write.
//!
//! Every page a space maps is charged to its domain once, shared or not,
//! so a fork charges the child's domain for all the parent's pages up
//! front, and a domain's limit bounds what its programs can touch.
//!
//! Entries for pages under `PROT_NONE` keep their page without access
//! (the architecture's held entries), so `mprotect` can take access away
//! and give it back without losing what the page holds.
//!
//! A page of a private region that only this space maps, lent to no
//! device, can be swapped out (ADR 0152, [`SpaceObject::swap_out`]): the
//! swap thread looks for pages no CPU used since its last look, writes
//! them to a swap area in runs, and has their entries hold their slots;
//! the next fault at one reads it back ([`SpaceObject::resolve`]). A
//! swapped-out page stays counted and charged, a fork shares its slot as
//! it shares a page, and unmapping it frees the slot.

use core::sync::atomic::Ordering;

use radix_abi::address_space::{ANONYMOUS_TOP, PAGE_SIZE, USER_START};
use radix_abi::errors::Kernel;
use radix_hal::{PageEdit, Scanned, UserAccess, UserMode};
use radix_mm::{FRAMES, Flags, Owner};
use radix_vfs::{NodeRef, Out};

use alloc::sync::Arc;
use alloc::vec::Vec;

use radix_mm::Kmalloc;
use radix_sync::Mutex;

use super::{Backing, Region, SpaceObject, SpaceRef, first_after, insert, overlaps};
use crate::Arch;
use crate::domains::DomainRef;
use crate::sched::wait;
use crate::swap;
use crate::user::{Result, error};

/// The most the place of anonymous memory moves below ANONYMOUS_TOP, at
/// random for each space: 1 TiB.
const ANON_WINDOW: u64 = 1 << 40;

/// Where a space's anonymous memory starts from: a random page boundary
/// within [`ANON_WINDOW`] below ANONYMOUS_TOP.
pub(super) fn random_top() -> u64 {
    ANONYMOUS_TOP - radix_random::below(ANON_WINDOW / PAGE_SIZE) * PAGE_SIZE
}

/// An anonymous region's mapping.
#[derive(Clone)]
pub struct Anon {
    /// What the program may do with its pages; none (`PROT_NONE`) keeps
    /// them out of reach.
    pub access: Option<UserAccess>,
    /// The pages of a shared mapping (`MAP_SHARED`), which every space
    /// that maps it uses, rather than each side of a fork getting a copy
    /// when it writes; `None` for a private one.
    pub shared: Option<SharedRef>,
    /// The file its pages are filled from, if any.
    pub file: Option<FileSource>,
}

/// How many pages a chunk of a [`SharedPages`] holds the frames of.
const CHUNK: u64 = 512;

/// A chunk's number, and the frames of its pages (0 for one not made).
type Chunk = (u64, Vec<u64, Kmalloc>);

/// A shared mapping's pages: page `n` is the one at the region's offset
/// plus `n`, wherever each space maps the region. It holds each page it
/// made as one of the page's sharers, so a page stays while the mapping
/// does, mapped in a space at the moment or not, and is charged meanwhile
/// to the domain of the space that made the mapping, as well as to each
/// space while it maps the page.
pub struct SharedPages {
    /// Its pages' frames in chunks of [`CHUNK`], each made with the first
    /// of its pages and kept in order of its number.
    chunks: Mutex<Vec<Chunk, Kmalloc>>,
    /// The domain its pages are charged to.
    domain: Option<DomainRef>,
}

pub type SharedRef = Arc<SharedPages, Kmalloc>;

impl SharedPages {
    /// A shared mapping's pages, none made yet, charged to `space`'s
    /// domain.
    pub fn new(space: &SpaceObject) -> Result<SharedRef> {
        let pages = SharedPages {
            chunks: Mutex::new(Vec::new_in(super::kmalloc())),
            domain: space.domain.clone(),
        };
        Arc::try_new_in(pages, super::kmalloc()).map_err(|_| error(Kernel::NoMemory))
    }

    /// The frame of page `index`, zeroed and charged if it is new, counted
    /// as shared once more for the space about to map it.
    fn get(&self, index: u64) -> core::result::Result<u64, Fault> {
        let mut chunks = self.chunks.lock();
        let number = index / CHUNK;
        let at = match chunks.binary_search_by_key(&number, |&(n, _)| n) {
            Ok(at) => at,
            Err(at) => {
                chunks.try_reserve(1).map_err(|_| Fault::NoMemory)?;
                let mut chunk = Vec::new_in(super::kmalloc());
                chunk.try_reserve_exact(CHUNK as usize).map_err(|_| Fault::NoMemory)?;
                // The room for both was reserved; the chunks stay in order.
                for _ in 0..CHUNK {
                    let _ = chunk.push_within_capacity(0);
                }
                let _ = chunks.push_within_capacity((number, chunk));
                chunks[at..].rotate_right(1);
                at
            }
        };
        let slot = &mut chunks[at].1[(index % CHUNK) as usize];
        if *slot != 0 {
            if let Some(info) = FRAMES.info(*slot) {
                info.share();
            }
            return Ok(*slot);
        }
        if let Some(domain) = &self.domain {
            domain.charge(1).map_err(|_| Fault::NoMemory)?;
        }
        let Ok(phys) = FRAMES.alloc(0, Flags { zero: true, ..Flags::MAY_SLEEP }, Owner::Anon)
        else {
            if let Some(domain) = &self.domain {
                domain.uncharge(1);
            }
            return Err(Fault::NoMemory);
        };
        // The mapping's share, and the space's about to map it.
        if let Some(info) = FRAMES.info(phys) {
            info.set_share_count(2);
        }
        *slot = phys;
        Ok(phys)
    }
}

impl Drop for SharedPages {
    fn drop(&mut self) {
        let mut made = 0;
        for (_, chunk) in self.chunks.get_mut().iter() {
            for &phys in chunk.iter().filter(|&&phys| phys != 0) {
                unshare(phys);
                made += 1;
            }
        }
        if let Some(domain) = &self.domain {
            domain.uncharge(made);
        }
    }
}

/// The file an anonymous region's pages are filled from when they are
/// made: page `n` of the source (the region's offset plus the page's
/// place in it) holds the file's bytes from `base + n * PAGE_SIZE`, as far
/// as `size` bytes from `base` reach, and zeros past that.
#[derive(Clone)]
pub struct FileSource {
    /// The file.
    pub node: NodeRef,
    /// Where the source's first page starts in the file, a page boundary.
    pub base: u64,
    /// How many bytes from `base` come from the file.
    pub size: u64,
}

/// Where `map_anonymous` puts what it maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// At this address if it is free; anywhere otherwise (0: anywhere).
    Hint(u64),
    /// At this address, replacing what is there (`MAP_FIXED`).
    Fixed(u64),
    /// At this address, if nothing is there (`MAP_FIXED_NOREPLACE`).
    FixedNoReplace(u64),
}

/// What became of a fault on an address of a space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The page is there now, with the access the fault needed: the
    /// access can be tried again.
    Resolved,
    /// The access is not allowed there, or nothing is mapped: a real
    /// fault (SIGSEGV).
    Refused,
    /// The page could not be filled from its file (SIGBUS).
    Io,
    /// The page's memory went bad, and it was taken out (SIGBUS, with
    /// `BUS_MCEERR_AR`).
    Poisoned,
    /// The space's domain has no room for another page, or memory ran out.
    NoMemory,
    /// Memory ran short with the space's regions locked, and the fault did
    /// not wait for any to be reclaimed: [`SpaceObject::resolve`] lets go
    /// of them, so that the swap thread can swap this space's pages out
    /// too, and tries again. `resolve` never returns it.
    Short,
}

/// The access a page of a private region gets while `sharers` spaces map
/// it: read-only while it is shared, so that a write copies it first.
fn private_access(access: UserAccess, sharers: u32) -> UserAccess {
    if sharers > 1 && access == UserAccess::ReadWrite { UserAccess::Read } else { access }
}

/// A page of RAM as a buffer to read a file into.
struct FrameOut {
    phys: u64,
}

impl Out for FrameOut {
    fn len(&self) -> usize {
        PAGE_SIZE as usize
    }

    fn put(&mut self, at: usize, bytes: &[u8]) -> radix_vfs::Result<()> {
        if at.checked_add(bytes.len()).is_none_or(|end| end > PAGE_SIZE as usize) {
            return Err(radix_vfs::Error::BadAddress);
        }
        Arch::write_frames(self.phys + at as u64, bytes);
        Ok(())
    }
}

/// Gives back a page of anonymous memory one space lets go of: the last
/// space to do so frees it.
fn unshare(phys: u64) {
    if let Some(info) = FRAMES.info(phys)
        && info.unshare() == 0
    {
        let _ = FRAMES.free(phys, 0, Owner::Anon);
    }
}

/// Lets go of a pin on a page of anonymous memory lent to a device, as
/// [`unshare`] lets go of a space's hold on it.
fn unpin(phys: u64) {
    if let Some(info) = FRAMES.info(phys)
        && info.unpin() == 0
    {
        let _ = FRAMES.free(phys, 0, Owner::Anon);
    }
}

/// The most pages a lending lists on the stack ([`SpaceObject::lend`]).
const SHORT_LENDING: usize = 16;

/// The bit of a space's count of lendings ([`SpaceObject::lend`]) that a
/// fork waiting for them sets.
const FORK_WAITS: u64 = 1 << 63;

/// What [`SpaceObject::pin_pages`] did.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pins {
    /// Pinned every page.
    All,
    /// A page is not there yet, or is read-only for a write into it: one a
    /// fault makes.
    NotReady,
    /// A page is not anonymous memory, or has as many pins as it holds.
    Refused,
}

/// As [`unshare`], for a page a space let go of that may be anonymous
/// memory or not (a frame's, a ring's, which their objects keep): whether
/// it was.
fn unshare_if_anon(phys: u64) -> bool {
    let Some(info) = FRAMES.info(phys).filter(|info| info.owner() == Owner::Anon) else {
        return false;
    };
    if info.unshare() == 0 {
        let _ = FRAMES.free(phys, 0, Owner::Anon);
    }
    true
}

impl SpaceObject {
    /// Charges `pages` more pages of anonymous memory to its domain.
    fn charge(&self, pages: u64) -> Result<()> {
        if let Some(domain) = &self.domain {
            domain.charge(pages).map_err(|_| error(Kernel::NoMemory))?;
        }
        self.anon_pages.fetch_add(pages, Ordering::Relaxed);
        Ok(())
    }

    /// Gives back the charge for `pages` pages it no longer maps.
    fn uncharge(&self, pages: u64) {
        if let Some(domain) = &self.domain {
            domain.uncharge(pages);
        }
        self.anon_pages.fetch_sub(pages, Ordering::Relaxed);
    }

    /// Lets go of every page of anonymous memory from `start` to `end`.
    pub(super) fn release_range(&self, start: u64, end: u64) {
        // Slots first, so that the walk below finds the lines it empties.
        let mut released = self.release_swapped(start, end);
        Arch::edit_pages(&self.arch, start, end, &mut |_, _, _| PageEdit::Clear, &mut |phys| {
            unshare(phys);
            released += 1;
        });
        self.uncharge(released);
    }

    /// Lets go of the swap slots of the pages from `start` to `end` that
    /// are swapped out: how many there were.
    fn release_swapped(&self, start: u64, end: u64) -> u64 {
        if self.swapped.load(Ordering::Relaxed) == 0 {
            return 0;
        }
        let mut released = 0;
        Arch::clear_swapped(&self.arch, start, end, &mut |slot| {
            crate::swap::free(slot);
            released += 1;
        });
        self.swapped.fetch_sub(released, Ordering::Relaxed);
        released
    }

    /// Unmaps everything from `start` to `end`, whatever region it is in,
    /// in one walk of the page tables: how an ending space lets go of all
    /// it maps. Pages of anonymous memory are given back and uncharged.
    pub(super) fn release_all(&self, start: u64, end: u64) {
        let mut released = self.release_swapped(start, end);
        Arch::edit_pages(&self.arch, start, end, &mut |_, _, _| PageEdit::Clear, &mut |phys| {
            released += u64::from(unshare_if_anon(phys));
        });
        self.uncharge(released);
    }

    /// The highest free range of `pages` pages below `top`, if one fits.
    pub(super) fn free_range(regions: &[Region], pages: u64, top: u64) -> Option<u64> {
        let size = pages.checked_mul(PAGE_SIZE)?;
        let mut ceiling = top;
        // The regions are sorted: look at the gaps from the top down.
        for region in regions.iter().rev() {
            if region.end() <= ceiling && ceiling - region.end() >= size {
                return Some(ceiling - size);
            }
            ceiling = ceiling.min(region.start);
        }
        let floor = USER_START + PAGE_SIZE;
        (ceiling >= floor + size).then(|| ceiling - size)
    }

    /// Maps `pages` pages of anonymous memory `anon` describes, placed as
    /// `placement` says, and returns where they went.
    pub fn map_anonymous(&self, pages: u64, anon: Anon, placement: Placement) -> Result<u64> {
        if pages == 0 {
            return Err(Kernel::InvalidArgument.status(1));
        }
        let mut regions = self.regions.lock();
        let range = |start: u64| radix_ipc::range::user_pages(start, pages).map(|end| (start, end));
        let (start, end) = match placement {
            Placement::Fixed(at) | Placement::FixedNoReplace(at) => {
                let (start, end) = range(at).ok_or(Kernel::InvalidArgument.status(0))?;
                if overlaps(&regions, start, end) {
                    if matches!(placement, Placement::FixedNoReplace(_)) {
                        return Err(error(Kernel::AlreadyMapped));
                    }
                    self.cut(&mut regions, start, end)?;
                }
                (start, end)
            }
            Placement::Hint(hint) => {
                let hinted = (hint != 0 && hint.is_multiple_of(PAGE_SIZE))
                    .then(|| range(hint))
                    .flatten()
                    .filter(|&(start, end)| !overlaps(&regions, start, end));
                match hinted {
                    Some(range) => range,
                    None => {
                        let start = Self::free_range(&regions, pages, self.anon_top)
                            .or_else(|| Self::free_range(&regions, pages, ANONYMOUS_TOP))
                            .ok_or(error(Kernel::NoMemory))?;
                        (start, start + pages * PAGE_SIZE)
                    }
                }
            }
        };
        regions.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        let shared = anon.shared.is_some();
        let region = Region {
            start,
            pages: (end - start) / PAGE_SIZE,
            backing: Backing::Anon(anon),
            offset: 0,
        };
        insert(&mut regions, region);
        if shared {
            self.may_share.store(true, Ordering::Release);
        }
        Ok(start)
    }

    /// Gives the anonymous memory from `start`, `pages` pages, `access`.
    /// Every page must be anonymous memory.
    pub fn protect(&self, start: u64, pages: u64, access: Option<UserAccess>) -> Result<()> {
        let end =
            radix_ipc::range::user_pages(start, pages).ok_or(Kernel::InvalidArgument.status(0))?;
        let mut regions = self.regions.lock();
        // All of it anonymous, with no gap.
        let mut covered = start;
        for region in &regions[first_after(&regions, start)..] {
            if covered >= end
                || region.start > covered
                || !matches!(region.backing, Backing::Anon(_))
            {
                break;
            }
            covered = region.end();
        }
        if covered < end {
            return Err(error(Kernel::NotMapped));
        }
        Self::split_at(&mut regions, start)?;
        Self::split_at(&mut regions, end)?;
        let first = first_after(&regions, start);
        for region in regions[first..].iter_mut().take_while(|r| r.start < end) {
            let Backing::Anon(anon) = &mut region.backing else { continue };
            anon.access = access;
            let shared = anon.shared.is_some();
            Arch::edit_pages(
                &self.arch,
                region.start,
                region.end(),
                &mut |_, phys, _| {
                    let sharers = FRAMES.info(phys).map_or(1, radix_mm::PageInfo::share_count);
                    PageEdit::Set(
                        access.map(|a| if shared { a } else { private_access(a, sharers) }),
                    )
                },
                &mut |_| {},
            );
        }
        Ok(())
    }

    /// Unmaps whatever is mapped from `start`, `pages` pages: anonymous
    /// memory and frames alike, passing over what holds nothing.
    pub fn discard(&self, start: u64, pages: u64) -> Result<()> {
        let end =
            radix_ipc::range::user_pages(start, pages).ok_or(Kernel::InvalidArgument.status(0))?;
        let mut regions = self.regions.lock();
        self.cut(&mut regions, start, end)
    }

    /// Maps page `page` of shared `region` with `access`: the mapping's
    /// page, made now if no space has touched it yet. The page is charged
    /// to the space.
    fn map_shared(
        &self,
        pages: &SharedPages,
        region: &Region,
        page: u64,
        access: UserAccess,
    ) -> Fault {
        if self.charge(1).is_err() {
            return Fault::NoMemory;
        }
        let phys = match pages.get(region.offset + (page - region.start) / PAGE_SIZE) {
            Ok(phys) => phys,
            Err(fault) => {
                self.uncharge(1);
                return fault;
            }
        };
        if Arch::set_entry(&self.arch, page, phys, Some(access), false).is_err() {
            // The mapping keeps its share.
            unshare(phys);
            self.uncharge(1);
            return Fault::NoMemory;
        }
        Fault::Resolved
    }

    /// Makes a new page for page `page` of `region`, filled as it says;
    /// returns its frame. The page is charged to the space.
    fn new_page(
        &self,
        region: &Region,
        anon: &Anon,
        page: u64,
        wait: bool,
    ) -> core::result::Result<u64, Fault> {
        self.charge(1).map_err(|_| Fault::NoMemory)?;
        let flags = Flags { zero: anon.file.is_none(), ..Flags::MAY_SLEEP };
        // A zeroed page of a region of more than a run, whose page before
        // is there, is likely one of many a buffer has touched in order:
        // it takes the next frame of this CPU's run of contiguous ones, so
        // that the buffer lies in long stretches of memory, which devices
        // and hypervisors move faster. Smaller regions, which a program's
        // start fills, keep taking the frames freed last.
        let in_order = anon.file.is_none()
            && region.pages >= u64::from(radix_mm::RUN_FRAMES)
            && page > region.start
            && Arch::query_entry(&self.arch, page - PAGE_SIZE).is_some();
        let phys = match fault_frame(flags, in_order, wait) {
            Ok(phys) => phys,
            Err(fault) => {
                self.uncharge(1);
                return Err(fault);
            }
        };
        if let Some(file) = &anon.file {
            let from = (region.offset + (page - region.start) / PAGE_SIZE) * PAGE_SIZE;
            let mut filled = 0;
            if from < file.size {
                let want = (file.size - from).min(PAGE_SIZE) as usize;
                let mut out = FrameOut { phys };
                match file
                    .node
                    .read(file.base + from, &mut BoundedOut { inner: &mut out, len: want })
                {
                    Ok(read) => {
                        filled = read as u64;
                        self.file_faults.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        let _ = FRAMES.free(phys, 0, Owner::Anon);
                        self.uncharge(1);
                        return Err(Fault::Io);
                    }
                }
            }
            zero_tail(phys, filled);
        }
        if let Some(info) = FRAMES.info(phys) {
            info.set_share_count(1);
        }
        Ok(phys)
    }

    /// Resolves a fault at `address` of this space, by a write or an
    /// instruction fetch or neither: makes the page if its region is
    /// anonymous memory that allows the access, or copies it if it is
    /// shared with another space and written to.
    pub fn resolve(&self, address: u64, write: bool, execute: bool) -> Fault {
        let page = address & !(PAGE_SIZE - 1);
        // With a swap area on, a fault that finds memory short does not
        // wait for it with the regions locked, where the swap thread would
        // pass this space over: it lets go of them, has the swap thread
        // make room, and tries again. The last try waits, as any
        // allocation does.
        let mut tries = if swap::on_any() { SHORT_TRIES } else { 0 };
        let fault = loop {
            match self.resolve_page(page, write, execute, tries == 0) {
                Fault::Short => {
                    tries -= 1;
                    if !swap::reclaim(SHORT_WAIT_NS) {
                        tries = 0;
                    }
                }
                fault => break fault,
            }
        };
        if fault == Fault::Resolved {
            self.faults.fetch_add(1, Ordering::Relaxed);
        }
        fault
    }

    /// [`resolve`](Self::resolve), at page `page`: [`Fault::Short`] if
    /// memory ran short and not `wait`.
    fn resolve_page(&self, page: u64, write: bool, execute: bool, wait: bool) -> Fault {
        let regions = self.regions.lock();
        let Some(region) = regions.get(first_after(&regions, page)).filter(|r| r.start <= page)
        else {
            return Fault::Refused;
        };
        let Backing::Anon(anon) = &region.backing else {
            // A frame's or ring's page is mapped from the start, so it
            // faults for want of access, or while a large page of it was
            // split (`cut`, which holds the lock taken above): then it is
            // mapped again by now, and the access is tried again.
            return match Arch::query_page(&self.arch, page) {
                Some((_, now))
                    if (!write || now == UserAccess::ReadWrite)
                        && (!execute || now == UserAccess::ReadExecute) =>
                {
                    Fault::Resolved
                }
                _ => Fault::Refused,
            };
        };
        let Some(access) = anon.access else { return Fault::Refused };
        if (write && access != UserAccess::ReadWrite)
            || (execute && access != UserAccess::ReadExecute)
        {
            return Fault::Refused;
        }
        match Arch::query_entry(&self.arch, page) {
            Some((phys, Some(now))) => {
                if !write || now == UserAccess::ReadWrite {
                    // Another thread made it meanwhile.
                    return Fault::Resolved;
                }
                let sharers = FRAMES.info(phys).map_or(1, radix_mm::PageInfo::share_count);
                // Only a fork of this space, which holds its regions' lock,
                // makes a page more shared: one space alone stays alone.
                if anon.shared.is_some() || sharers <= 1 {
                    return match Arch::set_entry(&self.arch, page, phys, Some(access), true) {
                        Ok(()) => Fault::Resolved,
                        Err(_) => Fault::NoMemory,
                    };
                }
                // Copy on write: the copy is this space's, the original
                // stays with the others.
                let copy = match fault_frame(Flags::MAY_SLEEP, false, wait) {
                    Ok(copy) => copy,
                    Err(fault) => return fault,
                };
                Arch::copy_frame(copy, phys);
                if let Some(info) = FRAMES.info(copy) {
                    info.set_share_count(1);
                }
                if Arch::set_entry(&self.arch, page, copy, Some(access), false).is_err() {
                    let _ = FRAMES.free(copy, 0, Owner::Anon);
                    return Fault::NoMemory;
                }
                unshare(phys);
                Fault::Resolved
            }
            Some((phys, None)) => {
                // Held without access, from before its region allowed it.
                let sharers = FRAMES.info(phys).map_or(1, radix_mm::PageInfo::share_count);
                let now =
                    if anon.shared.is_some() { access } else { private_access(access, sharers) };
                // Still shared, a write faults again, and copies.
                match Arch::set_entry(&self.arch, page, phys, Some(now), true) {
                    Ok(()) => Fault::Resolved,
                    Err(_) => Fault::NoMemory,
                }
            }
            None => {
                if let Some(pages) = &anon.shared {
                    return self.map_shared(pages, region, page, access);
                }
                if self.swapped.load(Ordering::Relaxed) > 0
                    && let Some(slot) = Arch::swapped(&self.arch, page)
                {
                    return self.swap_in(page, slot, Some(access), wait);
                }
                let phys = match self.new_page(region, anon, page, wait) {
                    Ok(phys) => phys,
                    Err(fault) => return fault,
                };
                if Arch::set_entry(&self.arch, page, phys, Some(access), false).is_err() {
                    unshare(phys);
                    self.uncharge(1);
                    return Fault::NoMemory;
                }
                Fault::Resolved
            }
        }
    }

    /// Makes the pages for `len` bytes from `address` ready for reading,
    /// or writing if `write`, as faults on them would, passing over those
    /// that are: how a copy that could not wait for a page, under a lock
    /// that making one can take, gets them before it tries again. `false`
    /// if one cannot be made.
    pub fn fault_in(&self, address: u64, len: usize, write: bool) -> bool {
        let Some(end) = address.checked_add(len as u64) else { return false };
        let mut page = address & !(PAGE_SIZE - 1);
        while page < end {
            let ready = match Arch::query_entry(&self.arch, page) {
                Some((_, Some(access))) => !write || access == UserAccess::ReadWrite,
                _ => false,
            };
            if !ready && self.resolve(page, write, false) != Fault::Resolved {
                return false;
            }
            page += PAGE_SIZE;
        }
        true
    }

    /// Lends the pages of `len` bytes from `address` to a device that
    /// reads them, or writes them if `write`: makes them as faults would,
    /// pins them ([`radix_mm::PageInfo::pin`]) and calls `with` with them
    /// (a physical address and length for each part of a page, in order),
    /// then lets go of them. A page the program unmaps meanwhile stays
    /// until then. A fork waits for every lending under way: it would make
    /// a page read-only in both spaces, and the first write to it would
    /// copy it, part-way through what the device writes. `None`, without
    /// calling `with`, if a page cannot be made or is not anonymous memory
    /// (a frame's, a ring's): the caller copies instead.
    pub fn lend<R>(
        &self,
        address: u64,
        len: usize,
        write: bool,
        with: impl FnOnce(&[(u64, u32)]) -> R,
    ) -> Option<R> {
        let end = address.checked_add(len as u64)?;
        if len == 0 {
            return None;
        }
        let pages = (end - (address & !(PAGE_SIZE - 1))).div_ceil(PAGE_SIZE) as usize;
        // A part for each page: on the stack for a short lending, as a
        // disk's 4 KiB and 64 KiB transfers are, so that it takes nothing
        // from the heap; on the heap for a longer one.
        let mut short = [(0, 0); SHORT_LENDING];
        let mut long = Vec::new_in(super::kmalloc());
        let frames = if pages <= SHORT_LENDING {
            &mut short[..pages]
        } else {
            long.try_reserve_exact(pages).ok()?;
            for _ in 0..pages {
                let _ = long.push_within_capacity((0, 0));
            }
            &mut long[..]
        };
        // Pages the program has touched are pinned in one walk of the page
        // tables; if one is not there yet, they are made as faults would
        // make them, with the regions unlocked, and pinned once more.
        match self.pin_pages(address, end, write, frames) {
            Pins::All => {}
            Pins::NotReady
                if self.fault_in(address, len, write)
                    && self.pin_pages(address, end, write, frames) == Pins::All => {}
            _ => return None,
        }
        let result = with(frames);
        for &(phys, _) in frames.iter() {
            unpin(phys & !(PAGE_SIZE - 1));
        }
        // The last lending to end wakes a fork that waits for it; with no
        // fork waiting, nothing is woken.
        if self.lending.fetch_sub(1, Ordering::AcqRel) == FORK_WAITS | 1 {
            wait::wake_all(self.lending_key());
        }
        Some(result)
    }

    /// Pins the pages of the bytes from `address` to `end`, as they are,
    /// into `frames`, a part for each page, and counts a lending, for
    /// [`SpaceObject::lend`]. A page that is not there, or is read-only
    /// for a `write`, or is not anonymous memory pins nothing.
    fn pin_pages(&self, address: u64, end: u64, write: bool, frames: &mut [(u64, u32)]) -> Pins {
        // With the regions locked no page of this space is made or let go
        // of, nor does a fork start.
        let _regions = self.regions.lock();
        // One walk of the page tables pins them in order; it stops at a
        // page that is not there (or is read-only for a `write`), or that
        // cannot be pinned.
        let (mut at, mut pinned) = (address, 0);
        let mut refused = false;
        Arch::frames(&self.arch, address, end, write, &mut |phys| {
            let Some(part) = frames.get_mut(pinned) else {
                refused = true;
                return false;
            };
            match FRAMES.info(phys) {
                Some(info) if info.owner() == Owner::Anon && info.pin() => {}
                _ => {
                    refused = true;
                    return false;
                }
            }
            let n = ((at & !(PAGE_SIZE - 1)) + PAGE_SIZE).min(end) - at;
            *part = (phys + at % PAGE_SIZE, n as u32);
            pinned += 1;
            at += n;
            at < end
        });
        if at < end {
            for &(phys, _) in &frames[..pinned] {
                unpin(phys & !(PAGE_SIZE - 1));
            }
            return if refused { Pins::Refused } else { Pins::NotReady };
        }
        self.lending.fetch_add(1, Ordering::AcqRel);
        Pins::All
    }

    /// What a fork waits on for lendings to finish.
    fn lending_key(&self) -> usize {
        core::ptr::from_ref(&self.lending) as usize
    }

    /// Makes sure the page at `address` of anonymous memory is there, as
    /// the program would find it, and returns its frame: for writing a new
    /// program's stack and relocations before any thread runs in the
    /// space. The page must not be shared.
    pub fn touch(&self, address: u64) -> Result<u64> {
        let page = address & !(PAGE_SIZE - 1);
        if let Some((phys, _)) = Arch::query_entry(&self.arch, page) {
            return Ok(phys);
        }
        match self.resolve(page, false, false) {
            Fault::Resolved => {}
            Fault::Io | Fault::Poisoned => return Err(radix_abi::errors::File::Io.status(0)),
            Fault::NoMemory | Fault::Short => return Err(error(Kernel::NoMemory)),
            Fault::Refused => return Err(error(Kernel::BadAddress)),
        }
        Arch::query_entry(&self.arch, page).map(|(phys, _)| phys).ok_or(error(Kernel::BadAddress))
    }

    /// Writes `bytes` at `address` of this space, which no thread runs in
    /// yet, making its pages as needed: how a new program's stack and
    /// relocations are written.
    pub fn write_new(&self, address: u64, bytes: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < bytes.len() {
            let at = address + done as u64;
            let in_page = (PAGE_SIZE - at % PAGE_SIZE) as usize;
            let n = in_page.min(bytes.len() - done);
            let phys = self.touch(at)?;
            Arch::write_frames(phys + at % PAGE_SIZE, &bytes[done..done + n]);
            done += n;
        }
        Ok(())
    }

    /// About how many pages [`fork_into`](Self::fork_into) takes for the
    /// child's tables: one for each 512 of this space's pages, and as
    /// many again for the levels above, the regions and the rest.
    pub fn fork_room(&self) -> u64 {
        self.anon_pages.load(Ordering::Relaxed) / 512 + 64
    }

    /// Gives `child`, a new space no thread runs in, what this one maps:
    /// its anonymous memory shared (private pages read-only in both until
    /// written), its frames mapped as they are; rings are left out. The
    /// child's domain is charged for every page of anonymous memory.
    pub fn fork_into(&self, child: &SpaceRef) -> Result<()> {
        let mut frames = alloc::vec::Vec::new_in(super::kmalloc());
        {
            let regions = self.regions.lock();
            // No lending starts with the regions locked; those under way
            // finish without them.
            // Marked first, so that the last to end wakes it.
            self.lending.fetch_or(FORK_WAITS, Ordering::AcqRel);
            wait::wait_until(self.lending_key(), "fork", || {
                self.lending.load(Ordering::Acquire) == FORK_WAITS
            });
            self.lending.fetch_and(!FORK_WAITS, Ordering::AcqRel);
            let mut copied = child.regions.lock_nested(1);
            copied.try_reserve(regions.len()).map_err(|_| error(Kernel::NoMemory))?;
            frames.try_reserve(regions.len()).map_err(|_| error(Kernel::NoMemory))?;
            // With the regions locked no page of this space is made or let
            // go of, so these are the pages the child gets.
            let total = self.anon_pages.load(Ordering::Relaxed);
            child.charge(total)?;
            // The child maps what this space does, shared memory included.
            child.may_share.store(self.may_share.load(Ordering::Relaxed), Ordering::Release);
            for region in regions.iter() {
                match &region.backing {
                    Backing::Anon(_) => {}
                    // Room was reserved above.
                    Backing::Frame(frame) => {
                        let _ = frames.push_within_capacity(frame.clone());
                    }
                    Backing::Ring(_) | Backing::Device(_) => continue,
                }
                // In the child's list before its pages are, so that the
                // child's end lets go of whatever it got.
                let _ = copied.push_within_capacity(Region {
                    start: region.start,
                    pages: region.pages,
                    backing: region.backing.clone(),
                    offset: region.offset,
                });
            }
            // One walk over all of them, which finds each entry's region as
            // it goes (both are in address order): a walk for each region
            // went down the tables from the top again for every one.
            let (mut at, mut given, mut last) = (0, 0u64, None);
            let (start, end) = match (regions.first(), regions.last()) {
                (Some(first), Some(last)) => (first.start, last.end()),
                _ => (0, 0),
            };
            let result =
                Arch::copy_pages(&self.arch, &child.arch, start, end, &mut |virt, phys, access| {
                    // Only an entry of anonymous memory is counted.
                    last = None;
                    while regions.get(at).is_some_and(|region| region.end() <= virt) {
                        at += 1;
                    }
                    let region = regions.get(at).filter(|region| region.start <= virt);
                    let anon = match region.map(|region| &region.backing) {
                        Some(Backing::Anon(anon)) => anon,
                        Some(Backing::Frame(_)) => return (PageEdit::Keep, Some(access)),
                        // Rings and cards are left out, as is whatever maps
                        // between the regions.
                        Some(Backing::Ring(_) | Backing::Device(_)) | None => {
                            return (PageEdit::Keep, None);
                        }
                    };
                    if let Some(info) = FRAMES.info(phys) {
                        info.share();
                    }
                    last = Some(phys);
                    given += 1;
                    match access {
                        Some(UserAccess::ReadWrite) if anon.shared.is_none() => {
                            (PageEdit::Set(Some(UserAccess::Read)), Some(Some(UserAccess::Read)))
                        }
                        access => (PageEdit::Keep, Some(access)),
                    }
                });
            if result.is_err() {
                // The entry the walk stopped at was counted, not given.
                if let Some(phys) = last {
                    unshare(phys);
                    given -= 1;
                }
                child.uncharge(total - given);
                return Err(error(Kernel::NoMemory));
            }
            if self.swapped.load(Ordering::Relaxed) > 0 {
                let swapped = self.fork_swapped(child, start, end);
                given += swapped.unwrap_or_else(|given| given);
                if swapped.is_err() {
                    child.uncharge(total - given);
                    return Err(error(Kernel::NoMemory));
                }
            }
            child.uncharge(total.saturating_sub(given));
        }
        // Each frame unmaps itself from the child too when it goes. One
        // that went meanwhile did not see the child: it is unmapped here.
        for frame in frames {
            let mut spaces = frame.spaces.lock();
            if frame.closed.load(Ordering::Acquire) {
                drop(spaces);
                child.unmap_backing(&Backing::Frame(frame));
                continue;
            }
            spaces.retain(|weak| weak.strong_count() > 0);
            if spaces.try_reserve(1).is_err() {
                drop(spaces);
                child.unmap_backing(&Backing::Frame(frame));
                return Err(error(Kernel::NoMemory));
            }
            let _ = spaces.push_within_capacity(alloc::sync::Arc::downgrade(child));
        }
        Ok(())
    }
}

/// The pages [`SpaceObject::swap_out`] looks at in one go: where each is,
/// and its frame or slot.
type Found = [(u64, u64); swap::BATCH];

impl SpaceObject {
    /// Gives `child` the pages from `start` to `end` that are swapped out,
    /// sharing their slots, with this space's regions locked: how many,
    /// or how many before memory ran out.
    #[inline(never)]
    fn fork_swapped(
        &self,
        child: &SpaceObject,
        start: u64,
        end: u64,
    ) -> core::result::Result<u64, u64> {
        let mut given = 0;
        let mut at = start;
        while at < end {
            let (found, n, next) = self.find(at, end, false, |scanned| match scanned {
                Scanned::Swapped(slot) => Some(slot),
                Scanned::Frame { .. } => None,
            });
            for &(virt, slot) in &found[..n] {
                if !swap::share(slot) {
                    return Err(given);
                }
                if Arch::set_swapped(&child.arch, virt, slot).is_err() {
                    swap::free(slot);
                    return Err(given);
                }
                child.swapped.fetch_add(1, Ordering::Relaxed);
                given += 1;
            }
            at = next;
        }
        Ok(given)
    }

    /// Takes the frame at `phys`, whose memory went bad (ADR 0168), out of
    /// the pages of this space's private anonymous memory that map it,
    /// which hold [`swap::POISONED`] instead and fault with
    /// [`Fault::Poisoned`] from then on: the first page of any region that
    /// mapped it, if one did. Other pages keep it (shared or file memory, or
    /// memory lent to a device), and meet it again on their next read.
    pub fn retire_frame(&self, phys: u64) -> Option<u64> {
        let regions = self.regions.lock();
        let mut first = None;
        for region in regions.iter() {
            let private = matches!(&region.backing, Backing::Anon(anon) if anon.shared.is_none());
            let mut at = region.start;
            while at < region.end() {
                let (found, n, next) =
                    self.find(at, region.end(), false, |scanned| match scanned {
                        Scanned::Frame { phys: frame, .. } if frame == phys => Some(frame),
                        _ => None,
                    });
                for &(virt, _) in &found[..n] {
                    first.get_or_insert(virt);
                    let anon = FRAMES
                        .info(phys)
                        .is_some_and(|info| info.owner() == Owner::Anon && info.pins() == 0);
                    if private && anon && Arch::retire_page(&self.arch, virt, phys, swap::POISONED)
                    {
                        unshare(phys);
                        self.swapped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                at = next;
            }
        }
        first
    }

    /// Up to [`swap::BATCH`] pages from `start` to `end` that `pick` takes,
    /// forgetting their use if `forget`: where each is and what `pick`
    /// gave, how many, and where to look on.
    fn find(
        &self,
        start: u64,
        end: u64,
        forget: bool,
        mut pick: impl FnMut(Scanned) -> Option<u64>,
    ) -> (Found, usize, u64) {
        let mut found = [(0, 0); swap::BATCH];
        let (mut n, mut next) = (0, end);
        Arch::scan(&self.arch, start, end, forget, &mut |virt, scanned| {
            if let Some(value) = pick(scanned) {
                found[n] = (virt, value);
                n += 1;
                if n == found.len() {
                    next = virt + PAGE_SIZE;
                    return false;
                }
            }
            true
        });
        (found, n, next)
    }

    /// Swaps out up to `want` pages of its private anonymous memory that
    /// it alone maps, lent to no device, and unused since the last look,
    /// going on from where the last call stopped: how many went. A space
    /// whose regions are locked is passed over, as its owner may be
    /// waiting for memory with them locked, and so is one no thread has
    /// run in yet. `None` once no swap area has room.
    pub(super) fn swap_out(&self, want: u64) -> Option<u64> {
        if !self.running.load(Ordering::Acquire) {
            return Some(0);
        }
        let Some(regions) = self.regions.try_lock() else { return Some(0) };
        let from = self.swap_cursor.load(Ordering::Relaxed);
        let mut out = 0;
        for region in &regions[first_after(&regions, from)..] {
            let Backing::Anon(anon) = &region.backing else { continue };
            let Some(access) = anon.access.filter(|_| anon.shared.is_none()) else { continue };
            let mut at = region.start.max(from);
            while at < region.end() && out < want {
                let (found, n, next) = self.find(at, region.end(), true, |scanned| match scanned {
                    Scanned::Frame { phys, used: false } => FRAMES
                        .info(phys)
                        .filter(|info| {
                            info.owner() == Owner::Anon
                                && info.share_count() == 1
                                && info.pins() == 0
                        })
                        .map(|_| phys),
                    _ => None,
                });
                let Some(went) = self.swap_batch(&found[..n], access) else {
                    self.swap_cursor.store(at, Ordering::Relaxed);
                    return None;
                };
                out += went;
                at = next;
            }
            if out >= want {
                self.swap_cursor.store(at, Ordering::Relaxed);
                return Some(out);
            }
        }
        // Round to the start for the next call.
        self.swap_cursor.store(0, Ordering::Relaxed);
        Some(out)
    }

    /// Swaps out `pages` (where each is, and its frame), which the scan
    /// found unused, of a region whose pages have `access`: how many went,
    /// or `None` if no swap area has room. A page used since, or whose run
    /// could not be written, stays.
    #[inline(never)]
    fn swap_batch(&self, pages: &[(u64, u64)], access: UserAccess) -> Option<u64> {
        if pages.is_empty() {
            return Some(0);
        }
        let (first, got) = swap::take(pages.len())?;
        let mut taken = [false; swap::BATCH];
        for (i, &(virt, phys)) in pages[..got].iter().enumerate() {
            taken[i] = Arch::swap_out(&self.arch, virt, phys, first + i as u64);
            if !taken[i] {
                swap::free(first + i as u64);
            }
        }
        let mut out = 0;
        let mut i = 0;
        while i < got {
            if !taken[i] {
                i += 1;
                continue;
            }
            let mut frames = [0u64; swap::BATCH];
            let mut j = i;
            while j < got && taken[j] {
                frames[j - i] = pages[j].1;
                j += 1;
            }
            let written = swap::write(first + i as u64, &frames[..j - i]).is_ok();
            for (k, &(virt, phys)) in pages.iter().enumerate().take(j).skip(i) {
                if written {
                    unshare(phys);
                    out += 1;
                } else {
                    // Back as it was: its table is there, with the regions
                    // locked, so nothing need be made.
                    let _ = Arch::set_entry(&self.arch, virt, phys, Some(access), false);
                    swap::free(first + k as u64);
                }
            }
            i = j;
        }
        self.swapped.fetch_add(out, Ordering::Relaxed);
        Some(out)
    }

    /// Reads the page at `page` back from `slot`, with `access` (`None`:
    /// held without access), with the regions locked: [`Fault::Short`] if
    /// memory is short and not `wait`.
    fn swap_in(&self, page: u64, slot: u64, access: Option<UserAccess>, wait: bool) -> Fault {
        if slot == swap::POISONED {
            return Fault::Poisoned;
        }
        let phys = match fault_frame(Flags::MAY_SLEEP, false, wait) {
            Ok(phys) => phys,
            Err(fault) => return fault,
        };
        if swap::read(slot, phys).is_err() {
            let _ = FRAMES.free(phys, 0, Owner::Anon);
            return Fault::Io;
        }
        if let Some(info) = FRAMES.info(phys) {
            info.set_share_count(1);
        }
        if Arch::set_entry(&self.arch, page, phys, access, false).is_err() {
            let _ = FRAMES.free(phys, 0, Owner::Anon);
            return Fault::NoMemory;
        }
        swap::free(slot);
        self.swapped.fetch_sub(1, Ordering::Relaxed);
        self.file_faults.fetch_add(1, Ordering::Relaxed);
        Fault::Resolved
    }

    /// Reads back every page swapped out to swap area `area`.
    pub(super) fn swap_in_area(&self, area: u64) -> core::result::Result<(), Fault> {
        if self.swapped.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        let regions = self.regions.lock();
        for region in regions.iter() {
            let Backing::Anon(anon) = &region.backing else { continue };
            let mut at = region.start;
            while at < region.end() {
                let (found, n, next) =
                    self.find(at, region.end(), false, |scanned| match scanned {
                        Scanned::Swapped(slot) if slot >> 32 == area => Some(slot),
                        _ => None,
                    });
                for &(virt, slot) in &found[..n] {
                    match self.swap_in(virt, slot, anon.access, true) {
                        Fault::Resolved => {}
                        fault => return Err(fault),
                    }
                }
                at = next;
            }
        }
        Ok(())
    }
}

/// How many times a fault lets go of its space's regions for the swap
/// thread to make room ([`SpaceObject::resolve`]), and how long it waits
/// each time.
const SHORT_TRIES: u32 = 16;
const SHORT_WAIT_NS: u64 = 200_000_000;

/// A frame of anonymous memory for a fault, with `flags`, from the CPU's
/// run if `sequential`: if not `wait`, taken without reclaiming, and
/// [`Fault::Short`] if none is free then.
fn fault_frame(flags: Flags, sequential: bool, wait: bool) -> core::result::Result<u64, Fault> {
    let frame = match (wait, sequential) {
        (false, _) => FRAMES.alloc_unreclaimed(flags, Owner::Anon, sequential),
        (true, true) => FRAMES.alloc_sequential(flags, Owner::Anon),
        (true, false) => FRAMES.alloc(0, flags, Owner::Anon),
    };
    frame.map_err(|_| if wait { Fault::NoMemory } else { Fault::Short })
}

/// Zeroes a page of RAM from byte `from` to its end.
fn zero_tail(phys: u64, from: u64) {
    let zeros = [0u8; 512];
    let mut at = from;
    while at < PAGE_SIZE {
        let n = (PAGE_SIZE - at).min(zeros.len() as u64);
        Arch::write_frames(phys + at, &zeros[..n as usize]);
        at += n;
    }
}

/// An [`Out`] that takes at most `len` bytes of another.
struct BoundedOut<'a> {
    inner: &'a mut dyn Out,
    len: usize,
}

impl Out for BoundedOut<'_> {
    fn len(&self) -> usize {
        self.len
    }

    fn put(&mut self, at: usize, bytes: &[u8]) -> radix_vfs::Result<()> {
        if at.checked_add(bytes.len()).is_none_or(|end| end > self.len) {
            return Err(radix_vfs::Error::BadAddress);
        }
        self.inner.put(at, bytes)
    }
}
