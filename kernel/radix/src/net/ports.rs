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

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use radix_abi::network::EPHEMERAL_FIRST;
use radix_sync::Mutex;

/// A transport protocol, which has ports of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

/// A bit for each port.
struct Table {
    words: [AtomicU64; 1024],
    /// Where the next search for a port of the kernel's own starts.
    cursor: AtomicUsize,
}

static TCP: Table =
    Table { words: [const { AtomicU64::new(0) }; 1024], cursor: AtomicUsize::new(0) };
static UDP: Table =
    Table { words: [const { AtomicU64::new(0) }; 1024], cursor: AtomicUsize::new(0) };

fn table(protocol: Protocol) -> &'static Table {
    match protocol {
        Protocol::Tcp => &TCP,
        Protocol::Udp => &UDP,
    }
}

/// The ports the kernel picks from: EPHEMERAL_FIRST to 65,535.
const EPHEMERAL: usize = 65_536 - EPHEMERAL_FIRST as usize;

/// The most ports held by sockets bound to an interface at once.
const SHARED_MAX: usize = 64;

/// A port a socket bound to an interface holds: its bit is set while any
/// is here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shared {
    protocol: Protocol,
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
        let (word, bit) = place(self.port);
        let words = &table(self.protocol).words;
        if self.device == 0 {
            words[word].fetch_and(!bit, Ordering::Release);
            return;
        }
        let mut shared = SHARED.lock();
        forget(&mut shared[..], self.protocol, self.port, self.device);
        if !shared.iter().flatten().any(|s| s.protocol == self.protocol && s.port == self.port) {
            words[word].fetch_and(!bit, Ordering::Release);
        }
    }
}

/// Takes out of `shared` the hold of `port` of `protocol` on `device`.
fn forget(shared: &mut [Option<Shared>], protocol: Protocol, port: u16, device: u32) {
    let ours = Some(Shared { protocol, port, device });
    if let Some(slot) = shared.iter_mut().find(|s| **s == ours) {
        *slot = None;
    }
}

fn place(port: u16) -> (usize, u64) {
    (usize::from(port) / 64, 1 << (port % 64))
}

/// Takes `port` (1 to 65,535) of `protocol`, unless a socket holds it.
pub fn take(protocol: Protocol, port: u16) -> Option<Held> {
    if port == 0 {
        return None;
    }
    let (word, bit) = place(port);
    let before = table(protocol).words[word].fetch_or(bit, Ordering::AcqRel);
    if before & bit != 0 {
        // Another socket's: no hold is made, as dropping one frees the
        // port.
        return None;
    }
    Some(Held { protocol, port, device: 0 })
}

/// Takes `port` (1 to 65,535) of `protocol` on the interface numbered
/// `device`, for a socket bound to it, unless a socket bound to no
/// interface or to that one holds it; 0 is [`take`].
pub fn take_on(protocol: Protocol, port: u16, device: u32) -> Option<Held> {
    if device == 0 || port == 0 {
        return take(protocol, port);
    }
    let mut shared = SHARED.lock();
    let (word, bit) = place(port);
    let words = &table(protocol).words;
    let before = words[word].fetch_or(bit, Ordering::AcqRel);
    let mut holders = shared.iter().flatten().filter(|s| s.protocol == protocol && s.port == port);
    let first = holders.next();
    if before & bit != 0 {
        // Held: by sockets on other interfaces alone, or it is not ours.
        let on = |s: &Shared| s.device == device;
        if first.is_none_or(on) || holders.any(on) {
            return None;
        }
    }
    let Some(free) = shared.iter_mut().find(|s| s.is_none()) else {
        if before & bit == 0 {
            words[word].fetch_and(!bit, Ordering::Release);
        }
        return None;
    };
    *free = Some(Shared { protocol, port, device });
    Some(Held { protocol, port, device })
}

/// Takes a port of `protocol` that no socket holds, from those the kernel
/// picks from: the next free one after a place chosen at random the first
/// time, so that the ports a machine uses are not guessed from its boot.
pub fn take_any(protocol: Protocol) -> Option<Held> {
    let table = table(protocol);
    if table.cursor.load(Ordering::Relaxed) == 0 {
        let start = (radix_random::u64() % EPHEMERAL as u64) as usize + 1;
        let _ = table.cursor.compare_exchange(0, start, Ordering::Relaxed, Ordering::Relaxed);
    }
    for _ in 0..EPHEMERAL {
        let next = table.cursor.fetch_add(1, Ordering::Relaxed);
        let port = EPHEMERAL_FIRST as usize + next % EPHEMERAL;
        if let Some(held) = take(protocol, port as u16) {
            return Some(held);
        }
    }
    None
}
