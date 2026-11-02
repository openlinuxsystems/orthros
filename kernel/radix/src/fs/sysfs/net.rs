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
//! netlink; and `ipv6`, what its IPv6 has (ADR 0331), a line each: its
//! addresses, tested or not, with what each is and how many seconds it
//! is preferred and valid for (`address 2001:db8::5/64 temporary
//! preferred 86000 valid 172000`, `tentative` at the end while it is
//! tested), the routers that advertised themselves (`router fe80::1
//! lifetime 1800`) and the one it was given (`router fe80::1 given`),
//! the DNS servers they advertised (`dns 2001:db8::53
//! lifetime 600`), the MTU (`mtu 1500`) and whether DHCPv6 is to give
//! addresses (`dhcp managed`) or other settings (`dhcp other`), which
//! netd(8) reads.

use core::fmt::{self, Write};

use radix_net::Counters;
use radix_net::ipv6::Kind as Ipv6Kind;
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
const FILES: [(&str, Field); 12] = [
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
    ("ipv6", write_ipv6),
];

/// Seconds from `now` to `until`, rounded down, or `forever`.
struct Left(Option<u64>, u64);

impl fmt::Display for Left {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(until) if until != u64::MAX => {
                write!(f, "{}", until.saturating_sub(self.1) / 1_000_000_000)
            }
            _ => f.write_str("forever"),
        }
    }
}

/// `ipv6`: what the interface's IPv6 has.
fn write_ipv6(interface: &'static Interface, text: &mut Text) -> fmt::Result {
    let now = crate::sched::now_ns();
    let stack = interface.stack.lock();
    let ipv6 = stack.ipv6();
    // `lo` has the cards' addresses after its own, as for IPv4.
    let own = if interface.is_loopback() { 1 } else { usize::MAX };
    for address in ipv6.addresses().take(own) {
        let kind = match address.kind {
            Ipv6Kind::LinkLocal => "link-local",
            Ipv6Kind::Stable => "stable",
            Ipv6Kind::Temporary => "temporary",
            Ipv6Kind::Static => "given",
        };
        write!(
            text,
            "address {} {kind} preferred {} valid {}",
            address.cidr,
            Left(address.preferred_until, now),
            Left(address.valid_until, now)
        )?;
        writeln!(text, "{}", if address.tentative() { " tentative" } else { "" })?;
    }
    for router in ipv6.routers() {
        writeln!(text, "router {} lifetime {}", router.address, Left(Some(router.until), now))?;
    }
    if let Some(router) = ipv6.given_router() {
        writeln!(text, "router {router} given")?;
    }
    for server in ipv6.dns() {
        writeln!(text, "dns {} lifetime {}", server.address, Left(Some(server.until), now))?;
    }
    if let Some(mtu) = ipv6.mtu() {
        writeln!(text, "mtu {mtu}")?;
    }
    match ipv6.dhcp() {
        (true, _) => writeln!(text, "dhcp managed"),
        (false, true) => writeln!(text, "dhcp other"),
        (false, false) => Ok(()),
    }
}

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
