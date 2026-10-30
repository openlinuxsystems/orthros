// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Two stacks on the host, joined by a simulated wire.

use std::boxed::Box;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::vec::Vec;

use smoltcp::iface::SocketStorage;
use smoltcp::socket::icmp;

use crate::{
    FRAME, Ipv4Address, Ipv4Cidr, Link, Memory, PingError, PingMemory, Pinger, Stack, parse_cidr,
};

type Queue = Rc<RefCell<VecDeque<Vec<u8>>>>;

/// One end of the wire: frames sent go into `out`, frames received come
/// from `inbox`.
struct End {
    mac: [u8; 6],
    inbox: Queue,
    out: Queue,
    /// Every frame this end sent, for the tests to look at.
    sent: Rc<RefCell<Vec<Vec<u8>>>>,
    flushes: usize,
}

impl Link for End {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        let got = self.inbox.borrow_mut().pop_front()?;
        let len = got.len().min(frame.len());
        frame[..len].copy_from_slice(&got[..len]);
        Some(len)
    }

    fn can_send(&mut self) -> bool {
        true
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        self.sent.borrow_mut().push(frame.to_vec());
        self.out.borrow_mut().push_back(frame.to_vec());
        true
    }

    fn flush(&mut self) {
        self.flushes += 1;
    }
}

fn leak<T: Clone>(value: T, len: usize) -> &'static mut [T] {
    Box::leak(std::vec![value; len].into_boxed_slice())
}

fn memory() -> Memory {
    Memory {
        receive: leak(0u8, FRAME),
        transmit: leak(0u8, FRAME),
        sockets: Box::leak(Box::new([SocketStorage::EMPTY; 4])),
    }
}

fn ping_memory() -> PingMemory {
    PingMemory {
        receive_meta: leak(icmp::PacketMetadata::EMPTY, 4),
        receive: leak(0u8, 256),
        transmit_meta: leak(icmp::PacketMetadata::EMPTY, 4),
        transmit: leak(0u8, 256),
    }
}

/// Two stacks on one wire, at 10.0.0.1 and 10.0.0.2, and what each sent.
fn pair() -> (Stack<End>, Stack<End>) {
    let (a_to_b, b_to_a): (Queue, Queue) = Default::default();
    let a = End {
        mac: [2, 0, 0, 0, 0, 1],
        inbox: b_to_a.clone(),
        out: a_to_b.clone(),
        sent: Default::default(),
        flushes: 0,
    };
    let b = End {
        mac: [2, 0, 0, 0, 0, 2],
        inbox: a_to_b,
        out: b_to_a,
        sent: Default::default(),
        flushes: 0,
    };
    let mut a = Stack::new(a, memory(), 1, 0);
    let mut b = Stack::new(b, memory(), 2, 0);
    a.set_ipv4(Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24), None);
    b.set_ipv4(Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 2), 24), None);
    (a, b)
}

/// Polls both stacks, a millisecond apart, until neither has anything to
/// do or `rounds` pass.
fn run(a: &mut Stack<End>, b: &mut Stack<End>, now: &mut u64, rounds: usize) {
    for _ in 0..rounds {
        *now += 1_000_000;
        a.poll(*now);
        b.poll(*now);
    }
}

#[test]
fn pings_the_other_stack_which_answers() {
    let (mut a, mut b) = pair();
    let pinger = Pinger::new(&mut a, ping_memory(), 0x1234).unwrap();
    let mut now = 0;
    for seq in 0..3 {
        pinger.send(&mut a, Ipv4Address::new(10, 0, 0, 2), seq).unwrap();
        run(&mut a, &mut b, &mut now, 5);
        assert_eq!(pinger.reply(&mut a), Some((seq, Ipv4Address::new(10, 0, 0, 2))));
        assert_eq!(pinger.reply(&mut a), None);
    }
    // The first request waited for ARP; the stack flushed its card after
    // each poll.
    let sent = a.link().sent.borrow().clone();
    assert_eq!(&sent[0][12..14], &[0x08, 0x06], "an ARP request first");
    assert!(a.link().flushes >= 15);
}

#[test]
fn sends_to_an_address_off_the_link_through_the_gateway() {
    let (mut a, mut b) = pair();
    a.set_ipv4(
        Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24),
        Some(Ipv4Address::new(10, 0, 0, 2)),
    );
    let pinger = Pinger::new(&mut a, ping_memory(), 7).unwrap();
    pinger.send(&mut a, Ipv4Address::new(192, 0, 2, 9), 1).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 5);
    let sent = a.link().sent.borrow().clone();
    // ARP for the gateway, then the request to the gateway's card.
    assert_eq!(&sent[0][12..14], &[0x08, 0x06]);
    assert_eq!(&sent[0][38..42], &[10, 0, 0, 2], "asks who has the gateway");
    let request = sent.iter().find(|f| f[12..14] == [0x08, 0x00]).expect("an IPv4 frame");
    assert_eq!(&request[0..6], &[2, 0, 0, 0, 0, 2], "to the gateway's card");
    assert_eq!(&request[30..34], &[192, 0, 2, 9], "for the far address");
    // The gateway does not forward it: no reply comes.
    assert_eq!(pinger.reply(&mut a), None);
}

#[test]
fn an_unconfigured_stack_cannot_ping() {
    let (a_link, _) = (
        End {
            mac: [2, 0, 0, 0, 0, 9],
            inbox: Default::default(),
            out: Default::default(),
            sent: Default::default(),
            flushes: 0,
        },
        (),
    );
    let mut a = Stack::new(a_link, memory(), 3, 0);
    let pinger = Pinger::new(&mut a, ping_memory(), 1).unwrap();
    assert_eq!(pinger.send(&mut a, Ipv4Address::new(10, 0, 0, 2), 0), Err(PingError::NoAddress));
}

#[test]
fn ignores_replies_for_other_pingers_and_garbage_frames() {
    let (mut a, mut b) = pair();
    let mine = Pinger::new(&mut a, ping_memory(), 1).unwrap();
    let theirs = Pinger::new(&mut a, ping_memory(), 2).unwrap();
    // Garbage on the wire first: too short, and a bad IPv4 packet.
    a.link().inbox.borrow_mut().push_back(std::vec![0xff; 10]);
    let mut bad = std::vec![0u8; 60];
    bad[12..14].copy_from_slice(&[0x08, 0x00]);
    bad[14] = 0x45;
    a.link().inbox.borrow_mut().push_back(bad);
    theirs.send(&mut a, Ipv4Address::new(10, 0, 0, 2), 5).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 5);
    assert_eq!(mine.reply(&mut a), None);
    assert_eq!(theirs.reply(&mut a), Some((5, Ipv4Address::new(10, 0, 0, 2))));
}

#[test]
fn refuses_sockets_past_its_slots() {
    let (mut a, _b) = pair();
    for ident in 0..4 {
        assert!(Pinger::new(&mut a, ping_memory(), ident).is_some());
    }
    assert!(Pinger::new(&mut a, ping_memory(), 9).is_none());
}

#[test]
fn says_when_it_has_nothing_to_do() {
    let (mut a, _b) = pair();
    assert_eq!(a.poll_delay(0), None);
    let pinger = Pinger::new(&mut a, ping_memory(), 1).unwrap();
    pinger.send(&mut a, Ipv4Address::new(10, 0, 0, 2), 0).unwrap();
    assert_eq!(a.poll_delay(0), Some(0));
}

#[test]
fn reads_addresses_with_their_prefixes() {
    let cidr = |a, b, c, d, p| Some(Ipv4Cidr::new(Ipv4Address::new(a, b, c, d), p));
    assert_eq!(parse_cidr("10.0.2.15/24"), cidr(10, 0, 2, 15, 24));
    assert_eq!(parse_cidr("192.0.2.1"), cidr(192, 0, 2, 1, 32));
    assert_eq!(parse_cidr("0.0.0.0/0"), cidr(0, 0, 0, 0, 0));
    for bad in ["10.0.2.15/33", "10.0.2/24", "10.0.2.15/", "/24", "", "10.0.2.256", "a.b.c.d"] {
        assert_eq!(parse_cidr(bad), None, "{bad}");
    }
}
