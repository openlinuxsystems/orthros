// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ISO 9660, read-only (Phase 11g, ADR 0245): the filesystem of CDs,
//! DVDs and the images made of them, which install media and software
//! still come as.
//!
//! The crate is written from ECMA-119 (the free text of ISO 9660), the
//! Joliet specification (Microsoft, 1995) and the System Use Sharing
//! Protocol and Rock Ridge Interchange Protocol (IEEE P1282 and P1281,
//! 1.12), and checked against xorriso, which makes the tests' images.
//!
//! - [`format`] reads the volume descriptors and directory records, and
//!   ISO 9660's dates.
//! - [`rock`] reads a record's System Use entries: Rock Ridge's POSIX
//!   names, modes, owners, times, symbolic links, device numbers, and
//!   the relocation of directories deeper than ISO 9660's eight levels.
//! - [`fs::IsoFs`] is the filesystem: it picks one of the volume's
//!   trees, Rock Ridge's if the primary volume has it, else Joliet's
//!   (Unicode names, as Windows writes them), else the primary one's
//!   (upper-case names of eight and three letters, shown in lower case
//!   as Linux shows them). Metadata goes through a cache of the device's
//!   sectors; file data through each file's page cache, straight from
//!   its extents on the disk.
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
pub mod rock;

#[cfg(test)]
mod tests;

pub use fs::IsoFs;
pub use node::IsoNode;
