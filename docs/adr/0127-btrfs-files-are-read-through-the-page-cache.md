<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0127. Btrfs files are read through the page cache and checked as Linux checks them

- Status: Accepted
- Date: 2026-10-27

## Context

ADR 0125 reads Btrfs's trees. Phase 10b.2 makes them a filesystem the
VFS mounts: files, directories, links and subvolumes, read-only until
Phase 10c writes them. Each needs a decision that keeps what Linux shows
of the same disk, since a Dukovo disk is meant to be read by Linux too
and a Linux one by Dukovo.

- A subvolume is a tree of its own, with its own inode numbers, found
  from a directory entry whose location is the subvolume's root item. On
  Linux each subvolume has its own `st_dev`, so the same inode number in
  two subvolumes is two files. Radix's VFS has one ID per file and no
  device per subvolume.
- A filesystem has a default subvolume, the one the root tree's
  `default` entry names; Linux mounts it when no `subvol=` is given.
- Data has a checksum per sector in the checksum tree, unless its file
  is `NODATASUM` (or `NODATACOW`, which implies it). Linux checks every
  sector it reads, reads another copy when one does not match (the DUP
  and RAID1 profiles keep two), and fails the read with EIO when none
  does, or when a sector that should have a checksum has none.
- A file's data is in extents: inline in the tree, on the disk, or
  reserved (`fallocate`) and so reading as zeros, with holes between.
  Extents may be compressed, with zlib, LZO or zstd.
- Btrfs's sector size is the unit of its checksums. Linux on x86-64 makes
  4 KiB sectors, the size of a page.

## Decision

- **The mount is read-only**, whatever it is asked, until 10c, as Linux
  mounts a disk that refuses writes. It mounts the default subvolume. It
  needs sectors of 4 KiB, one per page, and a disk whose blocks are no
  larger; a filesystem of other sectors is refused with `NotSupported`,
  as is one on more than one device or with a log to replay (ADR 0125).
- **Nodes** come from a table keyed by subvolume and inode number, of
  weak references, so that every name of a file, and every way to a
  subvolume, finds one node with one page cache. A node is read from its
  inode item before the table is locked; if two lookups read the same
  inode at once, the second's node is dropped, which only frees memory,
  so no lock is held while the disk is read.
- **IDs**: a file of the top subvolume has its inode number, as Linux's
  `st_ino`; one of another subvolume has its inode number with the
  subvolume's number from bit 40, so that IDs stay unique on the mount
  where Linux would use another `st_dev`. Inode numbers stay well below
  2^40, as Btrfs gives them out one by one.
- **Directories** find a name by its hash (`DIR_ITEM`, whose item holds
  every name of that hash) and list in the order the names were made
  (`DIR_INDEX`), as Linux's `readdir` does; a listing's cursor is the
  next index. Names that are not UTF-8 are left out of listings, as
  Radix's names are UTF-8. An entry that names a subvolume leads to its
  top directory; a subvolume being deleted (no references left) is not
  found.
- **Files are read through the page cache.** A page's backing finds the
  extent at or before it: a hole or a reserved extent reads as zeros, an
  inline one from its item, and one on the disk is read straight into the
  page, then checked against its checksum, from the first copy on. When
  no copy matches, or the sector has no checksum and the file is not
  `NODATASUM`, the read fails with `Io`, as on Linux; a `NODATASUM` file
  reads what the disk holds. Compressed extents fail with `NotSupported`
  until 10b.3 adds the decompressors.
- **Links** are inline extents, as long as their inode's size says:
  `mkfs.btrfs` keeps a NUL after the path, which Linux does not show.
- **Spark** tries FAT first, then Btrfs, read-only. `mkfs.btrfs` clears
  the first 64 KiB, where FAT's boot sector is, but `mkfs.fat` writes
  only its own sectors and may leave an older Btrfs superblock at 64 KiB:
  where both are found, FAT is the newer.
- **Tests** read what Linux's tools make. The host tests make
  filesystems with `mkfs.btrfs --rootdir` from a tree of every kind of
  file, under each checksum and node size and the DUP and mixed
  profiles, with subvolumes, a default one, `NODATASUM` files and
  compression, and compare them, byte for byte, through the VFS; they
  damage data sectors and check that the second copy, or `Io`, follows.
  The tests use options that Ubuntu 24.04's btrfs-progs 6.6 lacks
  (`--subvol`, `--compress`, `--inode-flags`), so `ob test` fetches
  Ubuntu 26.04's btrfs-progs package, pinned by its SHA-256, and unpacks
  it, without installing it, when the system's copy lacks them. The
  `btrfs` boot test mounts two disks Linux made, one of 4 KiB sectors,
  and lists every file through system calls; the host checks every line,
  and that the disks are as it made them.

## Consequences

- A Linux-made Btrfs disk of one device mounts in Dukovo and reads as on
  Linux, with its subvolumes, and damage to its data is caught, not read.
- IDs of files in subvolumes other than the top differ from Linux's
  `st_ino`; their inode numbers in their own subvolume are kept.
- Compressed files, which `compress=` mount options and `btrfs
  property set` make common, read once 10b.3 is in; writing comes with
  10c.
- `ob test` needs `dpkg-deb` or a recent btrfs-progs; Ubuntu, Mint,
  Fedora and Arch Linux have one or the other.
