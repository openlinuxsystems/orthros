// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Btrfs (Phase 10b and 10c): Dukovo's own filesystem.
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
//! - [`inode`], [`dir`] and [`extent`]: the items a subvolume's tree
//!   keeps for its files, directories and data.
//! - [`inflate`], [`lzo`] and [`zstd`]: the three compressions of data,
//!   decoded from their specifications (RFC 1950 and 1951, the LZO1X
//!   format Linux documents, RFC 8878), and [`unpack`], which uncompresses
//!   extents with them.
//! - [`fs::BtrfsFs`] and [`node::BtrfsNode`]: a mounted filesystem, read
//!   through the VFS.
//! - [`cow::Writer`]: changing the trees by copying on write, in
//!   transactions that [`space`] gives room and `commit` makes durable
//!   (ADR 0129).
//!
//! The crate is a compartment: it has no `unsafe` code, and builds for the
//! host (feature `host`).

#![cfg_attr(not(any(test, kani, feature = "host")), no_std)]
#![forbid(unsafe_code)]
#![feature(allocator_ext)]
#![feature(vec_push_within_capacity)]

extern crate alloc;

mod balance;
mod blake2b;
pub mod chunk;
mod commit;
pub mod cow;
pub mod csum;
pub mod dir;
mod edit;
pub mod extent;
mod files;
pub mod format;
pub mod fs;
mod grow;
pub mod inflate;
pub mod inode;
mod log;
pub mod lzo;
pub mod node;
mod pack;
mod quota;
mod refs;
mod replace;
mod replay;
mod scrub;
mod sha256;
pub mod space;
mod subvol;
pub mod superblock;
pub mod tree;
mod trim;
pub mod unpack;
pub mod volume;
pub mod zstd;

#[cfg(test)]
mod tests;

pub use fs::BtrfsFs;
pub use volume::Volume;

use radix_vfs::mem::{self, KVec, NoMemory};

/// `len` zero bytes on the heap (the kernel's `alloc` has no `resize`,
/// which cannot fail).
pub(crate) fn zeroed(len: usize) -> Result<KVec<u8>, NoMemory> {
    filled(len, 0)
}

/// `len` copies of `value` on the heap.
pub(crate) fn filled<T: Copy>(len: usize, value: T) -> Result<KVec<T>, NoMemory> {
    let mut v = mem::vec_with_capacity(len)?;
    for _ in 0..len {
        // Room was made above.
        let _ = v.push_within_capacity(value);
    }
    Ok(v)
}
