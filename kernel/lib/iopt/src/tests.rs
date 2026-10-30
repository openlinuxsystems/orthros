// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Domains in two formats, as the units have them: one whose table entries
//! count the table below (VT-d's), and one with only a few ignored bits,
//! scattered (AMD-Vi's), whose tables are found empty by looking. Each is
//! checked against a model of what is mapped, and a unit that only counts
//! what it is told to forget.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::vec::Vec;

use proptest::prelude::*;

use crate::{Domain, Format, Invalidate, MapError, Memory, PAGE, blocks, map, map_range, unmap};

/// A page of table entries, aligned as a unit needs.
#[repr(C, align(4096))]
struct Table([AtomicU64; 512]);

/// Host memory for tables: each page leaked, found again by the address
/// it stands at.
struct HostMemory {
    pages: Mutex<HashMap<u64, &'static Table>>,
    /// Allocations allowed before they fail.
    allowed: AtomicUsize,
}

impl HostMemory {
    fn new() -> HostMemory {
        HostMemory { pages: Mutex::new(HashMap::new()), allowed: AtomicUsize::new(usize::MAX) }
    }

    fn live(&self) -> usize {
        self.pages.lock().unwrap().len()
    }
}

impl Memory for HostMemory {
    fn alloc_page(&self) -> Option<u64> {
        let allowed = self.allowed.load(Ordering::Relaxed);
        if allowed == 0 {
            return None;
        }
        if allowed != usize::MAX {
            self.allowed.store(allowed - 1, Ordering::Relaxed);
        }
        let table: &'static Table = Box::leak(Box::new(Table([const { AtomicU64::new(0) }; 512])));
        let phys = core::ptr::from_ref(table) as u64;
        self.pages.lock().unwrap().insert(phys, table);
        Some(phys)
    }

    fn free_page(&self, phys: u64) {
        assert!(self.pages.lock().unwrap().remove(&phys).is_some(), "a page given back twice");
    }

    fn entries(&self, phys: u64) -> &[AtomicU64; 512] {
        &self.pages.lock().unwrap().get(&phys).expect("a table the domain made").0
    }

    fn flush(&self, _entry: &AtomicU64) {}

    fn delay_us(&self, _us: u32) {}
}

/// VT-d's second-level entries: read and write in bits 0 and 1, counts in
/// 52 to 61.
struct Counted;

impl Format for Counted {
    const COUNT_MAX: u64 = 0x3ff;
    const COUNTS_TABLES: bool = true;

    fn table(child: u64, _level: u8) -> u64 {
        child | 3
    }

    fn page(address: u64) -> u64 {
        address | 3
    }

    fn present(entry: u64) -> bool {
        entry & 1 != 0
    }

    fn count(entry: u64) -> u64 {
        (entry >> 52) & 0x3ff
    }

    fn with_count(entry: u64, count: u64) -> u64 {
        (entry & !(0x3ff << 52)) | (count << 52)
    }
}

/// AMD-Vi's entries: present in bit 0, the next level in 9 to 11,
/// permissions in 61 and 62, and only bits 1 to 4, 7, 8 and 63 ignored.
struct Scattered;

impl Format for Scattered {
    const COUNT_MAX: u64 = 0x7f;
    const COUNTS_TABLES: bool = false;

    fn table(child: u64, level: u8) -> u64 {
        child | 1 | (u64::from(level - 1) << 9) | (3 << 61)
    }

    fn page(address: u64) -> u64 {
        address | 1 | (3 << 61)
    }

    fn present(entry: u64) -> bool {
        entry & 1 != 0
    }

    fn count(entry: u64) -> u64 {
        ((entry >> 1) & 0xf) | (((entry >> 7) & 3) << 4) | ((entry >> 63) << 6)
    }

    fn with_count(entry: u64, count: u64) -> u64 {
        let ignored = (0xf << 1) | (3 << 7) | (1 << 63);
        (entry & !ignored) | ((count & 0xf) << 1) | (((count >> 4) & 3) << 7) | ((count >> 6) << 63)
    }
}

/// A unit that remembers what it was told to forget.
#[derive(Default)]
struct Told {
    pages: Vec<(u16, u64, u8, bool)>,
    domains: Vec<u16>,
    caches_absent: bool,
    mapped: usize,
}

impl Invalidate for Told {
    type Error = ();

    fn forget_pages(&mut self, domain: u16, blocks: &[(u64, u8)], leaf: bool) -> Result<(), ()> {
        self.pages.extend(blocks.iter().map(|&(address, order)| (domain, address, order, leaf)));
        Ok(())
    }

    fn forget_domain(&mut self, domain: u16) -> Result<(), ()> {
        self.domains.push(domain);
        Ok(())
    }

    fn caches_absent(&self) -> bool {
        self.caches_absent
    }

    fn mapped(&mut self) -> Result<(), ()> {
        self.mapped += 1;
        Ok(())
    }
}

/// The pages `told` had forgotten, by address.
fn forgotten(told: &Told) -> Vec<u64> {
    let mut pages: Vec<u64> = told
        .pages
        .iter()
        .flat_map(|&(_, address, order, _)| (0..1u64 << order).map(move |n| address + n * PAGE))
        .collect();
    pages.sort_unstable();
    pages
}

fn counts_mappings_and_frees_tables<F: Format>() {
    let memory = HostMemory::new();
    let mut domain = Domain::<F>::new(&memory, 3, 4, true).unwrap();
    let mut unit = Told::default();
    assert_eq!(map(&mut domain, &mut unit, &memory, &[(0x10_0000, 0x2000)]), Ok(true));
    assert_eq!(map(&mut domain, &mut unit, &memory, &[(0x10_1800, 16)]), Ok(true));
    assert_eq!(domain.lookup(&memory, 0x10_0000), Some((0x10_0000, 1)));
    assert_eq!(domain.lookup(&memory, 0x10_1000), Some((0x10_1000, 2)));
    assert_eq!(domain.lookup(&memory, 0x10_2000), None);
    // The top table and one of each level below.
    assert_eq!(memory.live(), 4);
    assert_eq!(unit.mapped, 2);

    assert_eq!(unmap(&mut domain, &mut unit, &memory, &[(0x10_0000, 0x2000)]), Ok(()));
    assert_eq!(domain.lookup(&memory, 0x10_0000), None);
    assert_eq!(domain.lookup(&memory, 0x10_1000), Some((0x10_1000, 1)));
    assert_eq!(forgotten(&unit), [0x10_0000]);
    assert_eq!(memory.live(), 4);

    assert_eq!(unmap(&mut domain, &mut unit, &memory, &[(0x10_1800, 16)]), Ok(()));
    assert_eq!(domain.lookup(&memory, 0x10_1000), None);
    assert_eq!(forgotten(&unit), [0x10_0000, 0x10_1000]);
    // Every table below the top went, and the unit was told so.
    assert_eq!(memory.live(), 1);
    assert_eq!(unit.pages.last().map(|page| page.3), Some(false));
    domain.destroy(&memory);
    assert_eq!(memory.live(), 0);
}

#[test]
fn a_domain_with_counted_tables_counts_mappings_and_frees_tables() {
    counts_mappings_and_frees_tables::<Counted>();
}

#[test]
fn a_domain_whose_tables_are_found_empty_counts_mappings_and_frees_tables() {
    counts_mappings_and_frees_tables::<Scattered>();
}

#[test]
fn scattered_counts_keep_the_other_bits() {
    let entry = Scattered::page(0x1234_5000);
    for count in 0..=Scattered::COUNT_MAX {
        let counted = Scattered::with_count(entry, count);
        assert_eq!(Scattered::count(counted), count);
        assert_eq!(Scattered::with_count(counted, 0), entry);
    }
}

fn a_page_mapped_too_often_stays<F: Format>() {
    let memory = HostMemory::new();
    let mut domain = Domain::<F>::new(&memory, 1, 3, true).unwrap();
    let mut unit = Told::default();
    for _ in 0..F::COUNT_MAX + 5 {
        assert_eq!(map(&mut domain, &mut unit, &memory, &[(0x5000, 8)]), Ok(true));
    }
    for _ in 0..F::COUNT_MAX + 5 {
        assert_eq!(unmap(&mut domain, &mut unit, &memory, &[(0x5000, 8)]), Ok(()));
    }
    assert_eq!(domain.lookup(&memory, 0x5000), Some((0x5000, F::COUNT_MAX)));
    assert!(unit.pages.is_empty());
}

#[test]
fn a_page_mapped_too_often_stays_for_good() {
    a_page_mapped_too_often_stays::<Counted>();
    a_page_mapped_too_often_stays::<Scattered>();
}

#[test]
fn a_mapping_past_the_width_is_refused() {
    let memory = HostMemory::new();
    let mut domain = Domain::<Scattered>::new(&memory, 1, 3, true).unwrap();
    let mut cleared = |_: u64, _: u64| {};
    assert_eq!(domain.map_range(&memory, 1 << 39, 4096, &mut cleared), Err(MapError::TooHigh));
    assert_eq!(domain.map_range(&memory, (1 << 39) - 4096, 4096, &mut cleared), Ok(()));
}

#[test]
fn a_mapping_that_runs_out_of_memory_takes_back_what_it_mapped() {
    let memory = HostMemory::new();
    let mut domain = Domain::<Scattered>::new(&memory, 1, 4, true).unwrap();
    let mut unit = Told::default();
    // Enough for the tables of the first page, and one of the second's.
    memory.allowed.store(4, Ordering::Relaxed);
    let frames = [(0x1000, 4096), (1 << 30, 4096)];
    assert_eq!(map(&mut domain, &mut unit, &memory, &frames), Ok(false));
    assert_eq!(domain.lookup(&memory, 0x1000), None);
    assert_eq!(forgotten(&unit), [0x1000]);
}

#[test]
fn a_unit_that_caches_absent_entries_forgets_new_pages() {
    let memory = HostMemory::new();
    let mut domain = Domain::<Counted>::new(&memory, 7, 4, true).unwrap();
    let mut unit = Told { caches_absent: true, ..Told::default() };
    assert_eq!(map_range(&mut domain, &mut unit, &memory, 0x3000, 0x3000), Ok(true));
    assert_eq!(unit.pages, [(7, 0x3000, 0, true), (7, 0x4000, 1, true)]);
    assert_eq!(unit.mapped, 0);
}

#[test]
fn many_runs_forget_the_whole_domain() {
    let memory = HostMemory::new();
    let mut domain = Domain::<Scattered>::new(&memory, 9, 4, true).unwrap();
    let mut unit = Told::default();
    let frames: Vec<(u64, u32)> = (0..40).map(|n| (n * 2 * PAGE, 4096)).collect();
    assert_eq!(map(&mut domain, &mut unit, &memory, &frames), Ok(true));
    assert_eq!(unmap(&mut domain, &mut unit, &memory, &frames), Ok(()));
    assert_eq!(unit.domains, [9]);
    assert_eq!(memory.live(), 1);
}

#[test]
fn ranges_split_into_aligned_blocks() {
    let found: Vec<(u64, u8)> = blocks(0x3000, 13).collect();
    assert_eq!(found, [(0x3000, 0), (0x4000, 2), (0x8000, 3)]);
    assert_eq!(blocks(0, 1 << 20).collect::<Vec<_>>(), [(0, 20)]);
}

/// Checks `ops` (page, map or not) on a domain of format `F` against a
/// model: every page maps exactly as often as it was mapped and not
/// unmapped, every page that went was forgotten, and nothing but the top
/// table is left once everything is unmapped.
fn against_the_model<F: Format>(ops: &[(u64, bool)]) {
    let memory = HostMemory::new();
    let mut domain = Domain::<F>::new(&memory, 2, 4, true).unwrap();
    let mut unit = Told::default();
    let mut model: BTreeMap<u64, u64> = BTreeMap::new();
    for &(page, add) in ops {
        let frame = [(page * PAGE, 4096)];
        if add {
            assert_eq!(map(&mut domain, &mut unit, &memory, &frame), Ok(true));
            *model.entry(page).or_default() += 1;
        } else if let Some(count) = model.get_mut(&page) {
            unit.pages.clear();
            assert_eq!(unmap(&mut domain, &mut unit, &memory, &frame), Ok(()));
            *count -= 1;
            if *count == 0 {
                model.remove(&page);
                assert_eq!(forgotten(&unit), [page * PAGE]);
            } else {
                assert!(unit.pages.is_empty());
            }
        }
        for (&page, &count) in &model {
            assert_eq!(domain.lookup(&memory, page * PAGE), Some((page * PAGE, count)));
        }
    }
    let left: Vec<(u64, u32)> = model
        .iter()
        .flat_map(|(&page, &count)| (0..count).map(move |_| (page * PAGE, 4096)))
        .collect();
    assert_eq!(unmap(&mut domain, &mut unit, &memory, &left), Ok(()));
    assert_eq!(memory.live(), 1);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn domains_map_exactly_what_they_are_given(
        ops in proptest::collection::vec(
            (prop_oneof![0u64..1100, (1u64 << 27)..(1u64 << 27) + 8], any::<bool>()),
            1..200,
        )
    ) {
        against_the_model::<Counted>(&ops);
        against_the_model::<Scattered>(&ops);
    }
}
