// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A domain: the page table of the devices a unit has reach it, each
//! page mapped at its own physical address.
//!
//! A domain keeps counts in bits of its entries that the unit ignores
//! ([`Format::count`]): a page's entry counts the mappings of its page,
//! so a page goes when its last mapping does. Where the format has room
//! for it ([`Format::COUNTS_TABLES`]), a table's entry counts the entries
//! in use in the table it points to; elsewhere a table is found empty by
//! looking at its entries. A table goes when it is empty, and the tables
//! that go are freed only after the unit has forgotten them
//! ([`Domain::release`]). A page mapped [`Format::COUNT_MAX`] times at
//! once stays mapped for good.

use alloc::vec::Vec;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::Memory;

/// The size of the pages a domain maps.
pub const PAGE: u64 = 4096;

/// The physical address in an entry, in both formats.
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;

/// How a unit's page table entries are laid out.
pub trait Format {
    /// The most a count holds.
    const COUNT_MAX: u64;
    /// Whether a table's entry has room to count the 512 entries of the
    /// table below it.
    const COUNTS_TABLES: bool;

    /// The entry of a table of level `level`, 2 or more, that points to
    /// the table at `child`, for reading and writing.
    fn table(child: u64, level: u8) -> u64;
    /// The entry that maps the page at `address`, for reading and writing.
    fn page(address: u64) -> u64;
    /// Whether `entry` maps a page or points to a table.
    fn present(entry: u64) -> bool;
    /// The count `entry` keeps.
    fn count(entry: u64) -> u64;
    /// `entry` keeping `count` instead, which is at most
    /// [`Format::COUNT_MAX`].
    fn with_count(entry: u64, count: u64) -> u64;
}

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

/// A device's page table, in the format `F`.
#[derive(Debug)]
pub struct Domain<F> {
    id: u16,
    levels: u8,
    top: u64,
    coherent: bool,
    /// Tables emptied by unmapping, to be freed once the unit has
    /// forgotten them.
    freed: Vec<u64>,
    format: PhantomData<F>,
}

/// The index of `address` in a table of level `level`, 1 being the
/// tables of pages.
fn index(address: u64, level: u8) -> usize {
    ((address >> (12 + 9 * u32::from(level - 1))) & 0x1ff) as usize
}

/// The pages of `len` bytes from `start`: the first's address and how
/// many.
fn pages(start: u64, len: u64) -> Option<(u64, u64)> {
    let first = start & !(PAGE - 1);
    let end = start.checked_add(len)?.checked_next_multiple_of(PAGE)?;
    Some((first, (end - first) / PAGE))
}

/// Whether `entries` holds nothing but at `at`, looking out from there,
/// so that a run of pages unmapped in order finds a neighbour at once.
fn empty<F: Format>(entries: &[AtomicU64; 512], at: usize) -> bool {
    let present = |i: usize| F::present(entries[i].load(Ordering::Acquire));
    !(at + 1..512).any(present) && !(0..at).rev().any(present)
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

impl<F: Format> Domain<F> {
    /// An empty domain numbered `id`, of tables of `levels` levels, whose
    /// writes go back to memory unless the unit is `coherent`.
    pub fn new(memory: &impl Memory, id: u16, levels: u8, coherent: bool) -> Option<Domain<F>> {
        let top = memory.alloc_page()?;
        Some(Domain { id, levels, top, coherent, freed: Vec::new(), format: PhantomData })
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

    /// Counts one more entry in use below `entry`, a table's entry, where
    /// the format counts them.
    fn count_in(entry: Option<&AtomicU64>) {
        if let Some(entry) = entry.filter(|_| F::COUNTS_TABLES) {
            let value = entry.load(Ordering::Relaxed);
            entry.store(F::with_count(value, F::count(value) + 1), Ordering::Relaxed);
        }
    }

    /// Maps the page at `address` once more.
    fn map_page(&mut self, memory: &impl Memory, address: u64) -> Result<(), MapError> {
        let mut table = self.top;
        let mut parent: Option<&AtomicU64> = None;
        for level in (2..=self.levels).rev() {
            let entry = &memory.entries(table)[index(address, level)];
            let value = entry.load(Ordering::Acquire);
            table = if F::present(value) {
                value & ADDRESS
            } else {
                // A table left behind here when memory runs out further
                // down stays, empty, for the next mapping to use.
                let child = memory.alloc_page().ok_or(MapError::NoMemory)?;
                self.write(memory, entry, F::table(child, level));
                Self::count_in(parent);
                child
            };
            parent = Some(entry);
        }
        let entry = &memory.entries(table)[index(address, 1)];
        let value = entry.load(Ordering::Acquire);
        if F::present(value) {
            let count = F::count(value);
            if count < F::COUNT_MAX {
                entry.store(F::with_count(value, count + 1), Ordering::Relaxed);
            }
            return Ok(());
        }
        self.write(memory, entry, F::with_count(F::page(address), 1));
        Self::count_in(parent);
        Ok(())
    }

    /// Unmaps the pages of `frames` once, as [`Domain::map`] mapped them.
    /// `cleared` is told of each run of pages that went (its first page's
    /// address and how many), which the unit must forget before anything
    /// else uses them. Returns whether tables went too: their pages wait
    /// for [`Domain::release`].
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
        let mut path = [(0u64, 0usize); 7];
        let mut table = self.top;
        for level in (1..=self.levels).rev() {
            let at = index(address, level);
            path[usize::from(level)] = (table, at);
            if level == 1 {
                break;
            }
            let value = memory.entries(table)[at].load(Ordering::Acquire);
            if !F::present(value) {
                return Unmapped::Missing;
            }
            table = value & ADDRESS;
        }
        let (table, at) = path[1];
        let entry = &memory.entries(table)[at];
        let value = entry.load(Ordering::Acquire);
        if !F::present(value) {
            return Unmapped::Missing;
        }
        match F::count(value) {
            count if count == F::COUNT_MAX => return Unmapped::Kept,
            0 | 1 => {}
            count => {
                entry.store(F::with_count(value, count - 1), Ordering::Relaxed);
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
            if F::COUNTS_TABLES {
                let left = F::count(value).saturating_sub(1);
                if left > 0 {
                    entry.store(F::with_count(value, left), Ordering::Relaxed);
                    break;
                }
            } else {
                let (_, below) = path[usize::from(level) - 1];
                if !empty::<F>(memory.entries(value & ADDRESS), below) {
                    break;
                }
            }
            if self.freed.try_reserve(1).is_err() {
                // No room to remember it: the empty table stays.
                entry.store(F::with_count(value, 0), Ordering::Relaxed);
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
            if !F::present(value) {
                return None;
            }
            if level == 1 {
                return Some((value & ADDRESS, F::count(value)));
            }
            table = value & ADDRESS;
        }
        None
    }

    /// Frees every table, once no device names the domain and the unit has
    /// forgotten it.
    pub fn destroy(mut self, memory: &impl Memory) {
        self.release(memory);
        free_table::<F>(memory, self.top, self.levels);
    }
}

/// Frees the table at `table`, of level `level`, and those below it.
fn free_table<F: Format>(memory: &impl Memory, table: u64, level: u8) {
    if level > 1 {
        for entry in memory.entries(table) {
            let value = entry.load(Ordering::Acquire);
            if F::present(value) {
                free_table::<F>(memory, value & ADDRESS, level - 1);
            }
        }
    }
    memory.free_page(table);
}
