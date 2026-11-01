// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! UDF, read-only (Phase 11g, ADR 0245): the filesystem of DVDs, Blu-ray
//! discs and the images made of them, and of some disks shared between
//! systems.
//!
//! The crate is written from ECMA-167 3rd edition (the free text of ISO
//! 13346) and the OSTA Universal Disk Format specification, revision
//! 2.60, and checked against udftools' `mkudffs` and genisoimage, which
//! make the tests' images.
//!
//! - [`format`] reads descriptor tags, allocation descriptors,
//!   timestamps and OSTA compressed Unicode names.
//! - [`volume`] finds the volume: its recognition sequence, its anchor,
//!   its volume descriptors and its partition maps, physical, sparable
//!   (rewritable discs), virtual (CD-R and other write-once discs, with
//!   their virtual allocation table) and metadata (UDF 2.50 and later),
//!   and maps a partition's blocks to the device's bytes.
//! - [`fs::UdfFs`] is the filesystem: file entries and extended file
//!   entries, their allocation descriptors (short, long, extended, or
//!   the data in the entry itself), directories of file identifier
//!   descriptors, and symbolic links. Metadata goes through a cache of
//!   the device's sectors; file data through each file's page cache,
//!   straight from its extents on the disk.
//!
//! Nothing is ever written: a mount to write fails with
//! [`radix_vfs::Error::ReadOnly`], and the mount is made read-only.
//!
//! The crate is a compartment: it has no `unsafe` code, and builds for
//! the host (feature `host`).

#![cfg_attr(not(any(test, feature = "host")), no_std)]
#![forbid(unsafe_code)]
#![feature(allocator_ext)]
#![feature(vec_push_within_capacity)]

extern crate alloc;

pub mod format;
mod fs;
mod node;
pub mod volume;

#[cfg(test)]
mod tests;

pub use fs::UdfFs;
pub use node::UdfNode;
