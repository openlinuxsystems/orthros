// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Which TCP and UDP ports sockets hold (ADR 0183): one socket a port, of
//! each protocol, whatever address it is bound to. A socket holds its
//! port from when it is bound until it is closed, and a connection made
//! from a port the kernel picked holds it until its last packets have
//! gone, so that a new connection does not take the place of one still
//! closing. The connections a listening socket takes hold none of their
//! own.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use radix_abi::network::EPHEMERAL_FIRST;

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

/// A port a socket holds, until it is dropped.
#[derive(Debug)]
pub struct Held {
    protocol: Protocol,
    port: u16,
}

impl Held {
    /// The port.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let (word, bit) = place(self.port);
        table(self.protocol).words[word].fetch_and(!bit, Ordering::Release);
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
    (before & bit == 0).then_some(Held { protocol, port })
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
