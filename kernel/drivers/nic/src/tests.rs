// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use radix_hw::{Dma, fake_memory};

use super::*;

fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * PAGE))
}

#[test]
fn slots_are_apart_and_read_back() {
    let slots = Slots::new(5, &mut alloc).unwrap();
    assert_eq!(slots.count(), 5);
    assert_eq!(slots.phys(1) - slots.phys(0), SLOT as u64);
    slots.write(3, b"frame three");
    let mut out = [0u8; 11];
    slots.read(3, &mut out);
    assert_eq!(&out, b"frame three");
    // The card sees it at the slot's physical address.
    let mut seen = [0u8; 11];
    fake_memory(slots.phys(3), &mut seen);
    assert_eq!(&seen, b"frame three");
}

#[test]
#[should_panic(expected = "slot 5 of 5")]
fn a_slot_past_the_end_panics() {
    let slots = Slots::new(5, &mut alloc).unwrap();
    slots.phys(5);
}

#[test]
fn slots_need_their_memory() {
    assert!(Slots::new(4, &mut |_| None).is_none());
    // Memory shorter than asked for is refused, too.
    assert!(Slots::new(4, &mut |_| Some(Dma::fake(PAGE))).is_none());
}

#[test]
fn waiting_looks_once_more_at_the_end() {
    let mut waited = 0;
    let mut looks = 0;
    let done = wait_for(&mut |us| waited += us, 100, 30, || {
        looks += 1;
        looks == 5
    });
    assert!(done);
    assert_eq!(waited, 120);
    let mut waited = 0;
    assert!(!wait_for(&mut |us| waited += us, 100, 50, || false));
    assert_eq!(waited, 100);
}

#[test]
fn macs() {
    assert!(usable_mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]));
    assert!(!usable_mac([0; 6]));
    assert!(!usable_mac([0xff; 6]));
    assert!(!usable_mac([0x01, 0, 0x5e, 0, 0, 1]));
    let mac = local_mac([0xff; 6]);
    assert_eq!(mac[0], 0xfe);
    assert!(usable_mac(mac));
}

#[test]
fn rings_wrap() {
    assert_eq!(next(0, 4), 1);
    assert_eq!(next(3, 4), 0);
}
