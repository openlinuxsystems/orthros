// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! NTFS, read (Phase 11f.1) and written (11f.2 to 11f.4).
//!
//! NTFS is what Windows formats its disks with, and many USB disks come
//! with. Microsoft publishes no specification of it; this crate is built
//! from the descriptions others have published of the format (the
//! Linux-NTFS project's documentation, Microsoft's own [MS-FSCC] for
//! reparse points and [MS-XCA] for its compression), with no code of
//! Linux's or ntfs-3g's, which are GPL. It is checked against ntfs-3g's
//! tools: the tests format volumes with `mkntfs`, write files with
//! `ntfscp`, and read a volume ntfs-3g filled through FUSE; what Radix
//! writes, `ntfsresize --info`, `ntfsfix -n` and The Sleuth Kit check.
//! In CI, Windows' own `chkdsk` checks the volumes the tests write
//! (`windows_tests`). ADRs 0236 (reading), 0237 (writing), 0238
//! (compressed and sparse data, links, the hibernation override and
//! power cuts) and 0239 (attribute lists) record the decisions.
//!
//! - [`boot`] reads the boot sector: how big clusters and records are,
//!   and where the master file table is.
//! - [`record`] reads file records and their attributes, after putting
//!   back what their update sequence stands in for.
//! - [`runs`] reads run lists, where non-resident attributes' clusters
//!   are.
//! - [`upcase`] holds the volume's up-case table, which orders names.
//! - [`index`] walks directory indexes, B+ trees of names, and [`tree`]
//!   changes them.
//! - [`edit`] changes file records: attributes made, resized and taken
//!   out; [`list`] makes the attribute list of a file whose data is
//!   spread over more records.
//! - [`space`] finds free clusters in `$Bitmap` and free records in the
//!   master file table's bitmap.
//! - [`names`] makes the names and times Radix writes.
//! - [`lznt1`] uncompresses and compresses compressed files.
//! - [`reparse`] reads reparse points, links and junctions, and makes
//!   links.
//! - [`fs::NtfsFs`] is the filesystem; [`node::NtfsNode`] its files,
//!   directories and links. Metadata goes through a cache of the device's
//!   sectors ([`radix_block::cache::BlockCache`]); file data goes through
//!   each file's page cache, straight from the clusters on the disk.
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
pub mod edit;
pub mod fs;
pub mod index;
pub mod list;
pub mod lznt1;
pub mod names;
pub mod node;
pub mod record;
pub mod reparse;
pub mod runs;
pub mod space;
pub mod tree;
pub mod upcase;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tree_tests;
#[cfg(test)]
mod windows_tests;
#[cfg(test)]
mod write_tests;

pub use fs::NtfsFs;
pub use node::NtfsNode;
