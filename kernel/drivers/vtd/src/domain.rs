// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A domain: the second-level page table of the devices whose context
//! entries name it (VT-d 4.1, section 9.8), each page mapped at its own
//! physical address.
//!
//! Bits 52 to 61 of a second-level entry are ignored by the unit, so a
//! domain keeps counts there: a page's entry counts the mappings of its
//! page, and a table's entry the entries in use in the table it points
//! to. A page goes when its last mapping does, and a table when its last
//! entry does; the tables that go are freed only after the unit has
//! forgotten them ([`Domain::release`]). A page mapped [`COUNT_MAX`]
//! times at once stays mapped for good.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::Memory;

/// The size of the pages a domain maps.
pub const PAGE: u64 = 4096;

const READ: u64 = 1 << 0;
const WRITE: u64 = 1 << 1;
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;
const COUNT_SHIFT: u32 = 52;
/// The most a count holds.
const COUNT_MAX: u64 = 0x3ff;
const COUNT: u64 = COUNT_MAX << COUNT_SHIFT;
const ONE: u64 = 1 << COUNT_SHIFT;

/// Why pages could not be mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Memory for a page table ran out.
    NoMemory,
    /// A page is past the addresses the domain's tables reach.
    TooHigh,
}

/// What became of a page's entry when it was unmapped once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unmapped {
    /// It was not mapped.
    Missing,
    /// Other mappings keep it.
    Kept,
    /// It is gone, and with it tables, if `tables`.
    Cleared { tables: bool },
}

/// A device's page table.
#[derive(Debug)]
pub struct Domain {
    id: u16,
    levels: u8,
    top: u64,
    coherent: bool,
    /// Tables emptied by unmapping, to be freed once the unit has
    /// forgotten them.
    freed: Vec<u64>,
}

/// The index of `address` in a table of level `level`, 1 being the
/// tables of pages.
fn index(address: u64, level: u8) -> usize {
    ((address >> (12 + 9 * u32::from(level - 1))) & 0x1ff) as usize
}

/// The count an entry keeps.
fn count(entry: u64) -> u64 {
    (entry & COUNT) >> COUNT_SHIFT
}

/// The pages of `len` bytes from `start`: the first's address and how
/// many.
fn pages(start: u64, len: u64) -> Option<(u64, u64)> {
    let first = start & !(PAGE - 1);
    let end = start.checked_add(len)?.checked_next_multiple_of(PAGE)?;
    Some((first, (end - first) / PAGE))
}

/// Splits `pages` pages from `start` into blocks of 2 to some order of
/// pages, each aligned to as many: what page-selective invalidations
/// take. Yields each block's address and order.
pub fn blocks(mut start: u64, mut pages: u64) -> impl Iterator<Item = (u64, u8)> {
    core::iter::from_fn(move || {
        if pages == 0 {
            return None;
        }
        let aligned = (start / PAGE).trailing_zeros().min(63);
        let order = aligned.min(pages.ilog2());
        let block = (start, order as u8);
        start += PAGE << order;
        pages -= 1 << order;
        Some(block)
    })
}

impl Domain {
    /// An empty domain numbered `id`, of tables of `levels` levels, whose
    /// writes go back to memory unless the unit is `coherent`.
    pub(crate) fn new(memory: &impl Memory, id: u16, levels: u8, coherent: bool) -> Option<Domain> {
        let top = memory.alloc_page()?;
        Some(Domain { id, levels, top, coherent, freed: Vec::new() })
    }

    /// Its number, which the unit's caches know it by.
    pub fn id(&self) -> u16 {
        self.id
    }

    /// The levels of its tables.
    pub fn levels(&self) -> u8 {
        self.levels
    }

    /// Its top table's physical address.
    pub fn top(&self) -> u64 {
        self.top
    }

    /// The bits of address its tables reach.
    pub fn width(&self) -> u32 {
        12 + 9 * u32::from(self.levels)
    }

    /// Writes `value` to `entry`, where the unit's walks see it.
    fn write(&self, memory: &impl Memory, entry: &AtomicU64, value: u64) {
        entry.store(value, Ordering::Release);
        if !self.coherent {
            memory.flush(entry);
        }
    }

    /// Maps the pages of `frames` (physical address, length) at their
    /// addresses, for reading and writing. On an error, what this call had
    /// mapped is unmapped again, and `cleared` is told of what went, for
    /// the unit to forget ([`Domain::unmap`]).
    pub fn map(
        &mut self,
        memory: &impl Memory,
        frames: &[(u64, u32)],
        cleared: &mut dyn FnMut(u64, u64),
    ) -> Result<(), MapError> {
        for (at, &(phys, len)) in frames.iter().enumerate() {
            if let Err(err) = self.map_range(memory, phys, u64::from(len), cleared) {
                for &(phys, len) in &frames[..at] {
                    self.unmap_range(memory, phys, u64::from(len), cleared);
                }
                return Err(err);
            }
        }
        Ok(())
    }

    /// Maps the pages of `len` bytes from `start`, as [`Domain::map`].
    pub fn map_range(
        &mut self,
        memory: &impl Memory,
        start: u64,
        len: u64,
        cleared: &mut dyn FnMut(u64, u64),
    ) -> Result<(), MapError> {
        if len == 0 {
            return Ok(());
        }
        let (first, count) = pages(start, len).ok_or(MapError::TooHigh)?;
        if (first + count * PAGE - 1) >> self.width() != 0 {
            return Err(MapError::TooHigh);
        }
        for n in 0..count {
            if let Err(err) = self.map_page(memory, first + n * PAGE) {
                self.unmap_range(memory, first, n * PAGE, cleared);
                return Err(err);
            }
        }
        Ok(())
    }

    /// Maps the page at `address` once more.
    fn map_page(&mut self, memory: &impl Memory, address: u64) -> Result<(), MapError> {
        let mut table = self.top;
        let mut parent: Option<&AtomicU64> = None;
        for level in (2..=self.levels).rev() {
            let entry = &memory.entries(table)[index(address, level)];
            let value = entry.load(Ordering::Acquire);
            table = if value & READ != 0 {
                value & ADDRESS
            } else {
                // A table left behind here when memory runs out further
                // down stays, empty, for the next mapping to use.
                let child = memory.alloc_page().ok_or(MapError::NoMemory)?;
                self.write(memory, entry, child | READ | WRITE);
                if let Some(parent) = parent {
                    parent.fetch_add(ONE, Ordering::Relaxed);
                }
                child
            };
            parent = Some(entry);
        }
        let entry = &memory.entries(table)[index(address, 1)];
        let value = entry.load(Ordering::Acquire);
        if value & READ != 0 {
            if count(value) < COUNT_MAX {
                entry.store(value + ONE, Ordering::Relaxed);
            }
            return Ok(());
        }
        self.write(memory, entry, address | READ | WRITE | ONE);
        if let Some(parent) = parent {
            parent.fetch_add(ONE, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Unmaps the pages of `frames` once, as [`Domain::map`] mapped them.
    /// `cleared` is told of each run of pages that went (its first page's
    /// address and how many), which the unit must forget before anything
    /// else uses them ([`crate::Unit::flush`]). Returns whether tables
    /// went too: their pages wait for [`Domain::release`].
    pub fn unmap(
        &mut self,
        memory: &impl Memory,
        frames: &[(u64, u32)],
        cleared: &mut dyn FnMut(u64, u64),
    ) -> bool {
        let mut tables = false;
        for &(phys, len) in frames {
            tables |= self.unmap_range(memory, phys, u64::from(len), cleared);
        }
        tables
    }

    /// Unmaps the pages of `len` bytes from `start`, as
    /// [`Domain::unmap`].
    pub fn unmap_range(
        &mut self,
        memory: &impl Memory,
        start: u64,
        len: u64,
        cleared: &mut dyn FnMut(u64, u64),
    ) -> bool {
        let Some((first, count)) = pages(start, len).filter(|_| len > 0) else { return false };
        let mut tables = false;
        let mut run: Option<(u64, u64)> = None;
        for n in 0..count {
            let address = first + n * PAGE;
            match self.unmap_page(memory, address) {
                Unmapped::Cleared { tables: went } => {
                    tables |= went;
                    run = match run {
                        Some((from, pages)) if from + pages * PAGE == address => {
                            Some((from, pages + 1))
                        }
                        Some((from, pages)) => {
                            cleared(from, pages);
                            Some((address, 1))
                        }
                        None => Some((address, 1)),
                    };
                }
                Unmapped::Kept | Unmapped::Missing => {}
            }
        }
        if let Some((from, pages)) = run {
            cleared(from, pages);
        }
        tables
    }

    /// Unmaps the page at `address` once.
    fn unmap_page(&mut self, memory: &impl Memory, address: u64) -> Unmapped {
        // Each level's table and the index there on the way down.
        let mut path = [(0u64, 0usize); 6];
        let mut table = self.top;
        for level in (1..=self.levels).rev() {
            let at = index(address, level);
            path[usize::from(level)] = (table, at);
            if level == 1 {
                break;
            }
            let value = memory.entries(table)[at].load(Ordering::Acquire);
            if value & READ == 0 {
                return Unmapped::Missing;
            }
            table = value & ADDRESS;
        }
        let (table, at) = path[1];
        let entry = &memory.entries(table)[at];
        let value = entry.load(Ordering::Acquire);
        if value & READ == 0 {
            return Unmapped::Missing;
        }
        match count(value) {
            COUNT_MAX => return Unmapped::Kept,
            0 | 1 => {}
            _ => {
                entry.store(value - ONE, Ordering::Relaxed);
                return Unmapped::Kept;
            }
        }
        self.write(memory, entry, 0);
        // The table it was in lost an entry; each that is left empty goes,
        // and the one above it loses one.
        let mut tables = false;
        for level in 2..=self.levels {
            let (table, at) = path[usize::from(level)];
            let entry = &memory.entries(table)[at];
            let value = entry.load(Ordering::Acquire);
            let left = count(value).saturating_sub(1);
            if left > 0 {
                entry.store((value & !COUNT) | (left << COUNT_SHIFT), Ordering::Relaxed);
                break;
            }
            if self.freed.try_reserve(1).is_err() {
                // No room to remember it: the empty table stays.
                entry.store(value & !COUNT, Ordering::Relaxed);
                break;
            }
            let _ = self.freed.push_within_capacity(value & ADDRESS);
            self.write(memory, entry, 0);
            tables = true;
        }
        Unmapped::Cleared { tables }
    }

    /// Frees the tables unmapping emptied, once the unit has forgotten
    /// them.
    pub fn release(&mut self, memory: &impl Memory) {
        for table in self.freed.drain(..) {
            memory.free_page(table);
        }
    }

    /// The page `address` is mapped to and how many times it is mapped,
    /// if it is.
    pub fn lookup(&self, memory: &impl Memory, address: u64) -> Option<(u64, u64)> {
        let mut table = self.top;
        for level in (1..=self.levels).rev() {
            let value = memory.entries(table)[index(address, level)].load(Ordering::Acquire);
            if value & READ == 0 {
                return None;
            }
            if level == 1 {
                return Some((value & ADDRESS, count(value)));
            }
            table = value & ADDRESS;
        }
        None
    }

    /// Frees every table, once no device's context entry names the domain
    /// and the unit has forgotten it.
    pub fn destroy(mut self, memory: &impl Memory) {
        self.release(memory);
        free_table(memory, self.top, self.levels);
    }
}

/// Frees the table at `table`, of level `level`, and those below it.
fn free_table(memory: &impl Memory, table: u64, level: u8) {
    if level > 1 {
        for entry in memory.entries(table) {
            let value = entry.load(Ordering::Acquire);
            if value & READ != 0 {
                free_table(memory, value & ADDRESS, level - 1);
            }
        }
    }
    memory.free_page(table);
}
