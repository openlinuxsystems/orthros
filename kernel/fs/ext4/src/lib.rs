// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ext4, read-write, with its journal (Phase 10d).
//!
//! ext4 is Linux's default filesystem: most Linux installs, and many
//! external disks and servers, are formatted with it. This crate reads
//! and writes it from its on-disk format as the kernel's documentation
//! describes it ("ext4 Data Structures and Algorithms"), and is checked
//! against Linux's own tools: the tests format volumes with e2fsprogs'
//! `mkfs.ext4`, check what Radix wrote with its `e2fsck`, and read it back
//! with its `debugfs`; a volume Linux's kernel wrote, with transactions
//! left in its journal, is replayed and checked the same way. ADR 0141
//! records the design.
//!
//! - [`sb`] reads the superblock: the volume's geometry, its features and
//!   which of them Radix reads and writes, and its checksums.
//! - [`group`] holds the block groups' descriptors: where each group's
//!   bitmaps and inode table are, and how much of it is free.
//! - [`inode`] encodes and decodes inodes.
//! - [`extent`] holds a file's blocks as a list of extents in memory, and
//!   reads and writes the tree they are kept in on the disk.
//! - [`dir`] reads and changes directory blocks.
//! - [`xattr`] reads and changes extended attributes and access lists,
//!   in the inode and in a block of their own (ADR 0256).
//! - [`journal`] replays the journal (jbd2) at mount and writes a
//!   transaction to it at each sync.
//! - [`fs::Ext4Fs`] is the filesystem; [`node::Ext4Node`] a file or
//!   directory. Metadata goes through a cache of the device's blocks
//!   ([`radix_block::cache::BlockCache`]), where every change waits for the
//!   next commit; file data goes through each file's page cache, straight
//!   between its pages and its blocks.
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

mod alloc_space;
mod crc;
mod dir;
mod extent;
mod fs;
mod group;
mod inode;
mod journal;
mod node;
mod sb;
mod xattr;

#[cfg(test)]
mod tests;

pub use fs::Ext4Fs;
pub use node::Ext4Node;

/// The little-endian `u16` at `at` of `b`.
pub(crate) fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

/// The little-endian `u32` at `at` of `b`.
pub(crate) fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Sets the little-endian `u16` at `at` of `b`.
pub(crate) fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

/// Sets the little-endian `u32` at `at` of `b`.
pub(crate) fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// The big-endian `u32` at `at` of `b`: the journal's byte order.
pub(crate) fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Sets the big-endian `u32` at `at` of `b`.
pub(crate) fn putbe32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}
