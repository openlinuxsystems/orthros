// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::cell::RefCell;
use std::collections::VecDeque;

use super::*;

/// The shared memory, in one thread.
struct Shared(RefCell<Vec<u8>>);

impl Shared {
    fn new() -> Shared {
        let shared = Shared(RefCell::new(vec![0; SIZE]));
        init(&shared);
        shared
    }
}

impl Memory for Shared {
    fn load(&self, offset: usize) -> u32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= HEADER);
        let bytes = self.0.borrow();
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn store(&self, offset: usize, value: u32) {
        assert!(offset.is_multiple_of(4) && offset + 4 <= HEADER);
        self.0.borrow_mut()[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn read(&self, offset: usize, out: &mut [u8]) {
        out.copy_from_slice(&self.0.borrow()[offset..offset + out.len()]);
    }

    fn write(&self, offset: usize, data: &[u8]) {
        self.0.borrow_mut()[offset..offset + data.len()].copy_from_slice(data);
    }
}

fn frame(n: u32, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u32).wrapping_mul(31).wrapping_add(n) as u8).collect()
}

#[test]
fn the_header_says_what_the_layout_is() {
    let shared = Shared::new();
    assert!(valid(&shared));
    shared.store(at::SLOT, 4096);
    assert!(!valid(&shared));
    // The rings start empty and the link down.
    let mut stack = Stack::new();
    let mut buffer = [0u8; SLOT];
    assert_eq!(stack.receive(&shared, &mut buffer), None);
    assert_eq!(stack.link(&shared), Link::default());
    assert_eq!(SIZE, 4096 + 1024 * 1024);
}

#[test]
fn frames_go_both_ways_in_order() {
    let shared = Shared::new();
    let (mut stack, mut driver) = (Stack::new(), Driver::new());
    for n in 0..3 {
        assert!(driver.received(&shared, &frame(n, 60 + n as usize)));
    }
    let mut buffer = [0u8; SLOT];
    for n in 0..3 {
        let len = stack.receive(&shared, &mut buffer).unwrap();
        assert_eq!(&buffer[..len], &frame(n, 60 + n as usize)[..]);
    }
    assert_eq!(stack.receive(&shared, &mut buffer), None);

    assert!(stack.send(&shared, &frame(7, 1514)));
    assert!(stack.send(&shared, &frame(8, 42)));
    // Unseen until the stack flushes.
    assert!(!driver.sending(&shared));
    assert!(stack.flush(&shared));
    assert!(!stack.flush(&shared), "nothing new to tell");
    assert_eq!(driver.take(&shared, &mut buffer), Some(1514));
    assert_eq!(&buffer[..1514], &frame(7, 1514)[..]);
    assert_eq!(driver.take(&shared, &mut buffer), Some(42));
    assert_eq!(&buffer[..42], &frame(8, 42)[..]);
    assert_eq!(driver.take(&shared, &mut buffer), None);
}

#[test]
fn a_full_ring_takes_no_more_until_the_other_side_empties_it() {
    let shared = Shared::new();
    let (mut stack, mut driver) = (Stack::new(), Driver::new());
    for n in 0..ENTRIES {
        assert!(driver.received(&shared, &frame(n, 64)));
    }
    assert!(!driver.can_receive(&shared));
    assert!(!driver.received(&shared, &frame(0, 64)), "lost, as on a wire");
    let mut buffer = [0u8; SLOT];
    assert!(stack.receive(&shared, &mut buffer).is_some());
    assert!(driver.received(&shared, &frame(99, 64)), "a slot came back");

    for n in 0..ENTRIES {
        assert!(stack.send(&shared, &frame(n, 64)), "frame {n}");
    }
    assert!(!stack.can_send(&shared));
    assert!(!stack.send(&shared, &frame(0, 64)));
    stack.flush(&shared);
    assert_eq!(driver.take(&shared, &mut buffer), Some(64));
    assert!(stack.can_send(&shared));
}

#[test]
fn the_indexes_wrap_around() {
    let shared = Shared::new();
    let start = u32::MAX - 3;
    let (mut stack, mut driver) = (Stack::new(), Driver::new());
    stack.received = start;
    stack.sending = start;
    driver.received = start;
    driver.sending = start;
    for name in [at::RECEIVED_TAIL, at::RECEIVED_HEAD, at::SENDING_TAIL, at::SENDING_HEAD] {
        shared.store(name, start);
    }
    let mut buffer = [0u8; SLOT];
    for n in 0..10 {
        assert!(driver.received(&shared, &frame(n, 100)));
        assert!(stack.send(&shared, &frame(n + 50, 200)));
        stack.flush(&shared);
        assert_eq!(stack.receive(&shared, &mut buffer), Some(100));
        assert_eq!(&buffer[..100], &frame(n, 100)[..]);
        assert_eq!(driver.take(&shared, &mut buffer), Some(200));
        assert_eq!(&buffer[..200], &frame(n + 50, 200)[..]);
    }
    assert!(!stack.broken() && !driver.broken());
}

#[test]
fn a_driver_that_runs_its_tail_past_the_ring_breaks_the_link() {
    let shared = Shared::new();
    let mut stack = Stack::new();
    shared.store(at::RECEIVED_TAIL, ENTRIES + 1);
    let mbps: u32 = 1000;
    shared.store(at::LINK, UP | FULL_DUPLEX | mbps);
    let mut buffer = [0u8; SLOT];
    assert_eq!(stack.receive(&shared, &mut buffer), None);
    assert!(stack.broken());
    // Nothing more goes either way, and the link reads as down.
    assert!(!stack.can_send(&shared));
    assert!(!stack.send(&shared, &frame(0, 64)));
    assert_eq!(stack.link(&shared), Link::default());
}

#[test]
fn a_driver_that_moves_its_head_past_the_frames_given_breaks_the_link() {
    let shared = Shared::new();
    let mut stack = Stack::new();
    assert!(stack.send(&shared, &frame(0, 64)));
    stack.flush(&shared);
    // It says it took two.
    shared.store(at::SENDING_HEAD, 2);
    assert!(!stack.can_send(&shared));
    assert!(stack.broken());
}

#[test]
fn a_tail_behind_the_head_breaks_the_link() {
    let shared = Shared::new();
    let mut stack = Stack::new();
    shared.store(at::RECEIVED_TAIL, u32::MAX);
    assert_eq!(stack.receive(&shared, &mut [0u8; SLOT]), None);
    assert!(stack.broken());
}

#[test]
fn lengths_that_are_no_frames_are_dropped() {
    let shared = Shared::new();
    let (mut stack, mut driver) = (Stack::new(), Driver::new());
    // Written as a driver would, then given lengths that are no frame's.
    for (n, len) in [0, 5, 3000, u32::MAX, 60].into_iter().enumerate() {
        assert!(driver.received(&shared, &frame(n as u32, 60)));
        shared.store(at::RECEIVED_LENGTHS + 4 * n, len);
    }
    let mut buffer = [0u8; SLOT];
    assert_eq!(stack.receive(&shared, &mut buffer), Some(60));
    assert_eq!(&buffer[..60], &frame(4, 60)[..]);
    assert_eq!(stack.dropped(), 4);
    assert_eq!(stack.receive(&shared, &mut buffer), None);
    assert!(!stack.broken());
}

#[test]
fn a_frame_longer_than_the_buffer_is_cut_to_it() {
    let shared = Shared::new();
    let (mut stack, mut driver) = (Stack::new(), Driver::new());
    assert!(driver.received(&shared, &frame(1, SLOT)));
    let mut small = [0u8; 100];
    assert_eq!(stack.receive(&shared, &mut small), Some(100));
    assert_eq!(&small[..], &frame(1, SLOT)[..100]);
}

#[test]
fn the_stack_sends_only_frames() {
    let shared = Shared::new();
    let mut stack = Stack::new();
    assert!(!stack.send(&shared, &frame(0, MIN_FRAME - 1)));
    assert!(!stack.send(&shared, &frame(0, MAX_FRAME + 1)));
    assert!(!stack.flush(&shared));
    // And the driver takes only what is in a slot, whatever the length.
    let mut driver = Driver::new();
    assert!(stack.send(&shared, &frame(0, 64)));
    stack.flush(&shared);
    shared.store(at::SENDING_LENGTHS, u32::MAX);
    let mut buffer = vec![0u8; 4 * SLOT];
    assert_eq!(driver.take(&shared, &mut buffer), Some(SLOT));
}

#[test]
fn the_link_word_carries_the_cards_link() {
    let shared = Shared::new();
    let (stack, driver) = (Stack::new(), Driver::new());
    let gigabit = Link { up: true, mbps: 1000, full_duplex: true };
    driver.set_link(&shared, gigabit);
    assert_eq!(stack.link(&shared), gigabit);
    let half = Link { up: true, mbps: 10, full_duplex: false };
    driver.set_link(&shared, half);
    assert_eq!(stack.link(&shared), half);
    driver.set_link(&shared, Link { up: false, mbps: 1000, full_duplex: true });
    assert_eq!(stack.link(&shared), Link::default());
    driver.set_link(&shared, Link { up: true, mbps: u32::MAX, full_duplex: false });
    assert_eq!(stack.link(&shared).mbps, MBPS);
}

/// Both sides at random against a model: every frame arrives once, in
/// order, as it was, and neither side ever sees the link broken.
#[test]
fn many_frames_at_random_arrive_as_sent() {
    let shared = Shared::new();
    let (mut stack, mut driver) = (Stack::new(), Driver::new());
    let (mut up, mut down) = (VecDeque::new(), VecDeque::new());
    let mut unflushed = VecDeque::new();
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut buffer = [0u8; SLOT];
    for step in 0..20_000u32 {
        let len = MIN_FRAME + (random() % (MAX_FRAME - MIN_FRAME + 1) as u64) as usize;
        match random() % 6 {
            0 | 1 => {
                let sent = frame(step, len);
                if driver.received(&shared, &sent) {
                    up.push_back(sent);
                } else {
                    assert_eq!(up.len(), ENTRIES as usize);
                }
            }
            2 => match stack.receive(&shared, &mut buffer) {
                Some(got) => assert_eq!(&buffer[..got], &up.pop_front().unwrap()[..]),
                None => assert!(up.is_empty()),
            },
            3 => {
                let sent = frame(step, len);
                if stack.send(&shared, &sent) {
                    unflushed.push_back(sent);
                } else {
                    assert_eq!(down.len() + unflushed.len(), ENTRIES as usize);
                }
            }
            4 => {
                assert_eq!(stack.flush(&shared), !unflushed.is_empty());
                down.append(&mut unflushed);
            }
            _ => match driver.take(&shared, &mut buffer) {
                Some(got) => assert_eq!(&buffer[..got], &down.pop_front().unwrap()[..]),
                None => assert!(down.is_empty()),
            },
        }
    }
    assert!(!stack.broken() && !driver.broken());
    assert_eq!(stack.dropped(), 0);
}
