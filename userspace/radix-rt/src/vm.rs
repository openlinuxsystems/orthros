// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Addresses in the program's own part of its address space, from
//! `PROGRAM_BASE` up (radix-startup(2)), for the heap and for the
//! mappings the runtime makes.
//!
//! They are handed out in order from a random page in the first `WINDOW`
//! bytes, with an unmapped guard page after each run, and never handed
//! out twice: the part is terabytes long, far more than a program maps in
//! its life for now.

use core::sync::atomic::{AtomicU64, Ordering};

use libradix::SYS;
use radix_abi::address_space::PAGE_SIZE;
use radix_abi::{random, startup};

/// The next free address, or 0 before the first run is handed out.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Where the addresses end: well below the lowest stack.
const END: u64 = startup::STACK_TOP - startup::STACK_WINDOW - startup::STACK_SIZE;

/// A random page to start at.
fn start() -> u64 {
    let word = random::words(&SYS, startup::SLOT_RANDOM).map_or(0, |random| random.words[0]);
    startup::PROGRAM_BASE + (word % (startup::WINDOW / PAGE_SIZE)) * PAGE_SIZE
}

/// Takes `pages` pages of addresses that nothing maps yet, aligned to
/// `align` bytes (a power of two of at least a page).
pub fn take(pages: u64, align: u64) -> Option<u64> {
    if NEXT.load(Ordering::Acquire) == 0 {
        let _ = NEXT.compare_exchange(0, start(), Ordering::AcqRel, Ordering::Acquire);
    }
    let mut next = NEXT.load(Ordering::Acquire);
    loop {
        let address = next.checked_next_multiple_of(align)?;
        let end = address.checked_add(pages.checked_add(1)?.checked_mul(PAGE_SIZE)?)?;
        if end > END {
            return None;
        }
        match NEXT.compare_exchange_weak(next, end, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(address),
            Err(now) => next = now,
        }
    }
}
