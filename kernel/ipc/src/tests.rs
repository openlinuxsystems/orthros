// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use proptest::prelude::*;
use radix_abi::address_space::{USER_START, USER_TOP};
use radix_abi::endpoint::{MessageFlags, RESERVED_LABELS};
use radix_abi::ring::{Cqe, ENTRIES_MAX, Sqe};

use crate::info::{Info, InfoError};
use crate::notify;
use crate::range::{user_bytes, user_pages};
use crate::ring::{Layout, Queues};

#[test]
fn info_words_round_trip() {
    let info = Info { label: 0x1234, words: 5, caps: 2, move_caps: true, no_block: false };
    assert_eq!(Info::decode(info.encode()), Ok(info));
    assert_eq!(info.encode() & MessageFlags::MOVE_CAPS.bits(), MessageFlags::MOVE_CAPS.bits());
    let delivered = Info::decode(info.delivered()).unwrap();
    assert!(!delivered.move_caps && delivered.caps == 2 && delivered.words == 5);
}

#[test]
fn info_words_are_checked() {
    assert_eq!(Info::decode(9 << 32), Err(InfoError::TooManyWords));
    assert_eq!(Info::decode((8 << 32) | (5 << 36)), Err(InfoError::TooManyCaps));
    assert_eq!(Info::decode((1 << 32) | (2 << 36)), Err(InfoError::CapsBeyondWords));
    assert_eq!(Info::decode(1 << 39), Err(InfoError::UnknownBits));
    assert_eq!(Info::decode(1 << 63), Err(InfoError::UnknownBits));
    assert!(Info::decode(RESERVED_LABELS).unwrap().is_reserved());
    assert!(!Info::decode(RESERVED_LABELS - 1).unwrap().is_reserved());
}

#[test]
fn user_ranges_stay_in_the_lower_half() {
    assert!(user_bytes(0, 0));
    assert!(user_bytes(u64::MAX, 0), "an empty range touches nothing");
    assert!(user_bytes(USER_TOP - 8, 8));
    assert!(!user_bytes(USER_TOP - 8, 9));
    assert!(!user_bytes(u64::MAX - 3, 8), "no wrapping around");
    assert!(!user_bytes(0xffff_8000_0000_0000, 1));
    assert_eq!(user_pages(USER_START, 1), Some(USER_START + 4096));
    assert_eq!(user_pages(USER_START - 4096, 1), None, "the low pages stay unmapped");
    assert_eq!(user_pages(USER_START + 1, 1), None, "not page-aligned");
    assert_eq!(user_pages(USER_TOP - 4096, 1), Some(USER_TOP));
    assert_eq!(user_pages(USER_TOP - 4096, 2), None);
    assert_eq!(user_pages(USER_START, u64::MAX), None);
    assert_eq!(user_pages(USER_START, 0), None);
}

#[test]
fn notifications_always_leave_a_bit() {
    assert_eq!(notify::signal(0, 0), 1);
    assert_eq!(notify::signal(0b100, 0b10), 0b110);
}

#[test]
fn ring_layouts() {
    assert_eq!(Layout::new(0), None);
    assert_eq!(Layout::new(3), None);
    assert_eq!(Layout::new(ENTRIES_MAX * 2), None);
    let small = Layout::new(1).unwrap();
    assert_eq!((small.sqes, small.cqes, small.size, small.pages()), (4096, 8192, 12288, 3));
    let big = Layout::new(ENTRIES_MAX).unwrap();
    assert_eq!(big.size, 4096 + ENTRIES_MAX * 64 + ENTRIES_MAX * 2 * 32);
    assert_eq!(big.sqe(ENTRIES_MAX), big.sqes, "indexes wrap");
    assert_eq!(big.cqe(big.completions + 1), big.cqes + 32);
}

#[test]
fn a_full_completion_queue_stops_submission() {
    let layout = Layout::new(2).unwrap();
    let mut queues = Queues::new(&layout);
    assert_eq!(queues.available(2), Ok(2));
    assert_eq!(queues.available(3), Err(crate::ring::Overrun));
    // Four completions fit; nothing is read.
    for _ in 0..4 {
        assert!(queues.take(0).is_some());
        assert!(queues.complete().is_some());
    }
    assert_eq!(queues.take(0), None, "the queue is full");
    assert_eq!(queues.post(0), None);
    assert_eq!(queues.complete(), None, "nothing reserved");
    // The program reads two.
    assert!(queues.take(2).is_some());
    assert!(queues.post(2).is_some());
    assert_eq!(queues.take(2), None, "one reserved and five written");
}

proptest! {
    #[test]
    fn decoded_info_words_encode_back(word in any::<u64>()) {
        if let Ok(info) = Info::decode(word) {
            prop_assert_eq!(info.encode(), word);
            prop_assert!(info.caps <= info.words && info.words <= 8);
        }
    }

    /// While the program reads honestly, the kernel never has more
    /// completions unread or reserved than fit, so it never writes over
    /// one still unread; whatever the program writes, every index the
    /// kernel writes at is inside the ring.
    #[test]
    fn completions_never_overflow(
        log2 in 0u32..13,
        steps in proptest::collection::vec((0u8..4, any::<u32>()), 1..200),
    ) {
        let layout = Layout::new(1 << log2).unwrap();
        let mut queues = Queues::new(&layout);
        let mut head = 0u32;
        let mut honest = true;
        for (op, value) in steps {
            match op {
                0 => {
                    // An honest program reads some completions.
                    let unread = queues.unread(head);
                    head = head.wrapping_add(value % (unread + 1));
                }
                1 => {
                    // A program that writes anything.
                    head = value;
                    honest = false;
                }
                2 => {
                    if let Some(index) = queues.take(head) {
                        prop_assert!(index < layout.entries);
                    }
                }
                _ => {
                    if let Some(index) = queues.complete().or_else(|| queues.post(head)) {
                        prop_assert!(index < layout.completions);
                        prop_assert!(layout.cqe(index) + Cqe::SIZE as u32 <= layout.size);
                    }
                }
            }
            if honest {
                prop_assert!(queues.unread(head) + queues.reserved() <= layout.completions);
            }
            prop_assert!(queues.reserved() <= layout.completions);
            prop_assert!(layout.sqe(queues.sq_head()) + Sqe::SIZE as u32 <= layout.cqes);
        }
    }

    #[test]
    fn user_bytes_never_reach_the_kernel_half(address in any::<u64>(), len in any::<u64>()) {
        if user_bytes(address, len) && len > 0 {
            prop_assert!(address < USER_TOP && address + len <= USER_TOP);
        }
    }
}
