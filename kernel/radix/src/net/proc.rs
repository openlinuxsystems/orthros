// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/processes/net` (ADR 0190): the interfaces' counts, the routes and
//! the sockets, in the formats of Linux's `/proc/net/dev`, `route`, `tcp`
//! and `udp`, which ss(8) and ported tools read; and for IPv6 (ADR 0332)
//! `if_inet6`, `ipv6_route`, `tcp6` and `udp6`.
//!
//! Addresses and ports are in hexadecimal as Linux writes them: an IPv4
//! address is its four bytes, in network order, read as a little-endian
//! number (10.0.2.15 is `0F02000A`), an IPv6 address in the socket files
//! is four such numbers (::1 is `00000000000000000000000001000000`) and
//! in the others its bytes in order, and a port is a number (port 80 is
//! `0050`). A socket that listens, or a datagram socket, is a smoltcp
//! socket on each interface it is on, and on several for a listening
//! socket's pool; it is shown once, as Linux shows it. The user is root's
//! (0) and the inode 0, until users come (Phase 13).

use alloc::vec::Vec;
use core::fmt::{self, Write};

use radix_net::ipv6::{self, Kind};
use radix_net::smoltcp::socket::tcp::State;
use radix_net::{IpAddress, Ipv4Address, Ipv6Address, Shown};
use radix_vfs::Text;

use super::{interfaces, kmalloc};

/// Linux's header of `/proc/net/dev`.
const DEV_HEADER: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n";

/// `/processes/net/dev`: what each interface sent and received.
pub fn dev(text: &mut Text) -> fmt::Result {
    text.write_str(DEV_HEADER)?;
    for interface in interfaces() {
        let c = interface.stack.lock().counters();
        writeln!(
            text,
            "{:>6}:{:>8} {:>7} {:>4} {:>4} {:>4} {:>5} {:>10} {:>9} {:>8} {:>7} {:>4} {:>4} {:>4} {:>5} {:>7} {:>10}",
            interface.name.as_str(),
            c.received_bytes,
            c.received_packets,
            0,
            0,
            0,
            0,
            0,
            0,
            c.sent_bytes,
            c.sent_packets,
            0,
            c.send_dropped,
            0,
            0,
            0,
            0,
        )?;
    }
    Ok(())
}

/// The width Linux pads each line of `/proc/net/route` to, with blanks.
const ROUTE_LINE: usize = 127;

/// The most addresses an interface has, as smoltcp is built.
const ADDRESSES: usize = 16;

/// Linux's RTF_UP and RTF_GATEWAY.
const ROUTE_UP: u16 = 0x1;
const ROUTE_GATEWAY: u16 = 0x2;

/// `address` as `/proc/net` writes one: its bytes, in network order, as
/// a little-endian number.
fn hex(address: Ipv4Address) -> u32 {
    u32::from_le_bytes(address.octets())
}

/// Writes `line` and blanks to [`ROUTE_LINE`], then a newline.
fn padded(text: &mut Text, line: &str) -> fmt::Result {
    writeln!(text, "{line:<ROUTE_LINE$}")
}

/// `/processes/net/route`: each card's networks, then its default route,
/// as Linux's main table lists them (`lo`'s are in its local table, which
/// this file does not show).
pub fn route(text: &mut Text) -> fmt::Result {
    padded(
        text,
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT",
    )?;
    for interface in interfaces().filter(|interface| !interface.is_loopback()) {
        let mut lines = radix_klog::TextBuf::<160>::new();
        let (addresses, router) = {
            let stack = interface.stack.lock();
            let mut addresses = [None; ADDRESSES];
            for (place, cidr) in addresses.iter_mut().zip(stack.ipv4_addresses()) {
                *place = Some(cidr);
            }
            (addresses, stack.router())
        };
        let name = interface.name.as_str();
        let any = Ipv4Address::UNSPECIFIED;
        // A /32's network is the address alone, which has no route of
        // its own on Linux.
        let routes = addresses
            .into_iter()
            .flatten()
            .filter(|cidr| cidr.prefix_len() < 32)
            .map(|cidr| (cidr.network().address(), any, ROUTE_UP, cidr.netmask()))
            .chain(router.map(|router| (any, router, ROUTE_UP | ROUTE_GATEWAY, any)));
        for (destination, gateway, flags, mask) in routes {
            lines.clear();
            write!(
                lines,
                "{name}\t{:08X}\t{:08X}\t{flags:04X}\t0\t0\t0\t{:08X}\t0\t0\t0",
                hex(destination),
                hex(gateway),
                hex(mask),
            )?;
            padded(text, lines.as_str())?;
        }
    }
    Ok(())
}

/// Linux's IFA_F_ flags of an address in `/proc/net/if_inet6`.
const IFA_TEMPORARY: u8 = 0x01;
const IFA_DEPRECATED: u8 = 0x20;
const IFA_TENTATIVE: u8 = 0x40;
const IFA_PERMANENT: u8 = 0x80;

/// Linux's scopes, in `/proc/net/if_inet6`.
const SCOPE_GLOBAL: u8 = 0x00;
const SCOPE_HOST: u8 = 0x10;
const SCOPE_LINK: u8 = 0x20;

/// Writes `address`'s bytes in order, in hexadecimal.
fn write_ipv6(text: &mut Text, address: Ipv6Address) -> fmt::Result {
    for byte in address.octets() {
        write!(text, "{byte:02x}")?;
    }
    Ok(())
}

/// `/processes/net/if_inet6`: each interface's IPv6 addresses, tested or
/// not, as Linux lists them: the address, the interface's number, the
/// prefix length, the scope and the flags, in hexadecimal, and its name.
pub fn if_inet6(text: &mut Text) -> fmt::Result {
    let now = crate::sched::now_ns();
    for interface in interfaces() {
        let stack = interface.stack.lock();
        // `lo` has the cards' addresses after its own: they are theirs.
        let own = if interface.is_loopback() { 1 } else { usize::MAX };
        for address in stack.ipv6().addresses().take(own) {
            let ip = address.cidr.address();
            let scope = if ip.is_loopback() {
                SCOPE_HOST
            } else if ipv6::link_local(&ip) {
                SCOPE_LINK
            } else {
                SCOPE_GLOBAL
            };
            let mut flags = 0;
            if address.tentative() {
                flags |= IFA_TENTATIVE;
            }
            if address.deprecated(now) {
                flags |= IFA_DEPRECATED;
            }
            match address.kind {
                Kind::Temporary => flags |= IFA_TEMPORARY,
                Kind::Static | Kind::LinkLocal => flags |= IFA_PERMANENT,
                Kind::Stable => {}
            }
            write_ipv6(text, ip)?;
            writeln!(
                text,
                " {:02x} {:02x} {scope:02x} {flags:02x} {:>8}",
                interface.index(),
                address.cidr.prefix_len(),
                interface.name.as_str()
            )?;
        }
    }
    Ok(())
}

/// Linux's RTF_UP, RTF_GATEWAY, RTF_DEFAULT and RTF_ADDRCONF, of
/// `/proc/net/ipv6_route`.
const RTF6_UP: u32 = 0x0001;
const RTF6_GATEWAY: u32 = 0x0002;
const RTF6_DEFAULT: u32 = 0x0001_0000;
const RTF6_ADDRCONF: u32 = 0x0004_0000;

/// `/processes/net/ipv6_route`: each card's IPv6 networks, then its
/// default route, as Linux lists them: the destination and its prefix
/// length, the source and its (none), the next hop, the metric, two
/// counts, the flags and the interface.
pub fn ipv6_route(text: &mut Text) -> fmt::Result {
    let any = Ipv6Address::UNSPECIFIED;
    for interface in interfaces().filter(|interface| !interface.is_loopback()) {
        let (networks, router) = {
            let stack = interface.stack.lock();
            let mut networks = [None; ADDRESSES];
            let found = stack.ipv6_addresses().filter(|cidr| cidr.prefix_len() < 128);
            for (place, cidr) in networks.iter_mut().zip(found) {
                *place = Some(network6(cidr));
            }
            (networks, stack.router_ipv6())
        };
        let mut seen = [None; ADDRESSES];
        for (at, network) in networks.into_iter().enumerate() {
            let Some(network) = network.filter(|n| !seen.contains(&Some(*n))) else { continue };
            seen[at] = Some(network);
            route_line(
                text,
                network.address(),
                network.prefix_len(),
                any,
                256,
                RTF6_UP,
                interface,
            )?;
        }
        if let Some(router) = router {
            let flags = RTF6_UP | RTF6_GATEWAY | RTF6_DEFAULT | RTF6_ADDRCONF;
            route_line(text, any, 0, router, 1024, flags, interface)?;
        }
    }
    Ok(())
}

/// The network `cidr` is on: its address with the bits past its prefix
/// cleared.
fn network6(cidr: radix_net::Ipv6Cidr) -> radix_net::Ipv6Cidr {
    let bits = u128::from_be_bytes(cidr.address().octets());
    let mask = u128::MAX.checked_shl(128 - u32::from(cidr.prefix_len())).unwrap_or(0);
    radix_net::Ipv6Cidr::new(Ipv6Address::from((bits & mask).to_be_bytes()), cidr.prefix_len())
}

/// One line of `/proc/net/ipv6_route`.
fn route_line(
    text: &mut Text,
    to: Ipv6Address,
    prefix: u8,
    via: Ipv6Address,
    metric: u32,
    flags: u32,
    interface: &super::Interface,
) -> fmt::Result {
    write_ipv6(text, to)?;
    write!(text, " {prefix:02x} ")?;
    write_ipv6(text, Ipv6Address::UNSPECIFIED)?;
    write!(text, " 00 ")?;
    write_ipv6(text, via)?;
    writeln!(text, " {metric:08x} 00000000 00000000 {flags:08x} {:>8}", interface.name.as_str())
}

/// Linux's number for a TCP state, `st` in `/proc/net/tcp`.
fn state_number(state: State) -> u8 {
    match state {
        State::Established => 0x01,
        State::SynSent => 0x02,
        State::SynReceived => 0x03,
        State::FinWait1 => 0x04,
        State::FinWait2 => 0x05,
        State::TimeWait => 0x06,
        State::Closed => 0x07,
        State::CloseWait => 0x08,
        State::LastAck => 0x09,
        State::Listen => 0x0a,
        State::Closing => 0x0b,
    }
}

/// The sockets of every interface of `tcp`'s protocol and of IPv6 when
/// `v6`, each listening or datagram socket once, in the order of the
/// interfaces and their slots.
fn sockets(tcp: bool, v6: bool) -> Result<Vec<Shown, radix_mm::Kmalloc>, fmt::Error> {
    // Room is made before any stack is locked, and what has no room is
    // left out: a socket made meanwhile.
    let count: usize = interfaces().map(|interface| interface.stack.lock().sockets().count()).sum();
    let mut shown = Vec::new_in(kmalloc());
    shown.try_reserve_exact(count + 16).map_err(|_| fmt::Error)?;
    for interface in interfaces() {
        let stack = interface.stack.lock();
        for socket in stack.sockets().filter(|socket| socket.tcp == tcp && socket.is_ipv6() == v6) {
            // Ports are taken one socket each, so one at the same place
            // is the same socket.
            let once = !tcp || socket.state == State::Listen;
            let seen = once
                && shown.iter().any(|other: &Shown| {
                    other.local == socket.local && other.state == socket.state
                });
            if !seen && shown.push_within_capacity(socket).is_err() {
                break;
            }
        }
    }
    Ok(shown)
}

/// Writes `address` and `port` as a socket file does: IPv4's for `v6`
/// false, else IPv6's, an IPv4 address mapped (::ffff:10.0.2.15); none
/// is any.
fn write_end(text: &mut Text, address: Option<IpAddress>, port: u16, v6: bool) -> fmt::Result {
    if !v6 {
        let address = match address {
            Some(IpAddress::Ipv4(address)) => address,
            _ => Ipv4Address::UNSPECIFIED,
        };
        return write!(text, "{:08X}:{port:04X}", hex(address));
    }
    let address = match address {
        Some(IpAddress::Ipv6(address)) => address,
        Some(IpAddress::Ipv4(address)) => address.to_ipv6_mapped(),
        None => Ipv6Address::UNSPECIFIED,
    };
    for word in address.octets().as_chunks::<4>().0 {
        write!(text, "{:08X}", u32::from_le_bytes(*word))?;
    }
    write!(text, ":{port:04X}")
}

/// Writes `/proc/net/tcp`'s, `udp`'s, `tcp6`'s or `udp6`'s header and a
/// line for each socket.
fn socket_lines(text: &mut Text, tcp: bool, v6: bool) -> fmt::Result {
    let header = match (tcp, v6) {
        (true, false) => {
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode"
        }
        (false, false) => {
            "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops"
        }
        (true, true) => {
            "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode"
        }
        (false, true) => {
            "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops"
        }
    };
    writeln!(text, "{header}")?;
    for (line, socket) in sockets(tcp, v6)?.iter().enumerate() {
        let state = if tcp { state_number(socket.state) } else { 0x07 };
        let width = if tcp || v6 { 4 } else { 5 };
        write!(text, "{line:>width$}: ")?;
        write_end(text, socket.local.0, socket.local.1, v6)?;
        text.write_char(' ')?;
        let (address, port) = socket.remote.map_or((None, 0), |(a, p)| (Some(a), p));
        write_end(text, address, port, v6)?;
        writeln!(
            text,
            " {state:02X} {:08X}:{:08X} 00:00000000 00000000 {:>5} {:>8} 0",
            socket.send_queue, socket.receive_queue, 0, 0,
        )?;
    }
    Ok(())
}

/// `/processes/net/tcp`: every TCP socket of IPv4's, listening or
/// connected, and the connections closed that are still ending.
pub fn tcp(text: &mut Text) -> fmt::Result {
    socket_lines(text, true, false)
}

/// `/processes/net/udp`: every UDP socket of IPv4's that is bound.
pub fn udp(text: &mut Text) -> fmt::Result {
    socket_lines(text, false, false)
}

/// `/processes/net/tcp6`: likewise, IPv6's, and those on any address
/// that take both.
pub fn tcp6(text: &mut Text) -> fmt::Result {
    socket_lines(text, true, true)
}

/// `/processes/net/udp6`: likewise.
pub fn udp6(text: &mut Text) -> fmt::Result {
    socket_lines(text, false, true)
}
