// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The slab layer: one cache of slabs per size class.
//!
//! A slab is a block of frames cut into objects of one class. Its
//! bookkeeping lives in the [`PageInfo`] record of its first frame, so a
//! slab has no header and the heap needs no memory of its own to track
//! slabs: `next` and `prev` link it into its cache's list of partly used
//! slabs, the tag byte names its class, and `aux` packs the index of its
//! first free object (low 16 bits) and the number of objects in use (high
//! 16 bits).
//!
//! Free objects form a list threaded through the objects themselves. A
//! free object's first word holds a marker and the index of the next free
//! object; its second word holds a poison pattern. Taking an object checks
//! both, so a write to freed memory or a corrupted list is caught when the
//! object is next handed out instead of spreading; the heap then reports
//! it and never uses the object or the slab again.
//!
//! The free-object pattern is the only memory the slab layer touches; the
//! three functions that do (`mark_free`, `pattern` and `format`) are
//! `unsafe` and say what their callers guarantee.

#![allow(unsafe_code)]

use core::fmt;

use crate::class::Class;
use crate::frames::Frames;
use crate::page::{NIL, Owner, PAGE_SHIFT, PageInfo, State};
use crate::raw;

/// Marker in the high half of a free object's first word.
pub const FREE_MARK: usize = 0x5241_4458 << 32;
/// Pattern in a free object's second word.
pub const POISON: usize = 0x6b6b_6b6b_6b6b_6b6b;
/// End of a slab's free list.
const END: u16 = u16::MAX;
/// Empty slabs a cache keeps instead of returning them to the frame
/// allocator.
const KEEP_EMPTY: u32 = 1;

/// What kind of damage the heap found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorruptionKind {
    /// A free object's link to the next one is not valid.
    FreeList,
    /// A free object was written to after it was freed.
    UseAfterFree,
    /// An object was freed twice.
    DoubleFree,
    /// The pointer freed is not an object of the size freed.
    BadPointer,
    /// A slab's count of objects in use went wrong.
    Bookkeeping,
}

/// Heap damage, found when an object was taken or freed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Corruption {
    /// What was wrong.
    pub kind: CorruptionKind,
    /// The object's address.
    pub address: usize,
    /// The object size involved.
    pub size: usize,
}

impl fmt::Display for Corruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            CorruptionKind::FreeList => "the free list of the slab holding",
            CorruptionKind::UseAfterFree => "a write after free to",
            CorruptionKind::DoubleFree => "a double free of",
            CorruptionKind::BadPointer => "a free of something that is not",
            CorruptionKind::Bookkeeping => "the object counts of the slab holding",
        };
        write!(f, "heap corruption: {what} the {}-byte object at {:#x}", self.size, self.address)
    }
}

/// Where slab memory is: the frame allocator for records, and the direct
/// map for the objects.
#[derive(Clone, Copy)]
pub(crate) struct Env<'a> {
    pub frames: &'a Frames,
    pub direct_map: usize,
}

impl Env<'_> {
    fn info(&self, pfn: u64) -> Option<&PageInfo> {
        self.frames.info(pfn << PAGE_SHIFT)
    }

    /// Virtual address of the slab at `pfn`.
    pub fn slab_addr(&self, pfn: u64) -> usize {
        self.direct_map + ((pfn as usize) << PAGE_SHIFT)
    }

    /// The first frame of the slab of `class` holding `addr`, if `addr` is
    /// an object boundary in one; and the object's index.
    pub fn locate(&self, class: Class, addr: usize) -> Option<(u64, &PageInfo, usize)> {
        let phys = addr.checked_sub(self.direct_map)? as u64;
        let pfn = (phys >> PAGE_SHIFT) & !((1u64 << class.slab_order()) - 1);
        let info = self.info(pfn)?;
        let ours = info.state() == State::Allocated
            && info.owner() == Owner::Slab
            && usize::from(info.tag()) == class.index()
            && info.order() == class.slab_order();
        let index = class.object_at(addr - self.slab_addr(pfn))?;
        ours.then_some((pfn, info, index))
    }
}

fn unpack(aux: u32) -> (u16, u16) {
    (aux as u16, (aux >> 16) as u16)
}

fn pack(free: u16, used: u16) -> u32 {
    u32::from(free) | u32::from(used) << 16
}

/// Writes the free-object pattern into the object at `addr`, linking it to
/// `next`.
///
/// # Safety
///
/// `addr` must be an object of a slab the heap owns, mapped by the direct
/// map, and nobody may use the object: it is being freed or formatted.
pub(crate) unsafe fn mark_free(addr: usize, next: u16) {
    // SAFETY: objects are at least 16 bytes and 16-byte aligned, and the
    // caller guarantees the object is mapped and unused.
    unsafe {
        raw::write_word(addr, FREE_MARK | usize::from(next));
        raw::write_word(addr + 8, POISON);
    }
}

/// The first two words of the object at `addr`.
///
/// # Safety
///
/// `addr` must be an object of a slab the heap owns, mapped by the direct
/// map, that is free or being freed by its owner.
pub(crate) unsafe fn pattern(addr: usize) -> (usize, usize) {
    // SAFETY: as for `mark_free`; free objects are written only by the
    // heap, under the lock or magazine that holds them.
    unsafe { (raw::read_word(addr), raw::read_word(addr + 8)) }
}

/// Whether the object at `addr` already carries the free-object pattern.
///
/// # Safety
///
/// As for [`pattern`].
pub(crate) unsafe fn looks_free(addr: usize) -> bool {
    // SAFETY: the caller's guarantee.
    let (first, second) = unsafe { pattern(addr) };
    first & !0xffff_ffff == FREE_MARK && second == POISON
}

/// Checks the free-object pattern of the object at `addr` and returns its
/// link.
///
/// # Safety
///
/// `addr` must be a free object of a slab of `class` that the heap owns.
pub(crate) unsafe fn check_free(class: Class, addr: usize) -> Result<u16, Corruption> {
    // SAFETY: the caller's guarantee.
    let (first, second) = unsafe { pattern(addr) };
    let kind = if second != POISON {
        CorruptionKind::UseAfterFree
    } else if first & !0xffff_ffff != FREE_MARK
        || (first as u16 != END && usize::from(first as u16) >= class.objects())
    {
        CorruptionKind::FreeList
    } else {
        return Ok(first as u16);
    };
    Err(Corruption { kind, address: addr, size: class.size() })
}

/// Lays out a new slab's free list through all its objects.
///
/// # Safety
///
/// The slab at `pfn` must be a block of `class.slab_order()` frames just
/// allocated for the heap and mapped by the direct map.
pub(crate) unsafe fn format(env: Env<'_>, class: Class, pfn: u64) {
    let base = env.slab_addr(pfn);
    let objects = class.objects();
    for index in 0..objects {
        let next = if index + 1 == objects { END } else { (index + 1) as u16 };
        // SAFETY: the object lies inside the new slab, which the caller
        // owns.
        unsafe { mark_free(base + index * class.size(), next) };
    }
}

/// The slabs of one size class. Lives behind the class's lock.
pub(crate) struct SlabCache {
    /// First slab with free objects and objects in use.
    partial: u32,
    /// Slabs with no objects in use, linked through `next`.
    empty: u32,
    empty_count: u32,
    /// Slabs the cache holds.
    pub slabs: u64,
    /// Objects handed out (in use, or cached in magazines).
    pub in_use: u64,
    /// Slabs set aside because their free list was damaged.
    pub quarantined: u64,
}

impl SlabCache {
    pub const fn new() -> SlabCache {
        SlabCache { partial: NIL, empty: NIL, empty_count: 0, slabs: 0, in_use: 0, quarantined: 0 }
    }

    /// Adds a freshly formatted slab.
    pub fn add(&mut self, env: Env<'_>, pfn: u64) {
        if let Some(info) = env.info(pfn) {
            info.set_aux(pack(0, 0));
            self.slabs += 1;
            self.push_empty(env, pfn, info);
        }
    }

    /// Takes a free object, from a partly used slab if there is one.
    /// `Ok(None)` means the cache needs a new slab.
    pub fn take(&mut self, env: Env<'_>, class: Class) -> Result<Option<usize>, Corruption> {
        let pfn = if self.partial != NIL {
            u64::from(self.partial)
        } else if self.empty != NIL {
            let pfn = u64::from(self.empty);
            let Some(info) = env.info(pfn) else { return Ok(None) };
            self.empty = info.next();
            self.empty_count -= 1;
            self.push_partial(env, pfn, info);
            pfn
        } else {
            return Ok(None);
        };
        let Some(info) = env.info(pfn) else { return Ok(None) };
        let (free, used) = unpack(info.aux());
        let addr = env.slab_addr(pfn) + usize::from(free) * class.size();
        let next = if free == END || usize::from(free) >= class.objects() {
            Err(Corruption { kind: CorruptionKind::Bookkeeping, address: addr, size: class.size() })
        } else {
            // SAFETY: the slab is on this cache's partial list, so it is
            // the heap's and mapped, and `free` indexes its free object.
            unsafe { check_free(class, addr) }
        };
        let next = match next {
            Ok(next) => next,
            Err(corruption) => {
                // Never touch this slab again.
                self.unlink_partial(env, pfn, info);
                self.quarantined += 1;
                return Err(corruption);
            }
        };
        info.set_aux(pack(next, used + 1));
        if next == END {
            self.unlink_partial(env, pfn, info);
        }
        self.in_use += 1;
        Ok(Some(addr))
    }

    /// Returns the object at `addr`, which [`Env::locate`] found at `index`
    /// of the slab at `pfn` and which already carries the free pattern.
    /// Gives back a slab to return to the frame allocator, if one became
    /// surplus.
    pub fn give(
        &mut self,
        env: Env<'_>,
        class: Class,
        addr: usize,
        (pfn, info, index): (u64, &PageInfo, usize),
    ) -> Result<Option<u64>, Corruption> {
        let (free, used) = unpack(info.aux());
        if used == 0 {
            return Err(Corruption {
                kind: CorruptionKind::Bookkeeping,
                address: addr,
                size: class.size(),
            });
        }
        // SAFETY: `locate` found the object in a slab of this class, and
        // the heap's caller gave it up.
        unsafe { mark_free(addr, free) };
        info.set_aux(pack(index as u16, used - 1));
        self.in_use -= 1;
        if free == END {
            // It was full; now it has a free object.
            self.push_partial(env, pfn, info);
        }
        if used - 1 > 0 {
            return Ok(None);
        }
        self.unlink_partial(env, pfn, info);
        if self.empty_count < KEEP_EMPTY {
            self.push_empty(env, pfn, info);
            return Ok(None);
        }
        self.slabs -= 1;
        Ok(Some(pfn))
    }

    /// Takes an empty slab to return to the frame allocator.
    pub fn take_empty(&mut self, env: Env<'_>) -> Option<u64> {
        let pfn = u64::from(self.empty);
        let info = env.info(pfn).filter(|_| self.empty != NIL)?;
        self.empty = info.next();
        self.empty_count -= 1;
        self.slabs -= 1;
        Some(pfn)
    }

    fn push_empty(&mut self, _env: Env<'_>, pfn: u64, info: &PageInfo) {
        info.set_next(self.empty);
        info.set_prev(NIL);
        self.empty = pfn as u32;
        self.empty_count += 1;
    }

    fn push_partial(&mut self, env: Env<'_>, pfn: u64, info: &PageInfo) {
        info.set_prev(NIL);
        info.set_next(self.partial);
        if let Some(next) = env.info(u64::from(self.partial)).filter(|_| self.partial != NIL) {
            next.set_prev(pfn as u32);
        }
        self.partial = pfn as u32;
    }

    /// Unlinks the slab at `pfn` from the partial list, if it is on it.
    fn unlink_partial(&mut self, env: Env<'_>, pfn: u64, info: &PageInfo) {
        let (prev, next) = (info.prev(), info.next());
        if prev == NIL && self.partial != pfn as u32 {
            return;
        }
        match env.info(u64::from(prev)).filter(|_| prev != NIL) {
            Some(prev) => prev.set_next(next),
            None => self.partial = next,
        }
        if let Some(next) = env.info(u64::from(next)).filter(|_| next != NIL) {
            next.set_prev(prev);
        }
        info.set_next(NIL);
        info.set_prev(NIL);
    }
}
