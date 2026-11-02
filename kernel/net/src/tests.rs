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
    Across, ConnectError, FRAME, Ipv4Address, Ipv4Cidr, Link, Loopback, Medium, Memory, OtherEnd,
    PingError, PingMemory, Pinger, Stack, TcpMemory, UdpMemory, parse_cidr,
};

pub(crate) type Queue = Rc<RefCell<VecDeque<Vec<u8>>>>;

/// One end of the wire: frames sent go into `out`, frames received come
/// from `inbox`.
pub(crate) struct End {
    pub(crate) mac: [u8; 6],
    pub(crate) inbox: Queue,
    out: Queue,
    /// Every frame this end sent, for the tests to look at.
    pub(crate) sent: Rc<RefCell<Vec<Vec<u8>>>>,
    flushes: usize,
    /// Whether the stack makes and checks checksums on it.
    checksums: bool,
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

    fn needs_checksums(&self) -> bool {
        self.checksums
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
pub(crate) fn pair() -> (Stack<End>, Stack<End>) {
    wire(true)
}

/// Two stacks on one wire, as [`pair`], which make and check checksums or
/// not, as `checksums` says.
fn wire(checksums: bool) -> (Stack<End>, Stack<End>) {
    let (a_to_b, b_to_a): (Queue, Queue) = Default::default();
    let a = End {
        mac: [2, 0, 0, 0, 0, 1],
        inbox: b_to_a.clone(),
        out: a_to_b.clone(),
        sent: Rc::default(),
        flushes: 0,
        checksums,
    };
    let b = End {
        mac: [2, 0, 0, 0, 0, 2],
        inbox: a_to_b,
        out: b_to_a,
        sent: Rc::default(),
        flushes: 0,
        checksums,
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
        checksums: true,
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

pub(crate) fn tcp_memory() -> TcpMemory {
    TcpMemory { receive: leak(0u8, 4096), transmit: leak(0u8, 4096) }
}

pub(crate) fn udp_memory() -> UdpMemory {
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
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49152).unwrap();
    // Connecting twice is refused while the first goes on.
    assert_eq!(
        a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49153),
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
        a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49152),
        Err(ConnectError::NotHere)
    );
}

#[test]
fn a_connection_to_a_port_no_one_listens_on_is_reset() {
    let (mut a, mut b) = pair();
    let client = a.add_socket(tcp_memory().socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 9), None, 50000).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 10);
    assert_eq!(a.socket_mut::<tcp::Socket<'static>>(client).unwrap().state(), tcp::State::Closed);
}

#[test]
fn a_quiet_connection_stays_open_past_its_timeout() {
    // The kernel gives every connection a timeout for answers (900 s,
    // ADR 0183); a connection with nothing to answer is not waiting for
    // one, however long it is quiet, as on Linux.
    let (mut a, mut b) = pair();
    let server = b.add_socket(tcp_memory().socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(7).unwrap();
    let client = a.add_socket(tcp_memory().socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49152).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 10);
    for (stack, handle) in [(&mut a, client), (&mut b, server)] {
        let socket = stack.socket_mut::<tcp::Socket<'static>>(handle).unwrap();
        socket.set_timeout(Some(smoltcp::time::Duration::from_secs(900)));
    }
    a.socket_mut::<tcp::Socket<'static>>(client).unwrap().send_slice(b"hi").unwrap();
    run(&mut a, &mut b, &mut now, 20);
    let mut got = [0; 4];
    let len = b.socket_mut::<tcp::Socket<'static>>(server).unwrap().recv_slice(&mut got).unwrap();
    assert_eq!(&got[..len], b"hi");
    // An hour of quiet, the stacks polled every ten seconds as a card's
    // thread polls them for other sockets' work.
    for _ in 0..360 {
        now += 10_000_000_000;
        a.poll(now);
        b.poll(now);
    }
    for (stack, handle) in [(&mut a, client), (&mut b, server)] {
        let state = stack.socket_mut::<tcp::Socket<'static>>(handle).unwrap().state();
        assert_eq!(state, tcp::State::Established);
    }
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
    assert_eq!(
        a.source_for(Ipv4Address::new(10, 0, 0, 2).into()),
        Some(Ipv4Address::new(10, 0, 0, 1).into())
    );
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
    assert_eq!(a.source_for(own.address().into()), Some(own.address().into()));
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
    lo.connect(client, (local.into(), 80), None, 49152).unwrap();
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
    lo.connect(client, (local.into(), 80), None, 49152).unwrap();
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

/// A loopback stack with a connection from 127.0.0.1:49152 to a server
/// at port 80, made: the stack, the client's handle and the server's.
fn connected() -> (Stack<Loopback>, SocketHandle, SocketHandle) {
    let mut lo = loopback();
    let local = Ipv4Address::new(127, 0, 0, 1);
    let server = lo.add_socket(tcp_memory().socket()).ok().unwrap();
    lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(80).unwrap();
    let client = lo.add_socket(tcp_memory().socket()).ok().unwrap();
    lo.connect(client, (local.into(), 80), None, 49152).unwrap();
    lo.poll(1_000_000);
    for handle in [client, server] {
        let state = lo.socket_mut::<tcp::Socket<'static>>(handle).unwrap().state();
        assert_eq!(state, tcp::State::Established);
    }
    (lo, client, server)
}

/// Passes `bytes` across from `from`, all of them, or fails.
fn across(lo: &mut Stack<Loopback>, from: SocketHandle, other: &mut OtherEnd, bytes: &[u8]) {
    let passed = lo.send_across(from, other, 2_000_000, |room| {
        room[..bytes.len()].copy_from_slice(bytes);
        (bytes.len(), ())
    });
    assert_eq!(passed, Across::Passed((), bytes.len()));
}

/// Receives what `on` has, up to `len` bytes.
fn take_tcp(lo: &mut Stack<Loopback>, on: SocketHandle, len: usize) -> Vec<u8> {
    let mut got = std::vec![0u8; len];
    let len = lo.socket_mut::<tcp::Socket<'static>>(on).unwrap().recv_slice(&mut got).unwrap();
    got.truncate(len);
    got
}

#[test]
fn a_loopback_connection_passes_its_bytes_straight_across() {
    let (mut lo, client, server) = connected();
    let (mut client_end, mut server_end) = (OtherEnd::default(), OtherEnd::default());
    across(&mut lo, client, &mut client_end, b"hello");
    // No packet went, and none is due: both ends count the bytes as sent,
    // received and acknowledged.
    assert!(!lo.link().pending());
    assert_eq!(lo.poll_delay(2_000_000), None);
    assert_eq!(take_tcp(&mut lo, server, 64), b"hello");
    // Packets go between them as before, each way, their sequence numbers
    // and acknowledgements in step with what went across.
    lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().send_slice(b"world").unwrap();
    lo.poll(3_000_000);
    assert_eq!(take_tcp(&mut lo, client, 64), b"world");
    lo.socket_mut::<tcp::Socket<'static>>(client).unwrap().send_slice(b"again").unwrap();
    lo.poll(4_000_000);
    assert_eq!(take_tcp(&mut lo, server, 64), b"again");
    // Bytes go across again once what went in packets is acknowledged:
    // the server's ACK waits for its delayed-ACK timer.
    assert_eq!(lo.room_across(server, &mut server_end), Some(4096));
    assert_eq!(lo.room_across(client, &mut client_end), None);
    lo.poll(20_000_000);
    across(&mut lo, server, &mut server_end, b"back");
    across(&mut lo, client, &mut client_end, b"more");
    assert_eq!(take_tcp(&mut lo, client, 64), b"back");
    assert_eq!(take_tcp(&mut lo, server, 64), b"more");
    // The connection ends as any does, with nothing reset.
    lo.socket_mut::<tcp::Socket<'static>>(client).unwrap().close();
    lo.poll(5_000_000);
    let server_socket = lo.socket_mut::<tcp::Socket<'static>>(server).unwrap();
    assert_eq!(server_socket.recv_slice(&mut [0; 8]), Err(tcp::RecvError::Finished));
    server_socket.close();
    lo.poll(6_000_000);
    let client_state = lo.socket_mut::<tcp::Socket<'static>>(client).unwrap().state();
    assert_eq!(client_state, tcp::State::TimeWait);
    assert_eq!(lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().state(), tcp::State::Closed);
    // Neither end of a closed connection takes bytes across.
    assert_eq!(lo.room_across(client, &mut client_end), None);
}

#[test]
fn bytes_wait_for_room_across_and_the_window_goes_across_too() {
    let (mut lo, client, server) = connected();
    let (mut client_end, mut server_end) = (OtherEnd::default(), OtherEnd::default());
    assert_eq!(lo.room_across(client, &mut client_end), Some(4096));
    across(&mut lo, client, &mut client_end, &[1; 4096]);
    assert_eq!(lo.room_across(client, &mut client_end), Some(0));
    let full = lo.send_across(client, &mut client_end, 2_000_000, |_| -> (usize, ()) {
        unreachable!("no room")
    });
    assert_eq!(full, Across::Full);
    assert!(take_tcp(&mut lo, server, 1000) == [1; 1000]);
    // The window the read opened goes across, in no packet.
    assert_eq!(lo.window_across(server, &mut server_end, 3_000_000), Some(false));
    assert_eq!(lo.poll_delay(3_000_000), None);
    assert_eq!(lo.room_across(client, &mut client_end), Some(1000));
    // The room is at the buffer's start now, as the buffer is a ring.
    across(&mut lo, client, &mut client_end, &[2; 1000]);
    let mut expected = std::vec![1; 3096];
    expected.extend([2; 1000]);
    assert!(take_tcp(&mut lo, server, 4096) == expected);
    // Bytes sent in packets while the window was shut go once it opens:
    // the other end's window, told across, says so.
    across(&mut lo, client, &mut client_end, &[3; 4096]);
    let client_socket = lo.socket_mut::<tcp::Socket<'static>>(client).unwrap();
    assert_eq!(client_socket.send_slice(&[4; 100]), Ok(100));
    lo.poll(4_000_000);
    assert!(take_tcp(&mut lo, server, 4096) == [3; 4096]);
    assert_eq!(lo.window_across(server, &mut server_end, 5_000_000), Some(true));
    lo.poll(5_000_000);
    assert!(take_tcp(&mut lo, server, 4096) == [4; 100]);
}

#[test]
fn bytes_go_in_packets_while_others_are_on_their_way() {
    let (mut lo, client, server) = connected();
    let mut client_end = OtherEnd::default();
    let client_socket = lo.socket_mut::<tcp::Socket<'static>>(client).unwrap();
    client_socket.send_slice(b"first").unwrap();
    // The bytes queued for packets go first.
    assert_eq!(lo.room_across(client, &mut client_end), None);
    let packets = lo.send_across(client, &mut client_end, 2_000_000, |_| -> (usize, ()) {
        unreachable!("bytes are waiting")
    });
    assert_eq!(packets, Across::Packets);
    lo.poll(2_000_000);
    // And bytes go across again once those are acknowledged, after the
    // server's delayed-ACK timer, and while no packet is in the ring.
    assert_eq!(lo.room_across(client, &mut client_end), None);
    lo.poll(20_000_000);
    assert!(lo.link().send(&[0; 20]));
    assert_eq!(lo.room_across(client, &mut client_end), None);
    lo.poll(21_000_000);
    across(&mut lo, client, &mut client_end, b"second");
    assert_eq!(take_tcp(&mut lo, server, 64), b"firstsecond");
}

#[test]
fn a_loopback_connection_keeps_its_bytes_in_order_across_and_in_packets() {
    let (mut lo, client, server) = connected();
    let mut ends = [OtherEnd::default(), OtherEnd::default()];
    let handles = [client, server];
    // What each end has sent and received of its stream, counted, and
    // how much of what it sent went across.
    let (mut sent, mut received, mut passed) = ([0usize; 2], [0usize; 2], [0usize; 2]);
    let byte = |at: usize| (at % 251) as u8;
    let mut random = 0x2545_f491_4f6c_dd1du64;
    let mut now = 2_000_000;
    for _ in 0..20_000 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let side = (random & 1) as usize;
        let len = (random >> 8) as usize % 3000 + 1;
        // Up to 4 ms between steps: an ACK the delayed-ACK timer holds
        // goes within a few.
        now += (random >> 32) % 4_000_000;
        let from = handles[side];
        match (random >> 4) % 4 {
            0 => {
                let start = sent[side];
                let done = lo.send_across(from, &mut ends[side], now, |room| {
                    let len = len.min(room.len());
                    for (at, slot) in room[..len].iter_mut().enumerate() {
                        *slot = byte(start + at);
                    }
                    (len, ())
                });
                if let Across::Passed((), len) = done {
                    sent[side] += len;
                    passed[side] += len;
                }
            }
            1 => {
                let bytes: Vec<u8> = (sent[side]..sent[side] + len).map(byte).collect();
                let socket = lo.socket_mut::<tcp::Socket<'static>>(from).unwrap();
                sent[side] += socket.send_slice(&bytes).unwrap();
                lo.poll(now);
            }
            2 => {
                let got = take_tcp(&mut lo, from, len);
                for (at, &got) in got.iter().enumerate() {
                    assert_eq!(got, byte(received[side] + at));
                }
                received[side] += got.len();
                if random & 0x100 != 0 && lo.window_across(from, &mut ends[side], now) == Some(true)
                {
                    lo.poll(now);
                }
            }
            _ => {
                lo.poll(now);
            }
        }
        for handle in handles {
            let state = lo.socket_mut::<tcp::Socket<'static>>(handle).unwrap().state();
            assert_eq!(state, tcp::State::Established);
        }
    }
    // Everything sent comes, in order, and nothing more.
    for _ in 0..100 {
        now += 1_000_000;
        lo.poll(now);
        for side in 0..2 {
            let got = take_tcp(&mut lo, handles[side], 4096);
            for (at, &got) in got.iter().enumerate() {
                assert_eq!(got, byte(received[side] + at));
            }
            received[side] += got.len();
        }
    }
    assert_eq!(received, [sent[1], sent[0]]);
    for side in 0..2 {
        assert!(
            passed[side] > 100_000 && sent[side] - passed[side] > 100_000,
            "{sent:?} {passed:?}"
        );
    }
}

#[test]
fn bytes_go_across_only_to_the_next_byte_expected() {
    let (mut lo, client, server) = connected();
    let next = lo.socket_mut::<tcp::Socket<'static>>(client).unwrap().next_across().unwrap();
    let (ack, window) =
        lo.socket_mut::<tcp::Socket<'static>>(server).unwrap().window_across().unwrap();
    let server_socket = lo.socket_mut::<tcp::Socket<'static>>(server).unwrap();
    assert_eq!(server_socket.room_across(next), Some(4096));
    // An end out of step with the other, as after a lost packet, takes
    // nothing across, and its window is not taken.
    assert_eq!(server_socket.room_across(next + 1), None);
    let client_socket = lo.socket_mut::<tcp::Socket<'static>>(client).unwrap();
    assert!(!client_socket.heard_window_across(ack + 1, window, smoltcp::time::Instant::ZERO));
    assert!(client_socket.heard_window_across(ack, window, smoltcp::time::Instant::ZERO));
}

fn slots(len: usize) -> &'static mut [SocketStorage<'static>] {
    let mut slots = Vec::new();
    slots.resize_with(len, || SocketStorage::EMPTY);
    slots.leak()
}

#[test]
fn grows_its_socket_slots_and_keeps_its_sockets() {
    let (mut a, mut b) = pair();
    let server = b.add_socket(tcp_memory().socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(7).unwrap();
    let client = a.add_socket(tcp_memory().socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49152).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 10);
    for _ in 1..4 {
        a.add_socket(udp_memory().socket()).ok().unwrap();
    }
    assert_eq!(a.socket_slots(), 4);
    let refused = a.add_socket(udp_memory().socket()).err().unwrap();
    // Not more slots are given back.
    let same = slots(4);
    let same_place = same.as_ptr();
    assert_eq!(a.grow_sockets(same).err().unwrap().as_ptr(), same_place);
    // Twice as many: the old slots come back empty, and the sockets keep
    // their handles and their connection.
    let more = slots(8);
    let old = a.grow_sockets(more).ok().unwrap();
    assert_eq!(old.len(), 4);
    assert_eq!(a.socket_slots(), 8);
    a.add_socket(refused).ok().unwrap();
    a.socket_mut::<tcp::Socket<'static>>(client).unwrap().send_slice(b"hello").unwrap();
    run(&mut a, &mut b, &mut now, 10);
    let mut got = [0; 16];
    let len = b.socket_mut::<tcp::Socket<'static>>(server).unwrap().recv_slice(&mut got).unwrap();
    assert_eq!(&got[..len], b"hello");
}

/// A clock for timestamps in milliseconds, which the tests move.
static CLOCK_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

fn clock_ms() -> u32 {
    CLOCK_MS.load(std::sync::atomic::Ordering::Relaxed)
}

/// A TCP socket that sends timestamps from [`clock_ms`].
fn timed_tcp_socket() -> tcp::Socket<'static> {
    let mut socket = tcp_memory().socket();
    socket.set_tsval_generator(Some(clock_ms));
    socket
}

#[test]
fn a_connection_in_time_wait_gives_its_memory_back_and_lasts() {
    let (mut a, mut b) = pair();
    let server = b.add_socket(tcp_memory().socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(7).unwrap();
    let memory = tcp_memory();
    let lent = tcp_places(&memory);
    let client = a.add_socket(memory.socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49152).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 10);
    assert!(a.shed_tcp(client).is_none(), "only in TIME-WAIT");
    a.socket_mut::<tcp::Socket<'static>>(client).unwrap().close();
    run(&mut a, &mut b, &mut now, 10);
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().close();
    run(&mut a, &mut b, &mut now, 10);
    assert_eq!(a.socket_mut::<tcp::Socket<'static>>(client).unwrap().state(), tcp::State::TimeWait);
    assert_eq!(tcp_places(&a.shed_tcp(client).unwrap()), lent);
    assert!(a.shed_tcp(client).unwrap().receive.is_empty(), "shed already");
    // It stays until TIME-WAIT ends, and then holds nothing.
    run(&mut a, &mut b, &mut now, 100);
    assert_eq!(a.socket_mut::<tcp::Socket<'static>>(client).unwrap().state(), tcp::State::TimeWait);
    now += 10_000_000_000;
    run(&mut a, &mut b, &mut now, 1);
    assert_eq!(a.socket_mut::<tcp::Socket<'static>>(client).unwrap().state(), tcp::State::Closed);
    let left = a.remove_tcp(client).unwrap();
    assert!(left.receive.is_empty() && left.transmit.is_empty());
}

#[test]
fn a_new_connection_takes_the_place_of_one_in_time_wait() {
    // The server closes first, so it is the one in TIME-WAIT, in the slot
    // before the socket that listens now; the client connects again from
    // the same port. As on Linux, the SYN goes to the connection first,
    // which gives way to it, and the listening socket takes it at once,
    // rather than the old connection answering it and the client trying
    // again a second later.
    let (mut a, mut b) = pair();
    let first = b.add_socket(timed_tcp_socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(first).unwrap().listen(7).unwrap();
    let client = a.add_socket(timed_tcp_socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49152).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 10);
    assert_eq!(
        b.socket_mut::<tcp::Socket<'static>>(first).unwrap().state(),
        tcp::State::Established
    );
    let listener = b.add_socket(timed_tcp_socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(listener).unwrap().listen(7).unwrap();
    CLOCK_MS.store(100, std::sync::atomic::Ordering::Relaxed);
    b.socket_mut::<tcp::Socket<'static>>(first).unwrap().close();
    run(&mut a, &mut b, &mut now, 10);
    a.socket_mut::<tcp::Socket<'static>>(client).unwrap().close();
    run(&mut a, &mut b, &mut now, 10);
    assert_eq!(b.socket_mut::<tcp::Socket<'static>>(first).unwrap().state(), tcp::State::TimeWait);
    a.remove_tcp(client).unwrap();

    CLOCK_MS.store(200, std::sync::atomic::Ordering::Relaxed);
    let again = a.add_socket(timed_tcp_socket()).ok().unwrap();
    a.connect(again, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49152).unwrap();
    run(&mut a, &mut b, &mut now, 10);
    assert_eq!(
        a.socket_mut::<tcp::Socket<'static>>(again).unwrap().state(),
        tcp::State::Established
    );
    assert_eq!(b.socket_mut::<tcp::Socket<'static>>(first).unwrap().state(), tcp::State::Closed);
    assert_eq!(
        b.socket_mut::<tcp::Socket<'static>>(listener).unwrap().state(),
        tcp::State::Established
    );
}

/// Random numbers for the fuzzer: xorshift, from a seed of each test's,
/// so that a failure comes back on every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number below `n`, which is not 0.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }

    fn byte(&mut self) -> u8 {
        self.next().to_le_bytes()[0]
    }
}

/// Bytes at the edges of what a header's fields hold: lengths, version
/// and header-length nibbles, flags, the ends of a signed byte.
const EDGES: [u8; 12] = [0, 1, 2, 4, 5, 6, 0x0f, 0x11, 0x40, 0x7f, 0x80, 0xff];

/// Writes `bytes` into `frame` at `at`, as far as the frame goes.
fn put(frame: &mut [u8], at: usize, bytes: &[u8]) {
    for (to, &byte) in frame.iter_mut().skip(at).zip(bytes) {
        *to = byte;
    }
}

/// Changes `frame` one to four times: a bit flipped; a byte set to an
/// edge or at random; a 16-bit field (a length, a port, a window) set to
/// an edge; a 32-bit one (a sequence number, an acknowledgment, an
/// address) moved a little, so that it may still land in a window; the
/// frame cut short or made longer; or a piece of another frame copied
/// over it, where it was in that one. Three changes in four are in the
/// first 80 bytes, where the headers are.
fn mutate(frame: &mut Vec<u8>, others: &[Vec<u8>], random: &mut Random) {
    for _ in 0..=random.below(4) {
        if frame.is_empty() {
            frame.push(random.byte());
        }
        let at = if random.below(4) == 0 {
            random.below(frame.len())
        } else {
            random.below(frame.len().min(80))
        };
        match random.below(8) {
            0 => frame[at] ^= 1 << random.below(8),
            1 => frame[at] = EDGES[random.below(EDGES.len())],
            2 => frame[at] = random.byte(),
            3 => {
                let len = u16::try_from(frame.len()).unwrap_or(u16::MAX);
                let edges = [0, 1, 0x7fff, 0x8000, u16::MAX, len, len.wrapping_sub(14)];
                put(frame, at, &edges[random.below(edges.len())].to_be_bytes());
            }
            4 => {
                let mut word = [0; 4];
                put(&mut word, 0, frame.get(at..).unwrap_or_default());
                let moved = u32::from_be_bytes(word)
                    .wrapping_add(u32::try_from(random.below(4096)).unwrap())
                    .wrapping_sub(2048);
                put(frame, at, &moved.to_be_bytes());
            }
            5 => frame.truncate(random.below(frame.len() + 1)),
            6 => {
                for _ in 0..=random.below(64) {
                    frame.push(random.byte());
                }
            }
            _ => {
                let other = &others[random.below(others.len())];
                let start = random.below(other.len());
                let end = (start + 1 + random.below(16)).min(other.len());
                if frame.len() < end {
                    frame.resize(end, 0);
                }
                frame[start..end].copy_from_slice(&other[start..end]);
            }
        }
    }
}

/// Checks that `frame`, which a stack sent, is one any stack would take:
/// no longer than [`FRAME`], ARP or IPv4 over Ethernet, every header
/// whole, and with checksums that check when its link makes them.
fn assert_well_formed(frame: &[u8], checksums: bool) {
    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::wire::{
        ArpPacket, ArpRepr, EthernetFrame, EthernetProtocol, Icmpv4Message, Icmpv4Packet,
        Icmpv4Repr, IpProtocol, Ipv4Packet, Ipv4Repr, TcpPacket, TcpRepr, UdpPacket, UdpRepr,
    };

    assert!(frame.len() <= FRAME, "a frame of {} bytes", frame.len());
    let caps =
        if checksums { ChecksumCapabilities::default() } else { ChecksumCapabilities::ignored() };
    let ethernet = EthernetFrame::new_checked(frame).expect("an Ethernet header");
    match ethernet.ethertype() {
        EthernetProtocol::Arp => {
            ArpRepr::parse(&ArpPacket::new_checked(ethernet.payload()).unwrap()).unwrap();
        }
        EthernetProtocol::Ipv4 => {
            let packet = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
            let repr = Ipv4Repr::parse(&packet, &caps).unwrap();
            let (from, to) = (IpAddress::Ipv4(repr.src_addr), IpAddress::Ipv4(repr.dst_addr));
            let payload = packet.payload();
            match repr.next_header {
                IpProtocol::Tcp => {
                    let segment = TcpPacket::new_checked(payload).unwrap();
                    TcpRepr::parse(&segment, &from, &to, &caps).unwrap();
                }
                IpProtocol::Udp => {
                    let datagram = UdpPacket::new_checked(payload).unwrap();
                    UdpRepr::parse(&datagram, &from, &to, &caps).unwrap();
                }
                IpProtocol::Icmp => {
                    let message = Icmpv4Packet::new_checked(payload).unwrap();
                    match message.msg_type() {
                        // An error quotes the header of the packet it is
                        // about, and as much of its payload as there was,
                        // up to 8 bytes: smoltcp's parser takes no fewer.
                        Icmpv4Message::DstUnreachable | Icmpv4Message::TimeExceeded => {
                            assert!(!checksums || message.verify_checksum());
                            assert_eq!(&payload[4..8], &[0; 4], "unused, and zero");
                            let quoted = Ipv4Packet::new_unchecked(message.data());
                            assert_eq!(quoted.version(), 4);
                            assert!(usize::from(quoted.header_len()) <= message.data().len());
                        }
                        _ => drop(Icmpv4Repr::parse(&message, &caps).unwrap()),
                    }
                }
                other => panic!("sent IPv4 of protocol {other}"),
            }
        }
        other => panic!("sent a frame of type {other}"),
    }
}

/// The rounds of each fuzzing test: a frame each way in each.
const FUZZ_ROUNDS: usize = 100_000;

/// Two stacks with sockets of every kind and a connection open, then
/// frames each sent, changed at random, given to the other with the
/// frames on their way, as a broken or hostile network would: neither
/// panics, every frame either sends is well formed, and once the network
/// is sound again a new connection between them is made and carries
/// bytes each way.
fn fuzz(checksums: bool, seed: u64) {
    let (mut a, mut b) = wire(checksums);
    assert!(a.grow_sockets(slots(16)).is_ok() && b.grow_sockets(slots(16)).is_ok());
    let mut now = 0;

    // A connection with bytes each way, which stays open.
    let server = b.add_socket(timed_tcp_socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(7).unwrap();
    let client = a.add_socket(timed_tcp_socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49152).unwrap();
    run(&mut a, &mut b, &mut now, 10);
    a.socket_mut::<tcp::Socket<'static>>(client).unwrap().send_slice(b"hello").unwrap();
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().send_slice(b"world").unwrap();
    run(&mut a, &mut b, &mut now, 10);
    // One closed by each end in turn, and one refused.
    let closing = b.add_socket(timed_tcp_socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(closing).unwrap().listen(8).unwrap();
    let closed = a.add_socket(timed_tcp_socket()).ok().unwrap();
    a.connect(closed, (Ipv4Address::new(10, 0, 0, 2).into(), 8), None, 49153).unwrap();
    run(&mut a, &mut b, &mut now, 10);
    a.socket_mut::<tcp::Socket<'static>>(closed).unwrap().close();
    run(&mut a, &mut b, &mut now, 10);
    b.socket_mut::<tcp::Socket<'static>>(closing).unwrap().close();
    let refused = a.add_socket(timed_tcp_socket()).ok().unwrap();
    a.connect(refused, (Ipv4Address::new(10, 0, 0, 2).into(), 9), None, 49154).unwrap();
    run(&mut a, &mut b, &mut now, 10);
    // A socket that listens through it all.
    let listener = b.add_socket(timed_tcp_socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(listener).unwrap().listen(7).unwrap();
    // A datagram each way, and pings each way.
    let to_b = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)), 53);
    let to_a = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 1)), 40000);
    for (stack, port, to) in [(&mut a, 40000, to_b), (&mut b, 53, to_a)] {
        let handle = stack.add_socket(udp_memory().socket()).ok().unwrap();
        let socket = stack.socket_mut::<udp::Socket<'static>>(handle).unwrap();
        socket.bind(port).unwrap();
        socket.send_slice(b"datagram", to).unwrap();
    }
    let pingers = [Pinger::new(&mut a, ping_memory(), 1), Pinger::new(&mut b, ping_memory(), 2)]
        .map(|pinger| pinger.ok().unwrap());
    pingers[0].send(&mut a, Ipv4Address::new(10, 0, 0, 2), 1).unwrap();
    pingers[1].send(&mut b, Ipv4Address::new(10, 0, 0, 1), 1).unwrap();
    run(&mut a, &mut b, &mut now, 10);

    // What each sent so far is what the fuzzer starts from.
    let from_a = core::mem::take(&mut *a.link().sent.borrow_mut());
    let from_b = core::mem::take(&mut *b.link().sent.borrow_mut());
    assert!(from_a.len() >= 10 && from_b.len() >= 10);
    let mut random = Random(seed);
    for round in 0..FUZZ_ROUNDS {
        for (frames, to) in [(&from_a, &mut b), (&from_b, &mut a)] {
            let mut frame = frames[random.below(frames.len())].clone();
            // One in eight goes again as it was, a duplicate.
            if random.below(8) != 0 {
                mutate(&mut frame, frames, &mut random);
            }
            to.link().inbox.borrow_mut().push_back(frame);
        }
        // Bytes on the connection now and then, and time moves on: by a
        // millisecond mostly, now and then far enough for timers to fire.
        if round % 16 == 0
            && let Some(socket) = a.socket_mut::<tcp::Socket<'static>>(client)
            && socket.can_send()
        {
            socket.send_slice(b"more").unwrap();
        }
        now += match random.below(64) {
            0 => 3_000_000_000,
            1..=4 => 200_000_000,
            _ => 1_000_000,
        };
        a.poll(now);
        b.poll(now);
        for stack in [&mut a, &mut b] {
            for frame in stack.link().sent.borrow_mut().drain(..) {
                assert_well_formed(&frame, checksums);
            }
        }
    }

    // The network sound again, and long enough after for what was learned
    // from it, such as where an address is, to be forgotten: a new
    // connection, from and to ports of its own, carries bytes each way.
    a.link().inbox.borrow_mut().clear();
    b.link().inbox.borrow_mut().clear();
    now += 120_000_000_000;
    run(&mut a, &mut b, &mut now, 20);
    let server = b.add_socket(timed_tcp_socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(10).unwrap();
    let client = a.add_socket(timed_tcp_socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 10), None, 50000).unwrap();
    run(&mut a, &mut b, &mut now, 2_000);
    let socket = a.socket_mut::<tcp::Socket<'static>>(client).unwrap();
    assert_eq!(socket.state(), tcp::State::Established);
    socket.send_slice(b"after").unwrap();
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().send_slice(b"all").unwrap();
    run(&mut a, &mut b, &mut now, 20);
    let mut got = [0; 8];
    let len = b.socket_mut::<tcp::Socket<'static>>(server).unwrap().recv_slice(&mut got).unwrap();
    assert_eq!(&got[..len], b"after");
    let len = a.socket_mut::<tcp::Socket<'static>>(client).unwrap().recv_slice(&mut got).unwrap();
    assert_eq!(&got[..len], b"all");
}

#[test]
fn frames_changed_at_random_break_neither_stack() {
    fuzz(true, 0x2545_f491_4f6c_dd1d);
}

#[test]
fn frames_changed_at_random_break_neither_stack_without_checksums() {
    // As on the loopback link: every change gets past the checksums to
    // the headers and the sockets behind them.
    fuzz(false, 0x9e37_79b9_7f4a_7c15);
}

#[test]
fn counts_what_each_interface_sends_and_receives() {
    let (mut a, mut b) = pair();
    let pinger = Pinger::new(&mut a, ping_memory(), 0x55).ok().unwrap();
    pinger.send(&mut a, Ipv4Address::new(10, 0, 0, 2), 1).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 5);
    assert_eq!(pinger.reply(&mut a), Some((1, Ipv4Address::new(10, 0, 0, 2))));
    // An ARP request and an echo request out, an ARP reply and an echo
    // reply back: each frame, and its bytes, on both sides.
    let (sent, received) = (a.counters(), b.counters());
    let frames = a.link().sent.borrow().clone();
    assert_eq!(sent.sent_packets, 2);
    assert_eq!(sent.sent_bytes, frames.iter().map(|f| f.len() as u64).sum::<u64>());
    assert_eq!((received.received_packets, received.received_bytes), (2, sent.sent_bytes));
    assert_eq!((sent.received_packets, received.sent_packets), (2, 2));
    assert_eq!(sent.received_bytes, received.sent_bytes);
    assert_eq!(sent.send_dropped + received.send_dropped, 0);
    // The loopback link counts its packets read in place too, each once.
    let mut lo = loopback();
    let pinger = Pinger::new(&mut lo, ping_memory(), 0x56).ok().unwrap();
    pinger.send(&mut lo, Ipv4Address::new(127, 0, 0, 1), 1).unwrap();
    lo.poll(1_000_000);
    assert_eq!(pinger.reply(&mut lo), Some((1, Ipv4Address::new(127, 0, 0, 1))));
    let counters = lo.counters();
    assert_eq!((counters.sent_packets, counters.received_packets), (2, 2));
    assert_eq!(counters.sent_bytes, counters.received_bytes);
}

#[test]
fn takes_several_addresses_and_a_router_and_lets_them_go() {
    let (mut a, mut b) = pair();
    let first = Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24);
    let second = Ipv4Cidr::new(Ipv4Address::new(192, 168, 7, 1), 24);
    let router = Ipv4Address::new(10, 0, 0, 254);
    a.configure(&[first, second], Some(router));
    assert_eq!(a.ipv4_addresses().collect::<Vec<_>>(), [first, second]);
    assert_eq!((a.ipv4(), a.router()), (Some(first), Some(router)));
    assert!(
        a.reaches(Ipv4Address::new(192, 168, 7, 9)) && a.routes_to(Ipv4Address::new(8, 8, 8, 8))
    );
    // It answers a ping to its second address, from the other's network.
    b.configure(&[Ipv4Cidr::new(Ipv4Address::new(192, 168, 7, 2), 24)], None);
    let pinger = Pinger::new(&mut b, ping_memory(), 0x77).ok().unwrap();
    pinger.send(&mut b, second.address(), 3).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 5);
    assert_eq!(pinger.reply(&mut b), Some((3, second.address())));
    // None, as a DHCP client starts: no address, no router, no way out.
    a.configure(&[], None);
    assert_eq!((a.ipv4(), a.router()), (None, None));
    assert!(!a.routes_to(Ipv4Address::new(8, 8, 8, 8)) && !a.has_address(first.address()));
    assert_eq!(a.mac(), Some(crate::EthernetAddress([2, 0, 0, 0, 0, 1])));
    assert_eq!(loopback().mac(), None);
}

#[test]
fn an_interface_without_an_address_sends_and_takes_broadcasts() {
    // DHCP's first exchange: the client, with no address, sends from
    // 0.0.0.0 to everyone, and the server answers everyone.
    let (mut client, mut server) = pair();
    client.configure(&[], None);
    let on_client = client.add_socket(udp_memory().socket()).ok().unwrap();
    let on_server = server.add_socket(udp_memory().socket()).ok().unwrap();
    server.socket_mut::<udp::Socket<'static>>(on_server).unwrap().bind(67).unwrap();
    let socket = client.socket_mut::<udp::Socket<'static>>(on_client).unwrap();
    socket.bind(68).unwrap();
    let everyone = IpAddress::Ipv4(Ipv4Address::BROADCAST);
    let meta = udp::UdpMetadata {
        local_address: Some(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)),
        ..IpEndpoint::new(everyone, 67).into()
    };
    socket.send_slice(b"discover", meta).unwrap();
    let mut now = 0;
    run(&mut client, &mut server, &mut now, 5);
    let frame = client.link().sent.borrow()[0].clone();
    assert_eq!(&frame[0..6], &[0xff; 6], "to every card");
    assert_eq!(&frame[26..34], &[0, 0, 0, 0, 255, 255, 255, 255], "from 0.0.0.0 to everyone");
    let socket = server.socket_mut::<udp::Socket<'static>>(on_server).unwrap();
    let (data, meta) = socket.recv().unwrap();
    assert_eq!((data, meta.endpoint.port), (&b"discover"[..], 68));
    assert_eq!(meta.endpoint.addr, IpAddress::Ipv4(Ipv4Address::UNSPECIFIED));
    socket.send_slice(b"offer", IpEndpoint::new(everyone, 68)).unwrap();
    run(&mut client, &mut server, &mut now, 5);
    let socket = client.socket_mut::<udp::Socket<'static>>(on_client).unwrap();
    let (data, meta) = socket.recv().unwrap();
    assert_eq!((data, meta.endpoint.port), (&b"offer"[..], 67));
}

#[test]
fn lists_its_sockets_as_proc_net_does() {
    let (mut a, mut b) = pair();
    let listener = b.add_socket(tcp_memory().socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(listener).unwrap().listen(7).unwrap();
    let datagrams = b.add_socket(udp_memory().socket()).ok().unwrap();
    b.socket_mut::<udp::Socket<'static>>(datagrams).unwrap().bind(53).unwrap();
    let _unbound = b.add_socket(udp_memory().socket()).ok().unwrap();
    let client = a.add_socket(tcp_memory().socket()).ok().unwrap();
    a.connect(client, (Ipv4Address::new(10, 0, 0, 2).into(), 7), None, 49152).unwrap();
    let mut now = 0;
    run(&mut a, &mut b, &mut now, 5);
    a.socket_mut::<tcp::Socket<'static>>(client).unwrap().send_slice(b"hi").unwrap();
    let shown = b.sockets().collect::<Vec<_>>();
    assert_eq!(shown.len(), 2, "the unbound socket is not shown");
    // The listening socket took the connection: smoltcp's are one each.
    assert_eq!(shown[0].local, (Some(Ipv4Address::new(10, 0, 0, 2).into()), 7));
    assert_eq!(
        (shown[0].remote, shown[0].state),
        (Some((Ipv4Address::new(10, 0, 0, 1).into(), 49152)), tcp::State::Established)
    );
    assert!(shown[0].tcp && !shown[1].tcp);
    assert_eq!((shown[1].local, shown[1].remote), ((None, 53), None));
    assert!(!shown[0].is_ipv6());
    // On any address and of neither version alone, it is IPv6's as well.
    assert!(shown[1].is_ipv6());
    let mine = a.sockets().next().unwrap();
    assert_eq!(
        (mine.local, mine.remote),
        (
            (Some(Ipv4Address::new(10, 0, 0, 1).into()), 49152),
            Some((Ipv4Address::new(10, 0, 0, 2).into(), 7))
        )
    );
    assert_eq!((mine.send_queue, mine.receive_queue), (2, 0));
    // A socket that listens, before any connection.
    let waiting = b.add_socket(tcp_memory().socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(waiting).unwrap().listen(8).unwrap();
    let listening = b.sockets().find(|s| s.local.1 == 8).unwrap();
    assert_eq!(
        (listening.local, listening.remote, listening.state),
        ((None, 8), None, tcp::State::Listen)
    );
}
