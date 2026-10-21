// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Kani proof harnesses: for every info word, every address range and
//! every sequence of ring operations within the bounds given, whatever the
//! program writes. `ob test kani` runs them; `docs/design/ipc.md` lists
//! what each establishes.

use radix_abi::address_space::{PAGE_SIZE, USER_START, USER_TOP};
use radix_abi::ring::{Cqe, Sqe};

use crate::info::Info;
use crate::range::{user_bytes, user_pages};
use crate::ring::{Layout, Queues};

/// An info word the kernel accepts encodes back to itself, and never
/// claims more than 8 words or more capabilities than words.
#[kani::proof]
fn info_words_are_exact() {
    let word: u64 = kani::any();
    if let Ok(info) = Info::decode(word) {
        assert!(info.encode() == word);
        assert!(info.words <= 8 && info.caps <= 4 && info.caps <= info.words);
        let delivered = Info::decode(info.delivered());
        assert!(delivered.is_ok());
    }
}

/// A user range the kernel accepts lies wholly below USER_TOP.
#[kani::proof]
fn user_bytes_stay_below_the_top() {
    let (address, len): (u64, u64) = (kani::any(), kani::any());
    if len > 0 && user_bytes(address, len) {
        assert!(address < USER_TOP);
        assert!(address.checked_add(len).is_some_and(|end| end <= USER_TOP));
    }
}

/// Pages the kernel accepts are aligned and between USER_START and
/// USER_TOP.
#[kani::proof]
fn user_pages_stay_in_the_user_half() {
    let (address, pages): (u64, u64) = (kani::any(), kani::any());
    if let Some(end) = user_pages(address, pages) {
        assert!(address >= USER_START && address.is_multiple_of(PAGE_SIZE));
        assert!(end <= USER_TOP && end > address);
        assert!(end - address == pages * PAGE_SIZE);
    }
}

/// Every entry of every ring layout lies inside its mapping, the parts
/// do not overlap, and the mapping is whole pages.
#[kani::proof]
fn ring_layouts_are_sound() {
    let log2: u32 = kani::any_where(|&n| n <= 12);
    let layout = Layout::new(1 << log2).expect("a power of two up to the limit");
    let index: u32 = kani::any();
    assert!(layout.sqes >= 144, "after the header");
    assert!(layout.sqe(index) + Sqe::SIZE as u32 <= layout.cqes);
    assert!(layout.cqe(index) >= layout.cqes);
    assert!(layout.cqe(index) + Cqe::SIZE as u32 <= layout.size);
    assert!(layout.size.is_multiple_of(PAGE_SIZE as u32));
}

/// From any reachable state, whatever head the program wrote, taking,
/// completing and posting keep the completions unread or reserved within
/// the queue, and every index inside it.
#[kani::proof]
#[kani::unwind(5)]
fn ring_completions_never_overflow() {
    let log2: u32 = kani::any_where(|&n| n <= 3);
    let layout = Layout::new(1 << log2).expect("a power of two up to the limit");
    let mut queues = Queues::new(&layout);
    for _ in 0..4 {
        let head: u32 = kani::any();
        match kani::any::<u8>() % 3 {
            0 => {
                if let Some(index) = queues.take(head) {
                    assert!(index < layout.entries);
                    assert!(queues.unread(head) + queues.reserved() <= layout.completions);
                }
            }
            1 => {
                let before = queues.reserved();
                if let Some(index) = queues.complete() {
                    assert!(index < layout.completions);
                    assert!(queues.reserved() == before - 1);
                }
            }
            _ => {
                let room = queues.room(head);
                if let Some(index) = queues.post(head) {
                    assert!(index < layout.completions);
                    assert!(room > 0);
                }
            }
        }
        assert!(queues.reserved() <= layout.completions);
    }
}
