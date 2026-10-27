// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Btrfs (Phase 10b): Dukovo's own filesystem, read first.
//!
//! Btrfs keeps everything in copy-on-write B-trees, checksums every block
//! of metadata and data, and spreads them over its devices in chunks.
//! This crate implements it from the format's published documentation
//! (btrfs.readthedocs.io: "On-disk format" and "Tree checker"), never
//! from GPL code, and checks it against the reference tools: the tests
//! make filesystems with `mkfs.btrfs` on the host and read them back.
//!
//! - [`csum`]: the four checksums, all in this crate.
//! - [`format`]: keys, tree and item numbers, root items.
//! - [`superblock`]: the superblock and the features Radix reads.
//! - [`chunk`]: where logical addresses lie on the device.
//! - [`tree`]: reading and checking tree blocks, and searching trees.
//! - [`volume::Volume`]: an opened device, its chunks and its trees.
//!
//! The crate is a compartment: it has no `unsafe` code, and builds for the
//! host (feature `host`).

#![cfg_attr(not(any(test, kani, feature = "host")), no_std)]
#![forbid(unsafe_code)]
#![feature(vec_push_within_capacity)]

extern crate alloc;

mod blake2b;
pub mod chunk;
pub mod csum;
pub mod format;
mod sha256;
pub mod superblock;
pub mod tree;
pub mod volume;

#[cfg(test)]
mod tests;

pub use volume::Volume;

use radix_vfs::mem::{self, KVec, NoMemory};

/// `len` zero bytes on the heap (the kernel's `alloc` has no `resize`,
/// which cannot fail).
pub(crate) fn zeroed(len: usize) -> Result<KVec<u8>, NoMemory> {
    let mut v = mem::vec_with_capacity(len)?;
    for _ in 0..len {
        // Room was made above.
        let _ = v.push_within_capacity(0);
    }
    Ok(v)
}
