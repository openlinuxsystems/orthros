// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Where the GUIDs, disk signatures and volume serial numbers come from:
//! the system's random numbers, or a seed, so that `./ob image` makes the
//! same image twice and the tests the same disks.

use std::fs::File;
use std::io::Read;

/// A source of random bytes: SplitMix64, seeded from the system's
/// generator or from a number. They name disks and volumes and need only
/// be different from each other's; no secret comes from them.
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Seeded from the system's generator: `/dev/urandom`, which Dukovo
    /// links to `/devices/urandom`. If it cannot be read, from the time.
    pub fn system() -> Rng {
        let mut seed = [0u8; 8];
        let read = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut seed));
        if read.is_err() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            seed = (now ^ u64::from(std::process::id()) << 32).to_le_bytes();
        }
        Rng::seeded(u64::from_le_bytes(seed))
    }

    /// Seeded from `seed`: the same bytes each time.
    pub fn seeded(seed: u64) -> Rng {
        Rng { state: seed }
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// The next 32 bits.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Fills `out`.
    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}
