// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/kernel/class/net` (ADR 0190): the network interfaces, as Linux's
//! `/sys/class/net` shows them, which ip(8) and the C library's
//! `if_nametoindex` and `getifaddrs` read.
//!
//! Each interface (`lo`, `net0`...) has Linux's files: `ifindex` and
//! `iflink`, its number (radix-network(2)); `address` and `broadcast`,
//! its MAC address and the one that reaches every card (all zeros for
//! `lo`), and `addr_len`; `mtu`; `type`, ARPHRD_ETHER (1) or
//! ARPHRD_LOOPBACK (772); `flags`, IFF_UP with IFF_BROADCAST for a card
//! or IFF_LOOPBACK for `lo`, in hexadecimal; `operstate` (`up`, `down`
//! for a card that stopped with an error, `unknown` for `lo`) and
//! `carrier`; and in `statistics`, what it sent and received. And one of
//! Radix's own, `ipv4`, its IPv4 addresses with their prefixes, a line
//! each, the first the one it sends from, which Linux gives only through
//! netlink.

use core::fmt::{self, Write};

use radix_net::Counters;
use radix_vfs::synthetic::{FixedDir, ListedDir, Listing};
use radix_vfs::{Error, Kind, NodeRef, Result, Text, mem, next_id};

use super::value;
use crate::net::{self, Interface};

/// Adds `net` to `class`.
pub(super) fn add(class: &FixedDir) -> Result<()> {
    let interfaces: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(Interfaces)?;
    class.add("net", ListedDir::make(next_id(), interfaces)?)
}

/// `net`: a directory for each interface.
struct Interfaces;

impl Listing for Interfaces {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        for interface in net::interfaces() {
            if !f(interface.name.as_str(), Kind::Directory) {
                return;
            }
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let interface = net::interfaces()
            .find(|interface| interface.name.as_str() == name)
            .ok_or(Error::NotFound)?;
        interface_dir(interface)
    }
}

/// What one of an interface's files says.
type Field = fn(&'static Interface, &mut Text) -> fmt::Result;

/// Linux's ARPHRD_ETHER and ARPHRD_LOOPBACK, an interface's `type`.
const ETHER: u16 = 1;
const LOOPBACK: u16 = 772;

/// Linux's IFF_UP, IFF_BROADCAST and IFF_LOOPBACK.
const UP: u32 = 0x1;
const BROADCAST: u32 = 0x2;
const IS_LOOPBACK: u32 = 0x8;

/// An interface's files, as Linux's.
const FILES: [(&str, Field); 11] = [
    ("ifindex", |i, t| writeln!(t, "{}", i.index())),
    ("iflink", |i, t| writeln!(t, "{}", i.index())),
    ("address", |i, t| write_mac(t, i.stack.lock().mac().map_or([0; 6], |mac| mac.0))),
    ("broadcast", |i, t| write_mac(t, if i.is_loopback() { [0; 6] } else { [0xff; 6] })),
    ("addr_len", |_, t| writeln!(t, "6")),
    ("mtu", |i, t| writeln!(t, "{}", i.stack.lock().mtu())),
    ("type", |i, t| writeln!(t, "{}", if i.is_loopback() { LOOPBACK } else { ETHER })),
    ("flags", |i, t| {
        writeln!(t, "{:#x}", UP | if i.is_loopback() { IS_LOOPBACK } else { BROADCAST })
    }),
    ("operstate", |i, t| writeln!(t, "{}", operstate(i))),
    ("carrier", |i, t| writeln!(t, "{}", u8::from(operstate(i) != "down"))),
    ("ipv4", |i, t| {
        // `lo` has the cards' addresses after its own, to reach them,
        // which Linux keeps in its local table: they are the cards'.
        let own = if i.is_loopback() { 1 } else { usize::MAX };
        for cidr in i.stack.lock().ipv4_addresses().take(own) {
            writeln!(t, "{cidr}")?;
        }
        Ok(())
    }),
];

/// What one of the counts in `statistics` says.
type Statistic = fn(&Counters) -> u64;

/// The files of `statistics`, as Linux's: what the stack counts, and
/// zero for the errors it does not.
const STATISTICS: [(&str, Statistic); 8] = [
    ("rx_bytes", |c| c.received_bytes),
    ("rx_packets", |c| c.received_packets),
    ("rx_errors", |_| 0),
    ("rx_dropped", |_| 0),
    ("tx_bytes", |c| c.sent_bytes),
    ("tx_packets", |c| c.sent_packets),
    ("tx_errors", |_| 0),
    ("tx_dropped", |c| c.send_dropped),
];

/// `operstate`: `lo` has no link to be up or down, as on Linux; a card's
/// is down once it stopped with an error, and one a program drives while
/// no driver has attached it or its driver says so (ADR 0198).
fn operstate(interface: &Interface) -> &'static str {
    if interface.is_loopback() {
        "unknown"
    } else if interface.stack.lock().link().down() {
        "down"
    } else {
        "up"
    }
}

/// Writes `mac` as Linux does, `52:54:00:12:34:56`, and a newline.
fn write_mac(text: &mut Text, mac: [u8; 6]) -> fmt::Result {
    let [a, b, c, d, e, f] = mac;
    writeln!(text, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}")
}

/// An interface's directory.
fn interface_dir(interface: &'static Interface) -> Result<NodeRef> {
    let dir = FixedDir::new(next_id())?;
    for (name, field) in FILES {
        dir.add(name, value(move |text| field(interface, text))?)?;
    }
    let statistics = dir.add_dir("statistics", next_id())?;
    for (name, count) in STATISTICS {
        statistics.add(
            name,
            value(move |text| writeln!(text, "{}", count(&interface.stack.lock().counters())))?,
        )?;
    }
    Ok(dir)
}
