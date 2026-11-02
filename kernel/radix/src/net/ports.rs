// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Which TCP and UDP ports sockets hold (ADR 0183): one socket a port, of
//! each protocol, whatever address it is bound to. A socket holds its
//! port from when it is bound until it is closed, and a connection made
//! from a port the kernel picked holds it until its last packets have
//! gone, so that a new connection does not take the place of one still
//! closing. The connections a listening socket takes hold none of their
//! own.
//!
//! Sockets bound to an interface (SO_BINDTODEVICE) before they take a
//! port may share it, one an interface, as on Linux: a DHCP client has a
//! socket on port 68 of each card (ADR 0191). A port shared so is held
//! while any of them holds it, and a socket bound to no interface cannot
//! take it meanwhile.
//!
//! IPv4 and IPv6 have ports of their own (ADR 0331): a socket of
//! AF_INET holds IPv4's, one of AF_INET6 that is IPV6_V6ONLY IPv6's, and
//! one of AF_INET6 that takes both holds the port in both, as on Linux,
//! so that a server may listen on port 22 over both through one socket
//! or two.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use radix_abi::network::EPHEMERAL_FIRST;
use radix_sync::Mutex;

/// A transport protocol, which has ports of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

/// The versions of IP whose ports a socket holds: one, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Versions {
    Ipv4,
    Ipv6,
    Both,
}

impl Versions {
    /// Whether they are `version`'s, IPv6's when `v6`.
    fn has(self, v6: bool) -> bool {
        match self {
            Versions::Ipv4 => !v6,
            Versions::Ipv6 => v6,
            Versions::Both => true,
        }
    }

    /// Each version, as whether it is IPv6.
    fn each(self) -> impl Iterator<Item = bool> {
        [false, true].into_iter().filter(move |&v6| self.has(v6))
    }
}

/// A bit for each port.
struct Table {
    words: [AtomicU64; 1024],
}

/// Where the next search for a port of the kernel's own starts.
struct Cursor(AtomicUsize);

/// TCP's and UDP's tables, IPv4's then IPv6's.
static TABLES: [Table; 4] = [const { Table { words: [const { AtomicU64::new(0) }; 1024] } }; 4];
static CURSORS: [Cursor; 2] = [const { Cursor(AtomicUsize::new(0)) }; 2];

fn table(protocol: Protocol, v6: bool) -> &'static Table {
    &TABLES[protocol as usize * 2 + usize::from(v6)]
}

/// The ports the kernel picks from: EPHEMERAL_FIRST to 65,535.
const EPHEMERAL: usize = 65_536 - EPHEMERAL_FIRST as usize;

/// The most ports held by sockets bound to an interface at once.
const SHARED_MAX: usize = 64;

/// A port a socket bound to an interface holds, of one version of IP:
/// its bit is set while any is here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shared {
    protocol: Protocol,
    v6: bool,
    port: u16,
    device: u32,
}

/// The ports held by sockets bound to an interface. Nothing is locked
/// under it.
static SHARED: Mutex<[Option<Shared>; SHARED_MAX]> = Mutex::new([None; SHARED_MAX]);

/// A port a socket holds, until it is dropped.
#[derive(Debug)]
pub struct Held {
    protocol: Protocol,
    versions: Versions,
    port: u16,
    /// The interface it is held on, which others may hold it on too; 0
    /// if the socket holds it alone.
    device: u32,
}

impl Held {
    /// The port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether others may hold it too, on other interfaces.
    pub fn shared(&self) -> bool {
        self.device != 0
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        for v6 in self.versions.each() {
            release(self.protocol, v6, self.port, self.device);
        }
    }
}

/// Lets go of `port` of `protocol` and one version of IP, held alone or
/// on `device`.
fn release(protocol: Protocol, v6: bool, port: u16, device: u32) {
    let (word, bit) = place(port);
    let words = &table(protocol, v6).words;
    if device == 0 {
        words[word].fetch_and(!bit, Ordering::Release);
        return;
    }
    let mut shared = SHARED.lock();
    let ours = Some(Shared { protocol, v6, port, device });
    if let Some(slot) = shared.iter_mut().find(|s| **s == ours) {
        *slot = None;
    }
    let held = |s: &Shared| s.protocol == protocol && s.v6 == v6 && s.port == port;
    if !shared.iter().flatten().any(held) {
        words[word].fetch_and(!bit, Ordering::Release);
    }
}

fn place(port: u16) -> (usize, u64) {
    (usize::from(port) / 64, 1 << (port % 64))
}

/// Takes `port` (1 to 65,535) of `protocol` and `versions`, unless a
/// socket holds it in any of them.
pub fn take(protocol: Protocol, versions: Versions, port: u16) -> Option<Held> {
    take_on(protocol, versions, port, 0)
}

/// Takes `port` (1 to 65,535) of `protocol` and `versions` on the
/// interface numbered `device`, for a socket bound to it, unless a
/// socket bound to no interface or to that one holds it; on 0, alone.
pub fn take_on(protocol: Protocol, versions: Versions, port: u16, device: u32) -> Option<Held> {
    if port == 0 {
        return None;
    }
    let mut taken = [false; 2];
    for v6 in versions.each() {
        if !take_one(protocol, v6, port, device) {
            // Those taken go back.
            for (v6, _) in [false, true].into_iter().zip(taken).filter(|(_, t)| *t) {
                release(protocol, v6, port, device);
            }
            return None;
        }
        taken[usize::from(v6)] = true;
    }
    Some(Held { protocol, versions, port, device })
}

/// Takes `port` of `protocol` and one version of IP, as [`take_on`].
fn take_one(protocol: Protocol, v6: bool, port: u16, device: u32) -> bool {
    let (word, bit) = place(port);
    let words = &table(protocol, v6).words;
    if device == 0 {
        // Another socket's, if set: no hold is made, as dropping one
        // frees the port.
        return words[word].fetch_or(bit, Ordering::AcqRel) & bit == 0;
    }
    let mut shared = SHARED.lock();
    let before = words[word].fetch_or(bit, Ordering::AcqRel);
    let mut holders =
        shared.iter().flatten().filter(|s| s.protocol == protocol && s.v6 == v6 && s.port == port);
    let first = holders.next();
    if before & bit != 0 {
        // Held: by sockets on other interfaces alone, or it is not ours.
        let on = |s: &Shared| s.device == device;
        if first.is_none_or(on) || holders.any(on) {
            return false;
        }
    }
    let Some(free) = shared.iter_mut().find(|s| s.is_none()) else {
        if before & bit == 0 {
            words[word].fetch_and(!bit, Ordering::Release);
        }
        return false;
    };
    *free = Some(Shared { protocol, v6, port, device });
    true
}

/// Takes a port of `protocol` and `versions` that no socket holds, from
/// those the kernel picks from. For TCP, the next free one after a place
/// chosen at random the first time, so that the ports a machine uses are
/// not guessed from its boot, and one just closed is not soon used again
/// for a connection still closing at the other end. For UDP, the first
/// free one from a place chosen at random each time (RFC 6056's first
/// algorithm), as Linux picks them: a DNS answer is taken for a query's
/// by its port and a 16-bit number, which together an attacker off the
/// path must guess (ADR 0193).
pub fn take_any(protocol: Protocol, versions: Versions) -> Option<Held> {
    let cursor = &CURSORS[protocol as usize].0;
    if protocol == Protocol::Udp {
        cursor.store(radix_random::u64() as usize % EPHEMERAL, Ordering::Relaxed);
    } else if cursor.load(Ordering::Relaxed) == 0 {
        let start = (radix_random::u64() % EPHEMERAL as u64) as usize + 1;
        let _ = cursor.compare_exchange(0, start, Ordering::Relaxed, Ordering::Relaxed);
    }
    for _ in 0..EPHEMERAL {
        let next = cursor.fetch_add(1, Ordering::Relaxed);
        let port = EPHEMERAL_FIRST as usize + next % EPHEMERAL;
        if let Some(held) = take(protocol, versions, port as u16) {
            return Some(held);
        }
    }
    None
}
