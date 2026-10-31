// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Two stacks on the host, joined by a simulated wire, and one on a
//! loopback link.

use std::boxed::Box;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::vec::Vec;

use smoltcp::iface::{SocketHandle, SocketStorage};
use smoltcp::socket::{icmp, tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint};

use crate::{
    ConnectError, FRAME, Ipv4Address, Ipv4Cidr, Link, Loopback, Medium, Memory, PingError,
    PingMemory, Pinger, Stack, TcpMemory, UdpMemory, parse_cidr,
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
        sent: Rc::default(),
        flushes: 0,
    };
    let b = End {
        mac: [2, 0, 0, 0, 0, 2],
        inbox: a_to_b,
        out: b_to_a,
        sent: Rc::default(),
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
    let pinger = Pinger::new(&mut a, ping_memory(), 0x1234).ok().unwrap();
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
    let pinger = Pinger::new(&mut a, ping_memory(), 7).ok().unwrap();
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
    let link = End {
        mac: [2, 0, 0, 0, 0, 9],
        inbox: Rc::default(),
        out: Rc::default(),
        sent: Rc::default(),
        flushes: 0,
    };
    let mut a = Stack::new(link, memory(), 3, 0);
    let pinger = Pinger::new(&mut a, ping_memory(), 1).ok().unwrap();
    assert_eq!(pinger.send(&mut a, Ipv4Address::new(10, 0, 0, 2), 0), Err(PingError::NoAddress));
}

#[test]
fn ignores_replies_for_other_pingers_and_garbage_frames() {
    let (mut a, mut b) = pair();
    let mine = Pinger::new(&mut a, ping_memory(), 1).ok().unwrap();
    let theirs = Pinger::new(&mut a, ping_memory(), 2).ok().unwrap();
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

/// Where each of a pinger's buffers is, and how long.
fn places(memory: &PingMemory) -> [(usize, usize); 4] {
    [
        (memory.receive_meta.as_ptr().addr(), memory.receive_meta.len()),
        (memory.receive.as_ptr().addr(), memory.receive.len()),
        (memory.transmit_meta.as_ptr().addr(), memory.transmit_meta.len()),
        (memory.transmit.as_ptr().addr(), memory.transmit.len()),
    ]
}

#[test]
fn refuses_sockets_past_its_slots_and_gives_their_memory_back() {
    let (mut a, _b) = pair();
    let mut pingers = Vec::new();
    for ident in 0..4 {
        pingers.push(Pinger::new(&mut a, ping_memory(), ident).ok().unwrap());
    }
    let memory = ping_memory();
    let lent = places(&memory);
    let back = Pinger::new(&mut a, memory, 9).err().unwrap();
    assert_eq!(places(&back), lent);
    // A slot closed is a slot free.
    pingers.pop().unwrap().close(&mut a).ok().unwrap();
    assert!(Pinger::new(&mut a, back, 9).is_ok());
}

#[test]
fn a_closed_pinger_gives_back_the_memory_it_was_lent() {
    let (mut a, mut b) = pair();
    let memory = ping_memory();
    let lent = places(&memory);
    let pinger = Pinger::new(&mut a, memory, 1).ok().unwrap();
    pinger.send(&mut a, Ipv4Address::new(10, 0, 0, 2), 1).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 5);
    assert_eq!(pinger.reply(&mut a), Some((1, Ipv4Address::new(10, 0, 0, 2))));
    let back = pinger.close(&mut a).ok().unwrap();
    assert_eq!(places(&back), lent);
    // The stack goes on without it.
    run(&mut a, &mut b, &mut now, 5);
}

#[test]
fn a_pinger_is_not_used_or_closed_on_another_stack() {
    let (mut a, mut b) = pair();
    let pinger = Pinger::new(&mut a, ping_memory(), 1).ok().unwrap();
    assert_eq!(pinger.send(&mut b, Ipv4Address::new(10, 0, 0, 1), 1), Err(PingError::NotHere));
    assert_eq!(pinger.reply(&mut b), None);
    let pinger = pinger.close(&mut b).err().unwrap();
    assert!(pinger.close(&mut a).is_ok());
    assert!(a.remove_socket(SocketHandle::default()).is_none());
}

#[test]
fn says_when_it_has_nothing_to_do() {
    let (mut a, _b) = pair();
    assert_eq!(a.poll_delay(0), None);
    let pinger = Pinger::new(&mut a, ping_memory(), 1).ok().unwrap();
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

fn tcp_memory() -> TcpMemory {
    TcpMemory { receive: leak(0u8, 4096), transmit: leak(0u8, 4096) }
}

fn udp_memory() -> UdpMemory {
    UdpMemory {
        receive_meta: leak(udp::PacketMetadata::EMPTY, 4),
        receive: leak(0u8, 2048),
        transmit_meta: leak(udp::PacketMetadata::EMPTY, 4),
        transmit: leak(0u8, 2048),
    }
}

/// Where a TCP socket's memory is, to compare with where it was lent.
fn tcp_places(memory: &TcpMemory) -> [(*const u8, usize); 2] {
    [
        (memory.receive.as_ptr(), memory.receive.len()),
        (memory.transmit.as_ptr(), memory.transmit.len()),
    ]
}

#[test]
fn makes_a_tcp_connection_and_gives_its_memory_back() {
    let (mut a, mut b) = pair();
    let server = b.add_socket(tcp_memory().socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(7).unwrap();
    let memory = tcp_memory();
    let lent = tcp_places(&memory);
    let client = a.add_socket(memory.socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2), 7), None, 49152).unwrap();
    // Connecting twice is refused while the first goes on.
    assert_eq!(
        a.connect(client, (Ipv4Address::new(10, 0, 0, 2), 7), None, 49153),
        Err(ConnectError::Open)
    );
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 10);
    let socket = a.socket_mut::<tcp::Socket<'static>>(client).unwrap();
    assert_eq!(socket.state(), tcp::State::Established);
    assert_eq!(socket.send_slice(b"hello").unwrap(), 5);
    run(&mut a, &mut b, &mut now, 10);
    let mut got = [0; 16];
    let len = b.socket_mut::<tcp::Socket<'static>>(server).unwrap().recv_slice(&mut got).unwrap();
    assert_eq!(&got[..len], b"hello");
    let peer = b.socket_mut::<tcp::Socket<'static>>(server).unwrap().remote_endpoint().unwrap();
    assert_eq!(peer, IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 1)), 49152));
    // Closed and taken out, the client's socket gives back what it had.
    a.socket_mut::<tcp::Socket<'static>>(client).unwrap().abort();
    run(&mut a, &mut b, &mut now, 10);
    assert!(a.remove_udp(client).is_none(), "a TCP socket is not taken out as UDP");
    assert_eq!(tcp_places(&a.remove_tcp(client).unwrap()), lent);
    assert!(a.remove_tcp(client).is_none());
    assert_eq!(
        a.connect(client, (Ipv4Address::new(10, 0, 0, 2), 7), None, 49152),
        Err(ConnectError::NotHere)
    );
}

#[test]
fn a_connection_to_a_port_no_one_listens_on_is_reset() {
    let (mut a, mut b) = pair();
    let client = a.add_socket(tcp_memory().socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2), 9), None, 50000).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 10);
    assert_eq!(a.socket_mut::<tcp::Socket<'static>>(client).unwrap().state(), tcp::State::Closed);
}

#[test]
fn sends_udp_datagrams_and_gives_their_memory_back() {
    let (mut a, mut b) = pair();
    let receiver = b.add_socket(udp_memory().socket()).ok().unwrap();
    b.socket_mut::<udp::Socket<'static>>(receiver).unwrap().bind(53).unwrap();
    let sender = a.add_socket(udp_memory().socket()).ok().unwrap();
    let socket = a.socket_mut::<udp::Socket<'static>>(sender).unwrap();
    socket.bind(40000).unwrap();
    let to = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)), 53);
    socket.send_slice(b"query", to).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 5);
    let socket = b.socket_mut::<udp::Socket<'static>>(receiver).unwrap();
    let (data, meta) = socket.recv().unwrap();
    assert_eq!(data, b"query");
    assert_eq!(meta.endpoint.port, 40000);
    let memory = b.remove_udp(receiver).unwrap();
    assert_eq!(memory.receive.len(), 2048);
    assert_eq!(memory.receive_meta.len(), 4);
    assert!(b.remove_udp(receiver).is_none());
}

#[test]
fn knows_its_addresses_and_routes() {
    let (mut a, _b) = pair();
    assert!(a.has_address(Ipv4Address::new(10, 0, 0, 1)));
    assert!(!a.has_address(Ipv4Address::new(10, 0, 0, 2)));
    assert!(a.routes_to(Ipv4Address::new(10, 0, 0, 200)));
    assert!(!a.routes_to(Ipv4Address::new(192, 0, 2, 1)));
    assert_eq!(a.source_for(Ipv4Address::new(10, 0, 0, 2)), Some(Ipv4Address::new(10, 0, 0, 1)));
    a.set_ipv4(
        Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24),
        Some(Ipv4Address::new(10, 0, 0, 254)),
    );
    assert!(a.routes_to(Ipv4Address::new(192, 0, 2, 1)));
    assert!(!a.reaches(Ipv4Address::new(192, 0, 2, 1)));
    assert_eq!(a.mtu(), 1500);
    // The loopback interface's way: several addresses, no router.
    let own = Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 32);
    a.set_ipv4_addresses(&[Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8), own]);
    assert!(a.has_address(Ipv4Address::new(127, 0, 0, 1)) && a.has_address(own.address()));
    assert!(!a.routes_to(Ipv4Address::new(192, 0, 2, 1)));
    assert!(a.reaches(Ipv4Address::new(127, 3, 0, 1)) && a.reaches(own.address()));
    assert_eq!(a.source_for(own.address()), Some(own.address()));
    assert_eq!(a.ipv4(), Some(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8)));
}

/// A stack on a loopback link, at 127.0.0.1/8.
fn loopback() -> Stack<Loopback> {
    let memory = Memory {
        receive: leak(0u8, 4096),
        transmit: leak(0u8, 4096),
        sockets: Box::leak(Box::new([SocketStorage::EMPTY; 4])),
    };
    let mut stack = Stack::new(Loopback::new(leak(0u8, 16384), 4096), memory, 3, 0);
    stack.set_ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8), None);
    stack
}

#[test]
fn the_loopback_link_gives_back_what_is_sent_in_order() {
    let mut link = Loopback::new(leak(0u8, 64), 30);
    assert_eq!(link.medium(), Medium::Ip);
    assert!(!link.pending() && link.can_send());
    let mut frame = [0u8; 40];
    for round in 0..10u8 {
        // Round the ring's end and back.
        assert!(link.send(&[round; 30]));
        assert!(link.send(&[round + 100; 5]));
        assert!(!link.can_send(), "two packets leave no room for a third of 30 bytes");
        assert!(!link.send(&[0; 30]));
        assert_eq!(link.receive(&mut frame), Some(30));
        assert_eq!(frame[..30], [round; 30]);
        assert_eq!(link.receive(&mut frame[..3]), Some(3), "a short buffer takes what fits");
        assert_eq!(frame[..3], [round + 100; 3]);
        assert_eq!(link.receive(&mut frame), None);
    }
    assert!(!link.send(&[0; 31]), "longer than its MTU");
    assert!(!link.needs_checksums(), "its packets never leave memory");
}

/// Takes the loopback link's oldest packet and checks it is `bytes`.
fn take(link: &mut Loopback, bytes: &[u8]) {
    let (oldest, _) = link.oldest().unwrap();
    oldest.read(|packet| assert_eq!(packet, bytes));
}

#[test]
fn the_loopback_ring_wraps_round_its_end() {
    // Slots of 48 bytes for packets of 40; the second ring has no room
    // after its last slot for a header that says it wraps.
    for len in [128, 100] {
        let mut link = Loopback::new(leak(0u8, len), 40);
        assert!(link.send(&[1; 40]) && link.send(&[2; 40]));
        assert!(!link.send(&[0; 25]), "no room for a third slot");
        take(&mut link, &[1; 40]);
        // The next goes at the start, before the oldest packet.
        assert!(link.send(&[3; 40]));
        assert!(!link.send(&[0; 1]), "the ring is full again");
        take(&mut link, &[2; 40]);
        take(&mut link, &[3; 40]);
        assert!(!link.pending());
        // Empty, it has all its room again.
        assert!(link.send(&[4; 40]) && link.send(&[5; 40]));
        take(&mut link, &[4; 40]);
        take(&mut link, &[5; 40]);
    }
    // Slots that wrapped fill the ring up to where the last packet before
    // the end was: the room after it, past the wrap, is free all the same.
    let mut link = Loopback::new(leak(0u8, 128), 40);
    assert!(link.send(&[1; 40]) && link.send(&[2; 40]));
    take(&mut link, &[1; 40]);
    assert!(link.send(&[3; 40]));
    take(&mut link, &[2; 40]);
    assert!(link.send(&[4; 40]), "after the packet that wrapped");
    assert!(link.send(&[5; 16]), "after the wrap");
    for (fill, len) in [(3, 40), (4, 40), (5, 16)] {
        take(&mut link, &std::vec![fill; len]);
    }
    assert_eq!(Loopback::ring_for(2, 40), 96);
    assert_eq!(Loopback::ring_for(2, 65_535), 2 * (8 + 65_536));
}

#[test]
fn an_answer_goes_in_place_beside_the_packet_it_answers() {
    // Slots of 48 bytes for packets of 40.
    let mut link = Loopback::new(leak(0u8, 128), 40);
    let mut spare = [0u8; 40];
    // After the packet answered, at the tail.
    assert!(link.send(&[1; 40]));
    let (oldest, answer) = link.oldest().unwrap();
    answer.send_with(40, &mut spare, |slot| slot.fill(9));
    oldest.read(|packet| assert_eq!(packet, &[1; 40], "the answer is beside it"));
    take(&mut link, &[9; 40]);
    // Before it, at the start, when the tail is too near the end.
    assert!(link.send(&[1; 40]) && link.send(&[2; 40]));
    take(&mut link, &[1; 40]);
    let (oldest, answer) = link.oldest().unwrap();
    answer.send_with(40, &mut spare, |slot| slot.fill(8));
    oldest.read(|packet| assert_eq!(packet, &[2; 40]));
    take(&mut link, &[8; 40]);
    assert!(!link.pending());
    // With no room, it is written to the spare buffer, and lost.
    assert!(link.send(&[1; 40]) && link.send(&[2; 40]));
    let (oldest, answer) = link.oldest().unwrap();
    answer.send_with(40, &mut spare, |slot| slot.fill(5));
    oldest.read(|packet| assert_eq!(packet, &[1; 40]));
    assert_eq!(spare, [5; 40]);
    take(&mut link, &[2; 40]);
    assert!(!link.pending(), "the answer that had no room is lost");

    // At a tail that came round already, before the packet answered: a
    // ring of two slots, the second taken, whose first holds a packet of
    // 16 bytes and has room for one of 8 after it.
    let mut link = Loopback::new(leak(0u8, 96), 40);
    assert!(link.send(&[1; 40]) && link.send(&[2; 40]));
    take(&mut link, &[1; 40]);
    assert!(link.send(&[3; 16]));
    let (oldest, answer) = link.oldest().unwrap();
    answer.send_with(8, &mut spare, |slot| slot.fill(4));
    oldest.read(|packet| assert_eq!(packet, &[2; 40]));
    take(&mut link, &[3; 16]);
    take(&mut link, &[4; 8]);
    assert!(!link.pending());
}

#[test]
fn the_loopback_ring_keeps_packets_whole_and_in_order() {
    // Packets and answers of random lengths, against a queue: each comes
    // back whole and in order, and an empty ring always has room.
    let mut link = Loopback::new(leak(0u8, 200), 60);
    let mut queue = VecDeque::new();
    let mut spare = [0u8; 60];
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut random = |below: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        usize::try_from(seed % below).unwrap()
    };
    for round in 0..100_000usize {
        let fill = u8::try_from(round % 251).unwrap();
        let len = 1 + random(60);
        match random(3) {
            0 => {
                let empty = queue.is_empty();
                if link.send(&std::vec![fill; len]) {
                    queue.push_back(std::vec![fill; len]);
                } else {
                    assert!(!empty, "an empty ring has room for any packet");
                }
            }
            1 => {
                let Some((oldest, answer)) = link.oldest() else {
                    assert!(queue.is_empty());
                    continue;
                };
                spare.fill(0);
                answer.send_with(len, &mut spare, |slot| slot.fill(fill.wrapping_add(1)));
                let expected = queue.pop_front().unwrap();
                oldest.read(|packet| assert_eq!(packet, &expected[..]));
                if spare[0] == 0 {
                    queue.push_back(std::vec![fill.wrapping_add(1); len]);
                }
            }
            _ => match queue.pop_front() {
                Some(expected) => take(&mut link, &expected),
                None => assert!(link.oldest().is_none()),
            },
        }
        assert_eq!(link.pending(), !queue.is_empty());
    }
}

#[test]
fn tcp_and_udp_go_through_the_loopback_link_in_one_poll() {
    let mut lo = loopback();
    let local = Ipv4Address::new(127, 0, 0, 1);
    let server = lo.add_socket(tcp_memory().socket()).ok().unwrap();
    lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(80).unwrap();
    let client = lo.add_socket(tcp_memory().socket()).ok().unwrap();
    lo.connect(client, (local, 80), None, 49152).unwrap();
    assert_eq!(lo.poll_delay(0), Some(0));
    // The SYN, its answer and the last ACK of the handshake, in one call.
    lo.poll(1_000_000);
    assert_eq!(
        lo.socket_mut::<tcp::Socket<'static>>(client).unwrap().state(),
        tcp::State::Established
    );
    assert_eq!(
        lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().state(),
        tcp::State::Established
    );
    lo.socket_mut::<tcp::Socket<'static>>(client).unwrap().send_slice(&[7; 3000]).unwrap();
    lo.poll(2_000_000);
    let mut got = [0u8; 4096];
    let len = lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().recv_slice(&mut got).unwrap();
    assert_eq!(&got[..len], &[7; 3000][..]);
    assert_eq!(lo.mtu(), 4096);

    let udp = lo.add_socket(udp_memory().socket()).ok().unwrap();
    let socket = lo.socket_mut::<udp::Socket<'static>>(udp).unwrap();
    socket.bind(5353).unwrap();
    socket.send_slice(b"to myself", IpEndpoint::new(IpAddress::Ipv4(local), 5353)).unwrap();
    lo.poll(3_000_000);
    let socket = lo.socket_mut::<udp::Socket<'static>>(udp).unwrap();
    assert_eq!(socket.recv().unwrap().0, b"to myself");
    assert!(!lo.link().pending());
}

#[test]
fn a_connection_on_the_loopback_link_ends_without_waiting() {
    let mut lo = loopback();
    let local = Ipv4Address::new(127, 0, 0, 1);
    let server = lo.add_socket(tcp_memory().socket()).ok().unwrap();
    lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(80).unwrap();
    let client = lo.add_socket(tcp_memory().socket()).ok().unwrap();
    lo.connect(client, (local, 80), None, 49152).unwrap();
    lo.poll(1_000_000);
    // The client closes first; the server sends its last bytes, which the
    // client's delayed-ACK timer waits to acknowledge, then closes. Its FIN
    // is acknowledged at once all the same, as Linux does, so the server is
    // not left in LAST-ACK for the delay and the client's TIME-WAIT owes
    // nothing.
    lo.socket_mut::<tcp::Socket<'static>>(client).unwrap().close();
    lo.poll(1_000_000);
    lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().send_slice(b"bye").unwrap();
    lo.poll(1_000_000);
    lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().close();
    lo.poll(1_000_000);
    assert_eq!(
        lo.socket_mut::<tcp::Socket<'static>>(client).unwrap().state(),
        tcp::State::TimeWait
    );
    assert_eq!(lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().state(), tcp::State::Closed);
    assert!(!lo.link().pending());
}
