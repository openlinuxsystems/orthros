// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The program's free capability slots.
//!
//! The slots radix-startup(2) gives a meaning (up to the cards' from
//! SLOT_DEVICE) are the program's; the rest of its capability space is handed out here,
//! one slot at a time, for the capabilities the runtime and the program
//! make.

use core::sync::atomic::{AtomicU64, Ordering};

use radix_abi::startup;

/// The first slot handed out: the one after the cards'.
pub const FIRST: u32 = startup::SLOT_DEVICE + startup::DEVICE_SLOTS;

const WORDS: usize = startup::SLOTS as usize / 64;

/// One bit per slot, set while the slot is taken.
static TAKEN: [AtomicU64; WORDS] = [const { AtomicU64::new(0) }; WORDS];

/// Takes a free slot, if there is one.
pub fn take() -> Option<u32> {
    for slot in FIRST..startup::SLOTS {
        let (word, bit) = (slot as usize / 64, 1u64 << (slot % 64));
        if TAKEN[word].fetch_or(bit, Ordering::AcqRel) & bit == 0 {
            return Some(slot);
        }
    }
    None
}

/// Gives back a slot from [`take`], which must be empty again.
pub fn give_back(slot: u32) {
    if (FIRST..startup::SLOTS).contains(&slot) {
        let (word, bit) = (slot as usize / 64, 1u64 << (slot % 64));
        TAKEN[word].fetch_and(!bit, Ordering::AcqRel);
    }
}
