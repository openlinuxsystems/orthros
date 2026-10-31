// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/processes/net` (ADR 0190): the interfaces' counts, the routes and
//! the sockets, in the formats of Linux's `/proc/net/dev`, `route`, `tcp`
//! and `udp`, which ss(8) and ported tools read.
//!
//! Addresses and ports are in hexadecimal as Linux writes them: an
//! address is its four bytes, in network order, read as a little-endian
//! number (10.0.2.15 is `0F02000A`), and a port is a number (port 80 is
//! `0050`). A socket that listens, or a datagram socket, is a smoltcp
//! socket on each interface it is on, and on several for a listening
//! socket's pool; it is shown once, as Linux shows it. The user is root's
//! (0) and the inode 0, until users come (Phase 13).

use alloc::vec::Vec;
use core::fmt::{self, Write};

use radix_net::smoltcp::socket::tcp::State;
use radix_net::{Ipv4Address, Shown};
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
const ADDRESSES: usize = 8;

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

/// The sockets of every interface of `tcp`'s protocol, each listening or
/// datagram socket once, in the order of the interfaces and their slots.
fn sockets(tcp: bool) -> Result<Vec<Shown, radix_mm::Kmalloc>, fmt::Error> {
    // Room is made before any stack is locked, and what has no room is
    // left out: a socket made meanwhile.
    let count: usize = interfaces().map(|interface| interface.stack.lock().sockets().count()).sum();
    let mut shown = Vec::new_in(kmalloc());
    shown.try_reserve_exact(count + 16).map_err(|_| fmt::Error)?;
    for interface in interfaces() {
        let stack = interface.stack.lock();
        for socket in stack.sockets().filter(|socket| socket.tcp == tcp) {
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

/// Writes `/proc/net/tcp`'s or `udp`'s header and a line for each socket.
fn socket_lines(text: &mut Text, tcp: bool) -> fmt::Result {
    let header = if tcp {
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode"
    } else {
        "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops"
    };
    writeln!(text, "{header}")?;
    for (line, socket) in sockets(tcp)?.iter().enumerate() {
        let (local, remote) = (socket.local, socket.remote);
        let state = if tcp { state_number(socket.state) } else { 0x07 };
        let width = if tcp { 4 } else { 5 };
        writeln!(
            text,
            "{line:>width$}: {:08X}:{:04X} {:08X}:{:04X} {state:02X} {:08X}:{:08X} 00:00000000 00000000 {:>5} {:>8} 0",
            hex(local.0),
            local.1,
            hex(remote.0),
            remote.1,
            socket.send_queue,
            socket.receive_queue,
            0,
            0,
        )?;
    }
    Ok(())
}

/// `/processes/net/tcp`: every TCP socket, listening or connected, and
/// the connections closed that are still ending.
pub fn tcp(text: &mut Text) -> fmt::Result {
    socket_lines(text, true)
}

/// `/processes/net/udp`: every UDP socket that is bound.
pub fn udp(text: &mut Text) -> fmt::Result {
    socket_lines(text, false)
}
