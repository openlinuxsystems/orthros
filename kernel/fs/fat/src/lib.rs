// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! FAT12, FAT16 and FAT32, read-write, with VFAT long names (Phase 7).
//!
//! FAT is the filesystem every computer reads: USB sticks, SD cards and
//! the EFI system partition use it, so Radix needs it from its first disk
//! on. This crate implements it from Microsoft's published specification
//! ("FAT: General Overview of On-Disk Format", version 1.03) and is
//! checked against the reference tools: the tests format volumes with
//! `mkfs.fat`, check what Radix wrote with `fsck.fat`, and read it back
//! with mtools, all on the host.
//!
//! - [`bpb`] reads the boot sector: where the tables, the root directory
//!   and the data are.
//! - [`table`] packs and unpacks the allocation table's entries.
//! - [`name`] holds directory entries, short and long names.
//! - [`fs::FatFs`] is the filesystem: its metadata (tables, directories)
//!   goes through a cache of the device's sectors
//!   ([`radix_block::cache::BlockCache`]); file data goes through each
//!   file's page cache, straight between its pages and the clusters on
//!   the disk.
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

pub mod bpb;
pub mod chain;
mod dir;
pub mod fs;
pub mod name;
pub mod node;
pub mod table;
pub mod time;

#[cfg(test)]
mod tests;

pub use fs::FatFs;
pub use node::FatNode;
