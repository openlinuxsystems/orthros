// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! IPv6's tests: two stacks on the simulated wire of `tests`, which make
//! their addresses and talk over them, and router advertisements written
//! by hand into one's inbox.

use std::vec::Vec;

use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{
    Icmpv6Packet, IpAddress, IpEndpoint, IpVersion, Ipv4Address, Ipv6Address, Ipv6Cidr,
};

use crate::ipv6::{self, Kind, Settings, TEMP_PREFERRED, TEMP_VALID};
use crate::tests::{End, pair, tcp_memory, udp_memory};
use crate::{Link, Stack};

const SECOND: u64 = 1_000_000_000;

const SECRET: [u8; 16] = *b"0123456789abcdef";

fn settings(temporary: bool) -> Option<Settings> {
    Some(Settings { secret: SECRET, autoconf: true, temporary })
}

/// Polls both stacks every 100 ms for `ns` nanoseconds.
fn wait(a: &mut Stack<End>, b: &mut Stack<End>, now: &mut u64, ns: u64) {
    let end = *now + ns;
    while *now < end {
        *now += SECOND / 10;
        a.poll(*now);
        b.poll(*now);
    }
}

/// The stack's link-local address, once it is its own.
fn link_local(stack: &Stack<End>) -> Option<Ipv6Address> {
    stack.ipv6_addresses().map(|cidr| cidr.address()).find(ipv6::link_local)
}

/// The addresses of `kind` the stack has, tested or not.
fn of_kind(stack: &Stack<End>, kind: Kind) -> Vec<ipv6::Address> {
    stack.ipv6().addresses().filter(|a| a.kind == kind).copied().collect()
}

fn ip(text: &str) -> Ipv6Address {
    text.parse().unwrap()
}

/// The router's MAC address and link-local address in the adverts below.
const ROUTER_MAC: [u8; 6] = [2, 0, 0, 0, 0, 0xfe];
fn router() -> Ipv6Address {
    ip("fe80::1")
}

/// A prefix information option: `prefix`/64, autonomous, with its valid
/// and preferred lifetimes in seconds.
fn prefix_option(prefix: &str, valid: u32, preferred: u32) -> Vec<u8> {
    let mut option = std::vec![3, 4, 64, 0xc0];
    option.extend(valid.to_be_bytes());
    option.extend(preferred.to_be_bytes());
    option.extend([0; 4]);
    option.extend(ip(prefix).octets());
    option
}

/// An RDNSS option for `servers`, with a lifetime in seconds.
fn dns_option(servers: &[&str], lifetime: u32) -> Vec<u8> {
    let mut option = std::vec![25, (1 + 2 * servers.len()) as u8, 0, 0];
    option.extend(lifetime.to_be_bytes());
    for server in servers {
        option.extend(ip(server).octets());
    }
    option
}

/// An MTU option.
fn mtu_option(mtu: u32) -> Vec<u8> {
    let mut option = std::vec![5, 1, 0, 0];
    option.extend(mtu.to_be_bytes());
    option
}

/// An Ethernet frame with a router advertisement from [`router`] to all
/// nodes, with a router lifetime in seconds, the M and O flags as
/// `flags`, and `options`; `hop_limit` is 255 for a real one.
fn advert(lifetime: u16, flags: u8, options: &[Vec<u8>], hop_limit: u8) -> Vec<u8> {
    let mut message = std::vec![134, 0, 0, 0, 64, flags];
    message.extend(lifetime.to_be_bytes());
    message.extend([0; 8]);
    for option in options {
        message.extend(option);
    }
    icmp_frame(ROUTER_MAC, router(), ipv6::ALL_NODES, hop_limit, message)
}

/// An Ethernet frame from `mac` and `from` to `to`'s multicast MAC address
/// (or all nodes' for unicast), carrying ICMPv6 `message`, whose checksum
/// is filled in.
fn icmp_frame(
    mac: [u8; 6],
    from: Ipv6Address,
    to: Ipv6Address,
    hop_limit: u8,
    mut message: Vec<u8>,
) -> Vec<u8> {
    let mut packet = Icmpv6Packet::new_unchecked(&mut message[..]);
    packet.fill_checksum(&from, &to);
    let o = to.octets();
    let mut frame = std::vec![0x33, 0x33, o[12], o[13], o[14], o[15]];
    frame.extend(mac);
    frame.extend([0x86, 0xdd]);
    frame.extend([0x60, 0, 0, 0]);
    frame.extend((message.len() as u16).to_be_bytes());
    frame.extend([58, hop_limit]);
    frame.extend(from.octets());
    frame.extend(to.octets());
    frame.extend(message);
    frame
}

/// A neighbour advertisement from someone else that says `target` is
/// theirs.
fn claim(target: Ipv6Address) -> Vec<u8> {
    let mut message = std::vec![136, 0, 0, 0, 0x20, 0, 0, 0];
    message.extend(target.octets());
    message.extend([2, 1]);
    message.extend([2, 0, 0, 0, 0, 0x99]);
    icmp_frame([2, 0, 0, 0, 0, 0x99], ip("fe80::99"), ipv6::ALL_NODES, 255, message)
}

/// Puts `frame` in the stack's inbox, as if it came on the wire.
fn deliver(stack: &mut Stack<End>, frame: Vec<u8>) {
    stack.link().inbox.borrow_mut().push_back(frame);
}

/// The ICMPv6 messages of `kind` the stack sent, as (source,
/// destination).
fn sent_icmp(stack: &mut Stack<End>, kind: u8) -> Vec<(Ipv6Address, Ipv6Address)> {
    let sent = stack.link().sent.borrow();
    sent.iter()
        .filter(|f| f.len() > 54 && f[12..14] == [0x86, 0xdd] && f[20] == 58 && f[54] == kind)
        .map(|f| {
            let at = |o: usize| Ipv6Address::from(<[u8; 16]>::try_from(&f[o..o + 16]).unwrap());
            (at(22), at(38))
        })
        .collect()
}

#[test]
fn stable_identifiers_depend_on_the_prefix_the_card_the_counter_and_the_key() {
    let prefix = Ipv6Cidr::new(ip("2001:db8:1::"), 64);
    let other = Ipv6Cidr::new(ip("2001:db8:2::"), 64);
    let mac = [2, 0, 0, 0, 0, 1];
    let iid = ipv6::stable_iid(&prefix, mac, 0, &SECRET);
    assert_eq!(iid, ipv6::stable_iid(&prefix, mac, 0, &SECRET));
    assert_ne!(iid, ipv6::stable_iid(&other, mac, 0, &SECRET));
    assert_ne!(iid, ipv6::stable_iid(&prefix, [2, 0, 0, 0, 0, 2], 0, &SECRET));
    assert_ne!(iid, ipv6::stable_iid(&prefix, mac, 1, &SECRET));
    assert_ne!(iid, ipv6::stable_iid(&prefix, mac, 0, &[0; 16]));
    // Only the prefix's 64 bits count, not an address's own.
    let address = Ipv6Cidr::new(ip("2001:db8:1::1234"), 64);
    assert_eq!(iid, ipv6::stable_iid(&address, mac, 0, &SECRET));
}

#[test]
fn link_local_addresses_are_tested_then_used_over_udp_and_tcp() {
    let (mut a, mut b) = pair();
    a.configure_ipv6(settings(false), &[], None);
    b.configure_ipv6(settings(false), &[], None);
    // Tested first: not the stack's own until a second has passed.
    assert_eq!(link_local(&a), None);
    let tentative = of_kind(&a, Kind::LinkLocal);
    assert!(tentative.len() == 1 && tentative[0].tentative());
    let mut now = 0;
    wait(&mut a, &mut b, &mut now, SECOND / 2);
    assert_eq!(link_local(&a), None);
    // The test: a solicitation from the unspecified address to the
    // address's solicited-node group.
    let target = tentative[0].cidr.address();
    let o = target.octets();
    let group = Ipv6Address::new(
        0xff02,
        0,
        0,
        0,
        0,
        1,
        0xff00 | u16::from(o[13]),
        u16::from_be_bytes([o[14], o[15]]),
    );
    assert_eq!(sent_icmp(&mut a, 135), [(Ipv6Address::UNSPECIFIED, group)]);
    wait(&mut a, &mut b, &mut now, SECOND);
    let (from, to) = (link_local(&a).unwrap(), link_local(&b).unwrap());
    assert_eq!(from, target);
    assert_ne!(from, to);
    // Not made from the MAC address (EUI-64), which would follow the
    // card from network to network.
    assert_ne!(&from.octets()[8..], &[0, 0, 0, 0xff, 0xfe, 0, 0, 1]);
    // Then it asks for routers, from it.
    assert_eq!(sent_icmp(&mut a, 133).first(), Some(&(from, ipv6::ALL_ROUTERS)));

    let receiver = b.add_socket(udp_memory().socket()).ok().unwrap();
    b.socket_mut::<udp::Socket<'static>>(receiver).unwrap().bind(53).unwrap();
    let sender = a.add_socket(udp_memory().socket()).ok().unwrap();
    let socket = a.socket_mut::<udp::Socket<'static>>(sender).unwrap();
    socket.bind(40000).unwrap();
    socket.send_slice(b"over six", IpEndpoint::new(IpAddress::Ipv6(to), 53)).unwrap();
    wait(&mut a, &mut b, &mut now, SECOND / 5);
    let (data, meta) = b.socket_mut::<udp::Socket<'static>>(receiver).unwrap().recv().unwrap();
    assert_eq!(data, b"over six");
    assert_eq!(meta.endpoint, IpEndpoint::new(IpAddress::Ipv6(from), 40000));

    let server = b.add_socket(tcp_memory().socket()).ok().unwrap();
    b.socket_mut::<tcp::Socket<'static>>(server).unwrap().listen(7).unwrap();
    let client = a.add_socket(tcp_memory().socket()).ok().unwrap();
    a.connect(client, (IpAddress::Ipv6(to), 7), None, 49152).unwrap();
    wait(&mut a, &mut b, &mut now, SECOND / 5);
    let socket = b.socket_mut::<tcp::Socket<'static>>(server).unwrap();
    assert_eq!(socket.state(), tcp::State::Established);
    assert_eq!(socket.remote_endpoint(), Some(IpEndpoint::new(IpAddress::Ipv6(from), 49152)));
}

#[test]
fn a_socket_kept_to_one_version_takes_nothing_over_the_other() {
    let (mut a, mut b) = pair();
    a.configure_ipv6(settings(false), &[], None);
    b.configure_ipv6(settings(false), &[], None);
    let mut now = 0;
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    let to = link_local(&b).unwrap();
    // A socket of IPv4's on any address does not take IPv6 connections,
    // nor its datagrams.
    let server = b.add_socket(tcp_memory().socket()).ok().unwrap();
    let socket = b.socket_mut::<tcp::Socket<'static>>(server).unwrap();
    socket.listen(7).unwrap();
    socket.set_listen_version(Some(IpVersion::Ipv4));
    let receiver = b.add_socket(udp_memory().socket()).ok().unwrap();
    let socket = b.socket_mut::<udp::Socket<'static>>(receiver).unwrap();
    socket.bind(53).unwrap();
    socket.set_version(Some(IpVersion::Ipv4));
    let client = a.add_socket(tcp_memory().socket()).ok().unwrap();
    a.connect(client, (IpAddress::Ipv6(to), 7), None, 49152).unwrap();
    let sender = a.add_socket(udp_memory().socket()).ok().unwrap();
    let socket = a.socket_mut::<udp::Socket<'static>>(sender).unwrap();
    socket.bind(40000).unwrap();
    socket.send_slice(b"six", IpEndpoint::new(IpAddress::Ipv6(to), 53)).unwrap();
    socket
        .send_slice(b"four", IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)), 53))
        .unwrap();
    // Long enough for the SYN to be sent again once each stack has found
    // the other's MAC address.
    wait(&mut a, &mut b, &mut now, 3 * SECOND);
    assert_eq!(b.socket_mut::<tcp::Socket<'static>>(server).unwrap().state(), tcp::State::Listen);
    assert_eq!(a.socket_mut::<tcp::Socket<'static>>(client).unwrap().state(), tcp::State::Closed);
    let socket = b.socket_mut::<udp::Socket<'static>>(receiver).unwrap();
    assert_eq!(socket.recv().unwrap().0, b"four");
    assert!(socket.recv().is_err());
}

#[test]
fn an_address_someone_else_has_is_not_taken() {
    let (mut a, mut b) = pair();
    let shared = Ipv6Cidr::new(ip("2001:db8::7"), 64);
    b.configure_ipv6(settings(false), &[shared], None);
    let mut now = 0;
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    assert!(b.has_address(shared.address()));
    // a tests it, b answers that it is its own, and a gives it up: an
    // address it was given is not made again.
    a.configure_ipv6(settings(false), &[shared], None);
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    assert!(!a.has_address(shared.address()));
    assert!(of_kind(&a, Kind::Static).is_empty());
    assert!(b.has_address(shared.address()));
    // Its link-local address, which nobody had, is its own.
    assert!(link_local(&a).is_some());
}

#[test]
fn a_stable_address_that_is_taken_is_made_again_with_the_next_counter() {
    let (mut a, mut b) = pair();
    a.configure_ipv6(settings(false), &[], None);
    let first = of_kind(&a, Kind::LinkLocal)[0].cidr.address();
    deliver(&mut a, claim(first));
    let mut now = 0;
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    let second = link_local(&a).unwrap();
    assert_ne!(second, first);
    let prefix = Ipv6Cidr::new(ip("fe80::"), 64);
    let iid = ipv6::stable_iid(&prefix, a.link().mac(), 1, &SECRET);
    assert_eq!(&second.octets()[8..], &iid);
    // Once its own, a claim is not taken for one on it: it stays.
    deliver(&mut a, claim(second));
    wait(&mut a, &mut b, &mut now, SECOND);
    assert_eq!(link_local(&a), Some(second));
}

#[test]
fn an_advert_gives_addresses_a_router_dns_servers_and_an_mtu() {
    let (mut a, mut b) = pair();
    a.configure_ipv6(settings(true), &[], None);
    let mut now = 0;
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    let options = [
        prefix_option("2001:db8:1::", 7 * 86_400, 7 * 86_400),
        dns_option(&["2001:db8::53", "2001:db8::54"], 600),
        mtu_option(1400),
    ];
    // One that came from off the link, with a hop limit below 255, is
    // ignored.
    deliver(&mut a, advert(1800, 0x40, &options, 64));
    wait(&mut a, &mut b, &mut now, SECOND / 5);
    assert!(a.ipv6().router().is_none());
    deliver(&mut a, advert(1800, 0x40, &options, 255));
    wait(&mut a, &mut b, &mut now, SECOND / 5);
    assert_eq!(a.router_ipv6(), Some(router()));
    assert_eq!(a.ipv6().dhcp(), (false, true));
    assert_eq!(a.ipv6().mtu(), Some(1400));
    let servers: Vec<_> = a.ipv6().dns().map(|s| s.address).collect();
    assert_eq!(servers, [ip("2001:db8::53"), ip("2001:db8::54")]);
    let stable = of_kind(&a, Kind::Stable);
    let temporary = of_kind(&a, Kind::Temporary);
    assert_eq!((stable.len(), temporary.len()), (1, 1));
    let prefix = Ipv6Cidr::new(ip("2001:db8:1::"), 64);
    assert!(prefix.contains_addr(&stable[0].cidr.address()));
    assert!(prefix.contains_addr(&temporary[0].cidr.address()));
    assert_eq!(
        &stable[0].cidr.address().octets()[8..],
        &ipv6::stable_iid(&prefix, a.link().mac(), 0, &SECRET)
    );
    // Neither is used before its test.
    assert!(!a.routes_to(ip("2001:db8:9::1")));
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    assert!(a.routes_to(ip("2001:db8:9::1")));
    // Sent from the temporary one, off the link and on it; to a
    // link-local address, from the link-local one.
    let temporary = temporary[0].cidr.address();
    assert_eq!(
        a.source_for(IpAddress::Ipv6(ip("2001:db8:9::1"))),
        Some(IpAddress::Ipv6(temporary))
    );
    assert_eq!(
        a.source_for(IpAddress::Ipv6(ip("2001:db8:1::5"))),
        Some(IpAddress::Ipv6(temporary))
    );
    let own = link_local(&a).unwrap();
    assert_eq!(a.source_for(IpAddress::Ipv6(ip("fe80::5"))), Some(IpAddress::Ipv6(own)));
    // The temporary address is preferred for a day at most, less a part
    // of it, and valid for two.
    let address = of_kind(&a, Kind::Temporary)[0];
    let preferred = address.preferred_until.unwrap();
    assert!(preferred <= now + TEMP_PREFERRED && preferred > now + TEMP_PREFERRED * 6 / 10);
    assert!(address.valid_until.unwrap() <= now + TEMP_VALID);
    // The router's lifetime ends it.
    deliver(&mut a, advert(0, 0, &[], 255));
    wait(&mut a, &mut b, &mut now, SECOND / 5);
    assert_eq!(a.router_ipv6(), None);
    assert!(!a.routes_to(ip("2001:db8:9::1")));
}

#[test]
fn an_advert_cannot_cut_a_prefix_short_but_lets_it_lapse() {
    let (mut a, mut b) = pair();
    a.configure_ipv6(settings(false), &[], None);
    let mut now = 0;
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    deliver(&mut a, advert(1800, 0, &[prefix_option("2001:db8:1::", 86_400, 3_600)], 255));
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    let address = of_kind(&a, Kind::Stable)[0].cidr.address();
    assert!(a.has_address(address));
    // Valid for a minute, an advert says: two hours, says RFC 4862.
    deliver(&mut a, advert(1800, 0, &[prefix_option("2001:db8:1::", 60, 30)], 255));
    wait(&mut a, &mut b, &mut now, SECOND / 5);
    let valid = of_kind(&a, Kind::Stable)[0].valid_until.unwrap();
    assert!(valid > now + 7_100 * SECOND && valid <= now + 7_200 * SECOND);
    // Deprecated after 30 seconds: still the stack's, not chosen while
    // another is not.
    wait(&mut a, &mut b, &mut now, 31 * SECOND);
    assert!(of_kind(&a, Kind::Stable)[0].deprecated(now));
    assert!(a.has_address(address));
    // Gone after the two hours.
    a.poll(now + 7_201 * SECOND);
    assert!(!a.has_address(address));
    assert!(of_kind(&a, Kind::Stable).is_empty());
}

#[test]
fn a_new_temporary_address_comes_before_the_old_one_is_deprecated() {
    let (mut a, mut b) = pair();
    a.configure_ipv6(settings(true), &[], None);
    let mut now = 0;
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    let lifetime = 7 * 86_400;
    deliver(&mut a, advert(9000, 0, &[prefix_option("2001:db8:1::", lifetime, lifetime)], 255));
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    let first = of_kind(&a, Kind::Temporary)[0];
    let deprecated_at = first.preferred_until.unwrap();
    // The stack says when it next has work: no later than its successor
    // is due.
    let due = now + a.poll_delay(now).unwrap();
    assert!(due <= deprecated_at - 5 * SECOND, "due at {due}, deprecated at {deprecated_at}");
    now = deprecated_at - 4 * SECOND;
    a.poll(now);
    let both = of_kind(&a, Kind::Temporary);
    assert_eq!(both.len(), 2);
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    let second = of_kind(&a, Kind::Temporary).into_iter().find(|t| t.cidr != first.cidr).unwrap();
    assert!(a.has_address(second.cidr.address()));
    // Once the first is deprecated, what is sent goes from the second.
    wait(&mut a, &mut b, &mut now, 3 * SECOND);
    let to = IpAddress::Ipv6(ip("2001:db8:9::1"));
    assert_eq!(a.source_for(to), Some(IpAddress::Ipv6(second.cidr.address())));
    // The first goes when its two days are up.
    a.poll(first.valid_until.unwrap());
    assert!(!a.has_address(first.cidr.address()));
}

#[test]
fn a_new_mac_address_makes_new_stable_addresses() {
    let (mut a, mut b) = pair();
    a.configure_ipv6(settings(false), &[], None);
    let mut now = 0;
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    let before = link_local(&a).unwrap();
    a.link().mac = [2, 0, 0, 0, 0, 0x33];
    a.set_mac([2, 0, 0, 0, 0, 0x33]);
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    let after = link_local(&a).unwrap();
    assert_ne!(before, after);
    assert!(!a.has_address(before));
}

#[test]
fn ipv6_off_leaves_the_interface_with_none() {
    let (mut a, mut b) = pair();
    let mut now = 0;
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    assert!(a.ipv6_addresses().next().is_none());
    assert!(sent_icmp(&mut a, 135).is_empty() && sent_icmp(&mut a, 133).is_empty());
    a.configure_ipv6(settings(false), &[], None);
    wait(&mut a, &mut b, &mut now, 2 * SECOND);
    assert!(link_local(&a).is_some());
    a.configure_ipv6(None, &[], None);
    assert!(a.ipv6_addresses().next().is_none());
    // Its IPv4 address stays through it all.
    assert!(a.has_address(Ipv4Address::new(10, 0, 0, 1)));
}
