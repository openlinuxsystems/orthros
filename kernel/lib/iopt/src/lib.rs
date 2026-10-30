// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! I/O page tables: what an IOMMU's devices reach, for the engines of
//! Intel VT-d (`radix-vtd`) and AMD-Vi (`radix-amdvi`).
//!
//! Both units translate a device's DMA through page tables of 512 entries
//! a table, 9 bits of address a level, in a format of their own
//! ([`Format`]). Radix maps each page at its own physical address: a
//! page's device address *is* its physical address, so drivers hand
//! devices physical addresses whether or not there is a unit.
//!
//! - [`Domain`] is a device's page table. It maps only the pages it is
//!   given, counts each page's mappings in bits of its entry that the unit
//!   ignores, so that two requests on one page do not take it from each
//!   other, and frees a table once nothing in it is mapped.
//! - [`map`], [`map_range`] and [`unmap`] change a domain and then have
//!   its unit forget what it may have cached of the change, through
//!   [`Invalidate`], before they return: invalidation is strict.
//!
//! The crate is a compartment: it has no `unsafe` code, and reaches the
//! tables through the [`Memory`] the kernel gives it. Nothing here sleeps
//! or takes a lock: the kernel serializes each unit and each domain.

#![cfg_attr(not(test), no_std)]
#![feature(vec_push_within_capacity)]
#![forbid(unsafe_code)]

extern crate alloc;

mod domain;
#[cfg(test)]
mod tests;

use core::sync::atomic::AtomicU64;

pub use domain::{Domain, Format, MapError, PAGE, blocks};

/// What the kernel lends an engine.
pub trait Memory {
    /// A zeroed 4 KiB page for a table, by its physical address; `None`
    /// when memory runs out.
    fn alloc_page(&self) -> Option<u64>;
    /// Gives back a page [`Memory::alloc_page`] gave.
    fn free_page(&self, phys: u64);
    /// The page at `phys`, as the CPU reads and writes its 512 entries.
    fn entries(&self, phys: u64) -> &[AtomicU64; 512];
    /// Writes the cache line of `entry` back to memory: for a unit whose
    /// table walks do not look in the CPU's caches.
    fn flush(&self, entry: &AtomicU64);
    /// Waits about `us` microseconds.
    fn delay_us(&self, us: u32);
}

impl<M: Memory + ?Sized> Memory for alloc::sync::Arc<M> {
    fn alloc_page(&self) -> Option<u64> {
        (**self).alloc_page()
    }

    fn free_page(&self, phys: u64) {
        (**self).free_page(phys);
    }

    fn entries(&self, phys: u64) -> &[AtomicU64; 512] {
        (**self).entries(phys)
    }

    fn flush(&self, entry: &AtomicU64) {
        (**self).flush(entry);
    }

    fn delay_us(&self, us: u32) {
        (**self).delay_us(us);
    }
}

/// How a unit forgets what it cached of a domain's tables.
pub trait Invalidate {
    /// Why it could not.
    type Error;

    /// Has the unit forget the pages of `blocks` of domain `domain`, each
    /// 2 to its order pages from its address, which is aligned to as many
    /// ([`blocks`]); `leaf` if only page entries changed, and no table
    /// went. Returns once it has.
    fn forget_pages(
        &mut self,
        domain: u16,
        blocks: &[(u64, u8)],
        leaf: bool,
    ) -> Result<(), Self::Error>;
    /// Has the unit forget every page of domain `domain`.
    fn forget_domain(&mut self, domain: u16) -> Result<(), Self::Error>;
    /// Whether the unit caches entries that are not present too, so that
    /// pages just mapped must be forgotten as well.
    fn caches_absent(&self) -> bool;
    /// What a mapping needs after it on a unit that caches only present
    /// entries, such as a write buffer flush.
    fn mapped(&mut self) -> Result<(), Self::Error>;
}

/// Runs of pages one change remembers to invalidate one by one; past that
/// it invalidates the whole domain.
const RUNS: usize = 16;

/// What a change to a domain cleared, for the unit to forget.
struct Cleared {
    runs: [(u64, u64); RUNS],
    count: usize,
    overflowed: bool,
}

impl Cleared {
    fn new() -> Cleared {
        Cleared { runs: [(0, 0); RUNS], count: 0, overflowed: false }
    }

    fn add(&mut self, address: u64, pages: u64) {
        if self.count == RUNS {
            self.overflowed = true;
        } else {
            self.runs[self.count] = (address, pages);
            self.count += 1;
        }
    }

    /// The runs of the pages of `ranges` (address, length).
    fn of(ranges: impl Iterator<Item = (u64, u64)>) -> Cleared {
        let mut cleared = Cleared::new();
        for (start, len) in ranges.filter(|&(_, len)| len > 0) {
            let first = start & !(PAGE - 1);
            let end = start.saturating_add(len).next_multiple_of(PAGE);
            cleared.add(first, (end - first) / PAGE);
        }
        cleared
    }

    /// Has `unit` forget what was cleared in domain `domain`; `leaf` if
    /// no table went.
    fn flush<U: Invalidate>(&self, unit: &mut U, domain: u16, leaf: bool) -> Result<(), U::Error> {
        if self.overflowed {
            return unit.forget_domain(domain);
        }
        let mut batch = [(0, 0); 32];
        let mut count = 0;
        for &(start, pages) in &self.runs[..self.count] {
            for block in blocks(start, pages) {
                if count == batch.len() {
                    unit.forget_pages(domain, &batch, leaf)?;
                    count = 0;
                }
                batch[count] = block;
                count += 1;
            }
        }
        if count > 0 { unit.forget_pages(domain, &batch[..count], leaf) } else { Ok(()) }
    }
}

/// After a mapping of `ranges`: has a unit that caches absent entries
/// forget the pages, or, if the mapping failed, forget what it undid.
fn settle<F: Format, U: Invalidate>(
    domain: &mut Domain<F>,
    unit: &mut U,
    memory: &impl Memory,
    mapped: bool,
    cleared: &Cleared,
    ranges: impl Iterator<Item = (u64, u64)>,
) -> Result<bool, U::Error> {
    if !mapped {
        cleared.flush(unit, domain.id(), false)?;
        domain.release(memory);
        return Ok(false);
    }
    if unit.caches_absent() {
        Cleared::of(ranges).flush(unit, domain.id(), true)?;
    } else {
        unit.mapped()?;
    }
    Ok(true)
}

/// Maps the pages of `frames` (physical address, length) in `domain` for
/// reading and writing, and has `unit` see them. `Ok(false)` if they
/// could not all be mapped: then none is.
pub fn map<F: Format, U: Invalidate>(
    domain: &mut Domain<F>,
    unit: &mut U,
    memory: &impl Memory,
    frames: &[(u64, u32)],
) -> Result<bool, U::Error> {
    let mut cleared = Cleared::new();
    let mapped = domain.map(memory, frames, &mut |address, pages| cleared.add(address, pages));
    let ranges = frames.iter().map(|&(phys, len)| (phys, u64::from(len)));
    settle(domain, unit, memory, mapped.is_ok(), &cleared, ranges)
}

/// Maps `len` bytes from `start`, as [`map`].
pub fn map_range<F: Format, U: Invalidate>(
    domain: &mut Domain<F>,
    unit: &mut U,
    memory: &impl Memory,
    start: u64,
    len: u64,
) -> Result<bool, U::Error> {
    let mut cleared = Cleared::new();
    let mapped =
        domain.map_range(memory, start, len, &mut |address, pages| cleared.add(address, pages));
    settle(domain, unit, memory, mapped.is_ok(), &cleared, core::iter::once((start, len)))
}

/// Unmaps what [`map`] mapped of `frames`, once, and returns when `unit`
/// has forgotten the pages that went: the device no longer reaches them.
pub fn unmap<F: Format, U: Invalidate>(
    domain: &mut Domain<F>,
    unit: &mut U,
    memory: &impl Memory,
    frames: &[(u64, u32)],
) -> Result<(), U::Error> {
    let mut cleared = Cleared::new();
    let tables = domain.unmap(memory, frames, &mut |address, pages| cleared.add(address, pages));
    // Tables the unit may still walk stay, if it would not forget.
    cleared.flush(unit, domain.id(), !tables)?;
    domain.release(memory);
    Ok(())
}
