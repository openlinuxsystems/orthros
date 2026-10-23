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
//! writable again in place. A page of a shared region (`MAP_SHARED`) stays
//! writable in both, and both see every write. A page goes back to the
//! allocator when the last space that maps it lets go of it.
//!
//! Every page a space maps is charged to its domain once, shared or not,
//! so a fork charges the child's domain for all the parent's pages up
//! front, and a domain's limit bounds what its programs can touch.
//!
//! Entries for pages under `PROT_NONE` keep their page without access
//! (the architecture's held entries), so `mprotect` can take access away
//! and give it back without losing what the page holds.

use core::sync::atomic::Ordering;

use radix_abi::address_space::{ANONYMOUS_TOP, PAGE_SIZE, USER_START};
use radix_abi::errors::Kernel;
use radix_hal::{PageEdit, UserAccess, UserMode};
use radix_mm::{FRAMES, Flags, Owner};
use radix_vfs::{NodeRef, Out};

use super::{Backing, Region, SpaceObject, SpaceRef, first_after, insert, overlaps};
use crate::Arch;
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
    /// Whether a forked space shares its pages for good (`MAP_SHARED`)
    /// rather than each side getting a copy when it writes.
    pub shared: bool,
    /// The file its pages are filled from, if any.
    pub file: Option<FileSource>,
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
    /// The space's domain has no room for another page, or memory ran out.
    NoMemory,
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
        let mut released = 0;
        Arch::edit_pages(&self.arch, start, end, &mut |_, _, _| PageEdit::Clear, &mut |phys| {
            unshare(phys);
            released += 1;
        });
        self.uncharge(released);
    }

    /// The highest free range of `pages` pages below `top`, if one fits.
    fn free_range(regions: &[Region], pages: u64, top: u64) -> Option<u64> {
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
        let region = Region {
            start,
            pages: (end - start) / PAGE_SIZE,
            backing: Backing::Anon(anon),
            offset: 0,
        };
        insert(&mut regions, region);
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
            let shared = anon.shared;
            Arch::edit_pages(
                &self.arch,
                region.start,
                region.end(),
                &mut |_, phys, _| {
                    let sharers = FRAMES.info(phys).map_or(1, |info| info.share_count());
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

    /// Makes a new page for page `page` of `region`, filled as it says;
    /// returns its frame. The page is charged to the space.
    fn new_page(
        &self,
        region: &Region,
        anon: &Anon,
        page: u64,
    ) -> core::result::Result<u64, Fault> {
        self.charge(1).map_err(|_| Fault::NoMemory)?;
        let flags = Flags { zero: anon.file.is_none(), ..Flags::MAY_SLEEP };
        let Ok(phys) = FRAMES.alloc(0, flags, Owner::Anon) else {
            self.uncharge(1);
            return Err(Fault::NoMemory);
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
                    Ok(read) => filled = read as u64,
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
        let regions = self.regions.lock();
        let Some(region) = regions.get(first_after(&regions, page)).filter(|r| r.start <= page)
        else {
            return Fault::Refused;
        };
        let Backing::Anon(anon) = &region.backing else { return Fault::Refused };
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
                let sharers = FRAMES.info(phys).map_or(1, |info| info.share_count());
                // Only a fork of this space, which holds its regions' lock,
                // makes a page more shared: one space alone stays alone.
                if anon.shared || sharers <= 1 {
                    return match Arch::set_entry(&self.arch, page, phys, Some(access), true) {
                        Ok(()) => Fault::Resolved,
                        Err(_) => Fault::NoMemory,
                    };
                }
                // Copy on write: the copy is this space's, the original
                // stays with the others.
                let Ok(copy) = FRAMES.alloc(0, Flags::MAY_SLEEP, Owner::Anon) else {
                    return Fault::NoMemory;
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
                let sharers = FRAMES.info(phys).map_or(1, |info| info.share_count());
                let now = if anon.shared { access } else { private_access(access, sharers) };
                // Still shared, a write faults again, and copies.
                match Arch::set_entry(&self.arch, page, phys, Some(now), true) {
                    Ok(()) => Fault::Resolved,
                    Err(_) => Fault::NoMemory,
                }
            }
            None => {
                let phys = match self.new_page(region, anon, page) {
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
            Fault::Io => return Err(radix_abi::errors::File::Io.status(0)),
            Fault::NoMemory => return Err(error(Kernel::NoMemory)),
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

    /// Gives `child`, a new space no thread runs in, what this one maps:
    /// its anonymous memory shared (private pages read-only in both until
    /// written), its frames mapped as they are; rings are left out. The
    /// child's domain is charged for every page of anonymous memory.
    pub fn fork_into(&self, child: &SpaceRef) -> Result<()> {
        let mut frames = alloc::vec::Vec::new_in(super::kmalloc());
        {
            let regions = self.regions.lock();
            let mut copied = child.regions.lock_nested(1);
            copied.try_reserve(regions.len()).map_err(|_| error(Kernel::NoMemory))?;
            frames.try_reserve(regions.len()).map_err(|_| error(Kernel::NoMemory))?;
            // With the regions locked no page of this space is made or let
            // go of, so these are the pages the child gets.
            let total = self.anon_pages.load(Ordering::Relaxed);
            child.charge(total)?;
            let mut given = 0u64;
            for region in regions.iter() {
                let anon = match &region.backing {
                    Backing::Anon(anon) => Some(anon),
                    Backing::Frame(frame) => {
                        // Room was reserved above.
                        let _ = frames.push_within_capacity(frame.clone());
                        None
                    }
                    Backing::Ring(_) => continue,
                };
                // In the child's list before its pages are, so that the
                // child's end lets go of whatever it got.
                let _ = copied.push_within_capacity(Region {
                    start: region.start,
                    pages: region.pages,
                    backing: region.backing.clone(),
                    offset: region.offset,
                });
                let shared = anon.is_some_and(|anon| anon.shared);
                let mut last = None;
                let result = Arch::copy_pages(
                    &self.arch,
                    &child.arch,
                    region.start,
                    region.end(),
                    &mut |_, phys, access| {
                        if anon.is_none() {
                            return (PageEdit::Keep, Some(access));
                        }
                        if let Some(info) = FRAMES.info(phys) {
                            info.share();
                        }
                        last = Some(phys);
                        given += 1;
                        match access {
                            Some(UserAccess::ReadWrite) if !shared => (
                                PageEdit::Set(Some(UserAccess::Read)),
                                Some(Some(UserAccess::Read)),
                            ),
                            access => (PageEdit::Keep, Some(access)),
                        }
                    },
                );
                if result.is_err() {
                    // The entry the walk stopped at was counted, not given.
                    if let Some(phys) = last {
                        unshare(phys);
                        given -= 1;
                    }
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
