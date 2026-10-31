// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Kani proof harnesses (ADR 0189): for every frame of up to the lengths
//! given, whatever its bytes, each header the stack reads from a card,
//! and each TCP option, is read without a panic, and what is read is
//! written back as one that reads the same; and for every sequence of
//! loopback packets within the bounds given, the ring hands each back
//! whole and in order. `ob test kani` runs them. Checksums are not
//! checked here, as the bounds would be out of Kani's reach; the fuzzer
//! in the tests checks them.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpPacket, ArpRepr, EthernetFrame, EthernetRepr, Icmpv4Packet, Icmpv4Repr, IpAddress,
    Ipv4Address, Ipv4Packet, Ipv4Repr, TcpOption, UdpPacket, UdpRepr,
};

use crate::{Link, Loopback};

/// Any bytes, of any length up to `N`.
fn any_bytes<const N: usize>() -> ([u8; N], usize) {
    (kani::any(), kani::any_where(|&len| len <= N))
}

/// The addresses of the segments and datagrams the proofs read.
const FROM: IpAddress = IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 1));
const TO: IpAddress = IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2));

/// An Ethernet header.
#[kani::proof]
#[kani::unwind(17)]
fn ethernet_headers_read_back_the_same() {
    let (bytes, len) = any_bytes::<16>();
    let Ok(frame) = EthernetFrame::new_checked(&bytes[..len]) else { return };
    let Ok(repr) = EthernetRepr::parse(&frame) else { return };
    let mut out = [0u8; 14];
    assert!(repr.buffer_len() == out.len());
    repr.emit(&mut EthernetFrame::new_unchecked(&mut out[..]));
    let again = EthernetFrame::new_checked(&out[..]).unwrap();
    assert!(EthernetRepr::parse(&again).unwrap() == repr);
}

/// An ARP packet.
#[kani::proof]
#[kani::unwind(33)]
fn arp_packets_read_back_the_same() {
    let (bytes, len) = any_bytes::<32>();
    let Ok(packet) = ArpPacket::new_checked(&bytes[..len]) else { return };
    let Ok(repr) = ArpRepr::parse(&packet) else { return };
    let mut out = [0u8; 28];
    assert!(repr.buffer_len() == out.len());
    repr.emit(&mut ArpPacket::new_unchecked(&mut out[..]));
    let again = ArpPacket::new_checked(&out[..]).unwrap();
    assert!(ArpRepr::parse(&again).unwrap() == repr);
}

/// An IPv4 header, with options or without, and the payload it says it
/// has, which is within the bytes that came.
#[kani::proof]
#[kani::unwind(65)]
fn ipv4_headers_read_back_the_same() {
    let (bytes, len) = any_bytes::<64>();
    let checksums = ChecksumCapabilities::ignored();
    let Ok(packet) = Ipv4Packet::new_checked(&bytes[..len]) else { return };
    let Ok(repr) = Ipv4Repr::parse(&packet, &checksums) else { return };
    let payload = packet.payload();
    assert!(payload.len() == repr.payload_len);
    let written = repr.buffer_len() + payload.len();
    assert!(written <= len);
    let mut out = [0u8; 64];
    let mut emitted = Ipv4Packet::new_unchecked(&mut out[..written]);
    repr.emit(&mut emitted, &checksums);
    emitted.payload_mut().copy_from_slice(payload);
    let again = Ipv4Packet::new_checked(&out[..written]).unwrap();
    assert!(Ipv4Repr::parse(&again, &checksums).unwrap() == repr);
    assert!(again.payload() == payload);
}

/// An ICMPv4 message: an echo, or an error with the header and the first
/// bytes of the packet it is about.
#[kani::proof]
#[kani::unwind(49)]
fn icmpv4_messages_read_back_the_same() {
    let (bytes, len) = any_bytes::<48>();
    let checksums = ChecksumCapabilities::ignored();
    let Ok(packet) = Icmpv4Packet::new_checked(&bytes[..len]) else { return };
    let Ok(repr) = Icmpv4Repr::parse(&packet, &checksums) else { return };
    let written = repr.buffer_len();
    assert!(written <= len);
    let mut out = [0u8; 48];
    repr.emit(&mut Icmpv4Packet::new_unchecked(&mut out[..written]), &checksums);
    let again = Icmpv4Packet::new_checked(&out[..written]).unwrap();
    assert!(Icmpv4Repr::parse(&again, &checksums).unwrap() == repr);
}

/// A UDP datagram, and its payload.
#[kani::proof]
#[kani::unwind(17)]
fn udp_datagrams_read_back_the_same() {
    let (bytes, len) = any_bytes::<16>();
    let checksums = ChecksumCapabilities::ignored();
    let Ok(packet) = UdpPacket::new_checked(&bytes[..len]) else { return };
    let Ok(repr) = UdpRepr::parse(&packet, &FROM, &TO, &checksums) else { return };
    let payload = packet.payload();
    let written = repr.header_len() + payload.len();
    assert!(written <= len);
    let mut out = [0u8; 16];
    let mut emitted = UdpPacket::new_unchecked(&mut out[..written]);
    let fill = |into: &mut [u8]| into.copy_from_slice(payload);
    repr.emit(&mut emitted, &FROM, &TO, payload.len(), fill, &checksums);
    let again = UdpPacket::new_checked(&out[..written]).unwrap();
    assert!(UdpRepr::parse(&again, &FROM, &TO, &checksums).unwrap() == repr);
    assert!(again.payload() == payload);
}

/// A TCP option, of any bytes up to the 40 a header holds: read without
/// a panic, it takes at least a byte, so that a header's options are
/// read to their end, and no more than the bytes there; written back, it
/// takes no more room than it came in and reads the same.
#[kani::proof]
#[kani::unwind(41)]
fn tcp_options_read_back_the_same() {
    let (bytes, len) = any_bytes::<40>();
    let Ok((rest, option)) = TcpOption::parse(&bytes[..len]) else { return };
    assert!(rest.len() < len);
    let written = option.buffer_len();
    assert!(written <= len - rest.len());
    let mut out = [0u8; 40];
    assert!(option.emit(&mut out[..written]).is_empty());
    let (left, again) = TcpOption::parse(&out[..written]).unwrap();
    assert!(left.is_empty() && again == option);
}

/// The longest packet of the loopback proof, 8 bytes, and its ring, 44
/// bytes, of which the link takes 40, a multiple of its headers' 8: room
/// for two slots of the longest packets and one of an empty packet.
const MTU: usize = 8;
const RING: usize = 44;
/// The sends, reads and answers the proof makes, in any order.
const STEPS: usize = 5;

/// Whatever packets are sent, read and answered in place, of whatever
/// lengths, the loopback ring hands back each packet it took, whole and
/// in order; it takes none longer than its MTU, and an empty ring takes
/// any other.
#[kani::proof]
#[kani::unwind(11)]
fn the_loopback_ring_keeps_packets_whole_and_in_order() {
    let mut link = Loopback::new(Box::leak(vec![0u8; RING].into_boxed_slice()), MTU);
    // The packets in the ring, oldest first, round from `first`: each
    // one's length and the byte it is filled with.
    let mut queue = [(0usize, 0u8); STEPS];
    let (mut first, mut count) = (0, 0);
    let mut spare = [0u8; MTU + 1];
    let spare_at = spare.as_ptr().addr();
    let mut fill = 0u8;
    for _ in 0..STEPS {
        fill += 1;
        let len = kani::any_where(|&len| len <= MTU + 1);
        let write = |bytes: &mut [u8]| {
            bytes.fill(fill);
            bytes.as_ptr().addr() != spare_at
        };
        match kani::any::<u8>() % 3 {
            0 => {
                let empty = count == 0;
                let took = link.send_with(len, &mut spare, write);
                assert!(took == (len <= MTU) || (!empty && !took));
                if took {
                    queue[(first + count) % STEPS] = (len, fill);
                    count += 1;
                }
            }
            read => {
                let Some((oldest, answer)) = link.oldest() else {
                    assert!(count == 0);
                    continue;
                };
                assert!(count > 0);
                let answered = read == 1 && answer.send_with(len, &mut spare, write);
                assert!(!answered || len <= MTU);
                let (want, filled) = queue[first];
                oldest.read(|packet| {
                    assert!(packet.len() == want && packet.iter().all(|&byte| byte == filled));
                });
                first = (first + 1) % STEPS;
                count -= 1;
                if answered {
                    queue[(first + count) % STEPS] = (len, fill);
                    count += 1;
                }
            }
        }
        assert!(link.pending() == (count > 0));
    }
}
