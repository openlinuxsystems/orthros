// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Kani proof harnesses for the allocators' invariants.
//!
//! Kani checks each harness for every value its `kani::any()` inputs can
//! take, within the loop bounds given, so these are proofs over all inputs
//! of the stated size rather than samples. `ob test kani` runs them;
//! `docs/design/memory.md` lists what each one establishes.

use core::sync::atomic::AtomicU64;

use crate::asid::Asids;
use crate::buddy::{Buddy, MAX_ORDER};
use crate::class::{self, CLASSES, Class, MAX_SMALL};
use crate::magazine::{Loaded, Magazine, ROUNDS};
use crate::page::{Owner, PAGE_SIZE, PageInfo, Region};
use crate::vrange::RangeTable;

/// Frames in the zone the buddy proofs use: room for a block of every
/// order up to [`MAX_ORDER`], which is 2 in the Kani build.
const FRAMES: usize = 4;
/// Operations in a sequence.
const STEPS: usize = 3;

fn zone(pages: &[PageInfo]) -> Buddy<'_> {
    let mut buddy = Buddy::new();
    buddy.add_region(Region { base: 8, pages }).unwrap();
    buddy.release(8, 8 + FRAMES as u64);
    buddy
}

/// Any sequence of allocations and frees keeps the free lists consistent
/// ([`Buddy::check`]), conserves frames, and hands out aligned blocks that
/// never overlap.
///
/// The sequence has any length up to `STEPS`, and the free lists are
/// checked once, at its end. Every state a longer sequence passes through
/// is the end of a shorter one, so this covers them all, and the costly
/// walk of the lists is modelled once instead of after every step.
#[kani::proof]
#[kani::unwind(5)]
fn buddy_keeps_its_invariants() {
    let pages: [PageInfo; FRAMES] = Default::default();
    let mut buddy = zone(&pages);
    let mut blocks = [(0u64, 0u32); STEPS];
    let mut live = 0;
    let steps: usize = kani::any_where(|&s| (1..=STEPS).contains(&s));
    for step in 0..STEPS {
        if step == steps {
            break;
        }
        if kani::any() || live == 0 {
            let order: u32 = kani::any_where(|&o| o <= MAX_ORDER);
            if let Some(pfn) = buddy.alloc(order, Owner::Kernel, 0) {
                assert!(pfn.is_multiple_of(1 << order));
                assert!(pfn >= 8 && pfn + (1 << order) <= 8 + FRAMES as u64);
                for &(other, other_order) in &blocks[..live] {
                    assert!(pfn + (1 << order) <= other || other + (1 << other_order) <= pfn);
                }
                blocks[live] = (pfn, order);
                live += 1;
            } else {
                assert!(!buddy.can_allocate(order));
            }
        } else {
            let index: usize = kani::any_where(|&i| i < live);
            let (pfn, order) = blocks[index];
            assert!(buddy.free(pfn, order, Owner::Kernel).is_ok());
            blocks[index] = blocks[live - 1];
            live -= 1;
        }
        let used: u64 = blocks[..live].iter().map(|&(_, order)| 1u64 << order).sum();
        assert_eq!(buddy.free_pages() + used, FRAMES as u64);
    }
    assert!(buddy.check().is_ok());
}

/// A free is accepted only for exactly the block that was allocated, with
/// its order and owner, whatever frame number, order and owner it names.
/// ([`Buddy::free`] checks with [`Buddy::check_free`] before it changes
/// anything, and `buddy_keeps_its_invariants` covers accepted frees.)
#[kani::proof]
#[kani::unwind(6)]
fn buddy_refuses_bad_frees() {
    let pages: [PageInfo; FRAMES] = Default::default();
    let mut buddy = zone(&pages);
    let order: u32 = kani::any_where(|&o| o <= MAX_ORDER);
    let block = buddy.alloc(order, Owner::Slab, 0).unwrap();
    let (pfn, freed_order): (u64, u32) = (kani::any(), kani::any());
    let owner = if kani::any() { Owner::Slab } else { Owner::Heap };
    let accepted = buddy.check_free(pfn, freed_order, owner).is_ok();
    assert_eq!(accepted, pfn == block && freed_order == order && owner == Owner::Slab);
}

/// The class chosen for a request is the smallest that holds it with its
/// alignment, and requests no class fits go to whole frames.
#[kani::proof]
#[kani::unwind(34)]
fn classes_fit_requests() {
    let size: usize = kani::any_where(|&s| s <= MAX_SMALL + 1);
    let shift: u32 = kani::any_where(|&s| s <= 13);
    let align = 1usize << shift;
    match Class::for_layout(size, align) {
        Some(class) => {
            assert!(class.index() < CLASSES);
            assert!(class.size() >= size && class.align() >= align);
            if class.index() > 0 {
                let smaller = Class::from_index(class.index() - 1).unwrap();
                assert!(smaller.size() < size || smaller.align() < align);
            }
            assert!(class.objects() >= 4 && class.objects() < usize::from(u16::MAX));
        }
        None => {
            assert!(size > MAX_SMALL || align > PAGE_SIZE);
        }
    }
}

/// Large requests get the smallest block of frames that holds them with
/// their alignment.
#[kani::proof]
fn large_orders_fit_requests() {
    let size: usize = kani::any();
    let shift: u32 = kani::any_where(|&s| s < 32);
    let align = 1usize << shift;
    if let Some(order) = class::large_order(size, align) {
        let bytes = PAGE_SIZE << order;
        assert!(order <= MAX_ORDER && bytes >= size && bytes >= align);
        assert!(order == 0 || (bytes / 2 < size || bytes / 2 < align));
    } else {
        assert!(size.max(align) > PAGE_SIZE << MAX_ORDER);
    }
}

/// A CPU's two magazines never lose or duplicate an object: what comes
/// out was put in, and pushes fail only when both are full.
#[kani::proof]
#[kani::unwind(32)]
fn magazines_hold_what_was_pushed() {
    let capacity: usize = kani::any_where(|&c| c >= 1 && c <= ROUNDS);
    let loaded: &'static mut Magazine = Box::leak(Box::new(Magazine::new()));
    let previous: &'static mut Magazine = Box::leak(Box::new(Magazine::new()));
    let mut cpu = Loaded { loaded: Some(loaded), previous: Some(previous) };
    let mut held = 0usize;
    for step in 1..=4usize {
        if kani::any() {
            match cpu.push(step, capacity) {
                Ok(()) => held += 1,
                Err(object) => {
                    assert_eq!(object, step);
                    assert_eq!(held, 2 * capacity);
                }
            }
        } else {
            match cpu.pop() {
                Some(object) => {
                    assert!(object >= 1 && object < step);
                    held -= 1;
                }
                None => assert_eq!(held, 0),
            }
        }
        assert_eq!(cpu.len(), held);
    }
}

/// The mapping window's free extents stay sorted and disjoint, ranges
/// handed out never overlap, and every byte is either free or handed out.
#[kani::proof]
#[kani::unwind(6)]
fn ranges_never_overlap() {
    const P: u64 = PAGE_SIZE as u64;
    let mut ranges = RangeTable::<4>::new();
    ranges.init(16 * P, 32 * P);
    let mut taken = [(0u64, 0u64); STEPS];
    let mut live = 0;
    for _ in 0..STEPS {
        if kani::any() || live == 0 {
            let pages: u64 = kani::any_where(|&p| p >= 1 && p <= 8);
            let shift: u32 = kani::any_where(|&s| s <= 2);
            if let Some(start) = ranges.alloc(pages * P, P << shift) {
                let end = start + pages * P;
                assert!(start.is_multiple_of(P << shift) && start >= 16 * P && end <= 32 * P);
                for &(s, e) in &taken[..live] {
                    assert!(end <= s || e <= start);
                }
                taken[live] = (start, end);
                live += 1;
            }
        } else {
            let index: usize = kani::any_where(|&i| i < live);
            let (start, end) = taken[index];
            assert!(ranges.release(start, end - start).is_ok());
            taken[index] = taken[live - 1];
            live -= 1;
        }
        assert!(ranges.check().is_ok());
        let used: u64 = taken[..live].iter().map(|&(s, e)| e - s).sum();
        assert_eq!(ranges.free_bytes() + used, 16 * P);
    }
}

/// With three identifiers and two CPUs, two address spaces running at the
/// same time never share an identifier, through any rollover.
#[kani::proof]
#[kani::unwind(6)]
fn running_spaces_never_share_an_identifier() {
    let asids = Asids::<2, 2>::new();
    let spaces = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
    let mut running: [Option<(usize, u16)>; 2] = [None; 2];
    for _ in 0..4 {
        let cpu: usize = kani::any_where(|&c| c < 2);
        let space: usize = kani::any_where(|&s| s < 4);
        let (id, _flush) = asids.switch(cpu, &spaces[space]);
        assert!(id != 0);
        running[cpu] = Some((space, id));
        if let (Some((a, a_id)), Some((b, b_id))) = (running[0], running[1]) {
            assert!(a == b || a_id != b_id);
        }
    }
}
