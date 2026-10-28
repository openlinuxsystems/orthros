// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! exFAT, read-write (Phase 10d).
//!
//! exFAT is what SD cards over 32 GB and most USB sticks come formatted
//! with, and what cameras and phones write: FAT's successor for media too
//! big for FAT32's 4 GiB files. This crate implements it from Microsoft's
//! published specification ("exFAT file system specification", 2019)
//! and is checked against the reference tools: the tests format volumes
//! with exfatprogs' `mkfs.exfat` and check what Radix wrote with its
//! `fsck.exfat`, on the host. ADR 0140 records the decision to ship it
//! and the patents that cover it.
//!
//! - [`boot`] reads the boot region and checks its checksum: where the
//!   table, the cluster heap and the root directory are.
//! - [`entry`] holds directory entries: a file is a set of them (its
//!   attributes and times, its stream with the size and first cluster,
//!   its name in pieces) with a checksum over the set.
//! - [`upcase`] holds the volume's up-case table, which decides which
//!   names are the same.
//! - [`fs::ExfatFs`] is the filesystem: its metadata (the table, the
//!   allocation bitmap, directories) goes through a cache of the
//!   device's sectors ([`radix_block::cache::BlockCache`]); file data goes
//!   through each file's page cache, straight between its pages and the
//!   clusters on the disk.
//!
//! Unlike FAT, exFAT keeps which clusters are free in a bitmap, and a
//! file whose clusters lie in one run needs no table entries at all: the
//! table is used only by files in pieces. Radix keeps new files in one
//! run while the clusters after them are free, as Linux and Windows do.
//!
//! The crate is a compartment: it has no `unsafe` code, and builds for the
//! host (feature `host`).

#![cfg_attr(not(any(test, kani, feature = "host")), no_std)]
#![forbid(unsafe_code)]
#![cfg_attr(not(kani), feature(allocator_ext))]
// Kani's own, older nightly has the allocator API unstable still.
#![cfg_attr(kani, feature(allocator_api))]
#![feature(vec_push_within_capacity)]

extern crate alloc;

pub mod boot;
mod dir;
pub mod entry;
pub mod fs;
pub mod node;
pub mod upcase;

#[cfg(test)]
mod tests;

pub use fs::ExfatFs;
pub use node::ExfatNode;
