// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The loopback interface's link (ADR 0183): every packet sent on it
//! comes back to be received, in order, and none leaves the machine.

use crate::{Link, Medium};

/// The bytes before each packet in the ring: its length.
const LENGTH: usize = 2;

/// A link that hands back what is sent on it: IP packets, each queued in
/// a ring of bytes after its length, until the stack receives it.
pub struct Loopback {
    ring: &'static mut [u8],
    /// Where the oldest packet's length starts.
    head: usize,
    /// How many bytes of the ring are in use, from `head`.
    used: usize,
    /// The longest packet it takes.
    mtu: usize,
}

impl Loopback {
    /// A loopback link for packets of up to `mtu` bytes (at most 65,535),
    /// which wait in `ring`.
    pub fn new(ring: &'static mut [u8], mtu: usize) -> Loopback {
        Loopback { ring, head: 0, used: 0, mtu: mtu.min(usize::from(u16::MAX)) }
    }

    /// Copies `bytes` into the ring from `at`, round its end.
    fn put(&mut self, at: usize, bytes: &[u8]) {
        let len = self.ring.len();
        let start = at % len;
        let first = bytes.len().min(len - start);
        self.ring[start..start + first].copy_from_slice(&bytes[..first]);
        self.ring[..bytes.len() - first].copy_from_slice(&bytes[first..]);
    }

    /// Copies the ring's bytes from `at` into `out`, round its end.
    fn get(&self, at: usize, out: &mut [u8]) {
        let len = self.ring.len();
        let start = at % len;
        let first = out.len().min(len - start);
        out[..first].copy_from_slice(&self.ring[start..start + first]);
        let rest = out.len() - first;
        out[first..].copy_from_slice(&self.ring[..rest]);
    }
}

impl Link for Loopback {
    fn medium(&self) -> Medium {
        Medium::Ip
    }

    fn pending(&self) -> bool {
        self.used > 0
    }

    fn mac(&self) -> [u8; 6] {
        [0; 6]
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.used == 0 {
            return None;
        }
        let mut length = [0; LENGTH];
        self.get(self.head, &mut length);
        let len = usize::from(u16::from_le_bytes(length));
        // What `frame` has no room for is lost, as on a card.
        let kept = len.min(frame.len());
        self.get(self.head + LENGTH, &mut frame[..kept]);
        self.head = (self.head + LENGTH + len) % self.ring.len();
        self.used -= LENGTH + len;
        Some(kept)
    }

    fn can_send(&mut self) -> bool {
        self.ring.len() - self.used >= LENGTH + self.mtu
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        let Ok(len) = u16::try_from(frame.len()) else { return false };
        if frame.len() > self.mtu || self.ring.len() - self.used < LENGTH + frame.len() {
            return false;
        }
        let tail = self.head + self.used;
        self.put(tail, &len.to_le_bytes());
        self.put(tail + LENGTH, frame);
        self.used += LENGTH + frame.len();
        true
    }

    fn flush(&mut self) {}

    fn needs_checksums(&self) -> bool {
        false
    }
}
