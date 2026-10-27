<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0130. Btrfs files change under one transaction lock, and their data is copied on write at write-back

- Status: Accepted
- Date: 2026-10-27

## Context

ADR 0129 writes Btrfs's trees by copying on write and commits them in an
order that survives a power cut. Phase 10c.1b changes files through
those trees: it makes, writes, truncates, removes, renames and links
them, so that a writable mount works as Linux's does and Linux's
`btrfs check` passes on every disk it leaves.

- A file is several kinds of item in its subvolume's tree, all under its
  inode number: the inode item, an `INODE_REF` per name, a `DIR_ITEM` (by
  name hash) and a `DIR_INDEX` (in the order made) per entry of a
  directory, and an `EXTENT_DATA` per extent of a file. An extent on the
  disk also has an `EXTENT_ITEM` in the extent tree, which counts the
  files' references to it, and its sectors' checksums in the checksum
  tree. One change to a file touches several of these, in several trees.
- The writer of ADR 0129 edits the running transaction's blocks in place.
  A reader walking a tree while a change edits it can see a block half
  changed.
- The page cache (ADR 0127) holds a file's data; the writeback thread
  writes its dirty pages every five seconds, and a program's `write`
  returns once its bytes are in the cache.
- Linux's `btrfs check` counts an inode's bytes on the disk (`nbytes`),
  its links, each directory's size (twice the length of its names), each
  extent's references, and the checksum of every sector of a file that
  has them; a filesystem that disagrees with it is corrupt.
- Since `btrfs-progs` 5.15, `mkfs.btrfs` makes filesystems with
  `no-holes`: a hole in a file has no item. Without it, every hole has an
  `EXTENT_DATA` of its own.

## Decision

- **One lock per writable mount** guards the running transaction. Every
  change holds it from its first item to its last, so the trees never
  show half a change, and so does every read of the trees on a writable
  mount. It is never held while a program's memory is copied, which may
  fault and read a file: a directory is listed in batches of 64 entries
  with the lock released between them, and a link's target is copied out
  of the tree before it is copied to the program. The lock order is a
  file's page cache, the transaction, a node's inode, the node table and
  the list of nodes to remove, the workspace of compressed reads, the
  block cache, and the disk.
- **Data is copied on write at write-back**, not at `write`: `write`
  changes the page cache and the inode's size in memory. At write-back the
  page cache hands each run of up to 32 contiguous dirty pages to the
  filesystem at once (`Backing::write_pages`), which takes a new extent,
  writes the pages to each of its copies, computes the checksums from the
  pages it wrote, and only then, under the lock, drops the old extents'
  references in that range, inserts the new `EXTENT_DATA` and
  `EXTENT_ITEM` and the checksums, and updates the inode. A tree never
  points at data that is not written, and a power cut keeps the old data.
  A run that finds no space is split in halves, down to one page.
- **References are counted as Linux counts them**: by root, inode and
  the file offset the extent starts at. Part of an extent kept when a
  write cuts through it is another reference of the same triple. An
  extent whose last reference goes is freed with its checksums; if the
  running transaction made it, at once, else pinned until the commit is
  durable (ADR 0129). Files with `NODATASUM` get no checksums, and new
  files take `NODATACOW` and `NODATASUM` from their directory, as on
  Linux; `NODATACOW` files are still copied on write until 10c.2.
- **Inline extents are read and converted, not written**: a write
  anywhere but over the whole of an inline extent, a hole punched in one,
  or a truncate to a size above zero first writes its bytes as a regular
  extent. New files are written in regular extents only, for now.
- **Names**: a new entry takes the directory's last index plus one (2 in
  an empty one, as on Linux), and a `DIR_ITEM` whose name hash another
  name already has is added to that item. Removing a name drops its three
  items and one link (a directory, which must be empty, has one); an inode
  whose last link goes is deleted at once with its extents.
- **Files removed while open** keep their items until their last node
  goes, with an orphan item in the subvolume. The sync after the last
  node goes deletes the file, its extents and the orphan item; a writable
  mount deletes every file an orphan item names, for a power cut that
  came first.
- **Commits** come when the oldest change is 30 seconds old, when more
  than 16384 pages (64 MiB) wait to be written, on `sync` and `syncfs`,
  and on `fsync`, which writes the file's pages and commits until the log
  tree (10c.2). The writeback thread looks every five seconds, and when a
  commit is due writes the dirty pages and commits.
- **What mounts writable**: only a filesystem with `no-holes`, one
  device, and no log tree to replay; and none that uses RAID5 or RAID6,
  zones, extent tree v2, the RAID stripe tree, or simple quotas. Any other
  is refused a writable mount (`NotSupported`) and still mounts read-only,
  as one with an unknown read-only-compatible feature does on Linux.
- **Not yet**: pipes, devices and sockets (`NotSupported` until 10c.3),
  renames from one subvolume to another (`CrossMount`, `EXDEV` as on
  Linux), removing a subvolume's entry (10c.2), changing a name kept in an
  extended reference (thousands of hard links in one directory, from
  Linux), space reserved before a change (10c.1c), new chunks (10c.1c),
  and writing small files inline.
- **Tests**: host tests make, change and remove files through the mount,
  then run `btrfs check --check-data-csum` on the disk and compare every
  file with a read-only mount of it; they change files Linux made (inline,
  compressed with zlib or zstd, hard links); they cut power at every
  point between flushes; and they check 600 random changes against a
  model, with 16 KiB nodes and CRC-32C, and with 4 KiB nodes and SHA-256.

Rejected:

- **Copying data at `write`**: every small `write` would copy a whole
  extent and its tree path, and a file written in 4 KiB pieces would end
  up in 4 KiB extents, where Linux's are up to 128 MiB.
- **A lock per tree, or per inode**: one change touches the subvolume,
  extent, checksum and free-space trees, and the extent tree's blocks are
  shared by all files. Readers on a writable mount wait for writers; a
  read-only mount takes no lock, as before.
- **Writing a hole item per hole**: `no-holes` is the default of every
  `mkfs.btrfs` since 5.15, and an old filesystem still mounts read-only.

## Consequences

- A program's `write` costs a page-cache copy, as on Linux; the copy on
  write and the checksums happen at write-back, in runs of up to 128 KiB.
- Reads of a writable mount wait while a change holds the lock. The
  lock is held for tree work only, never for disk reads of file data or
  copies to programs, so the wait is short; finer locking can come with
  the bench rows that show the need.
- Small files take a whole sector where Linux puts them in the tree,
  until inline writes come.
- `btrfs check` passes on every disk the tests leave, and each test's
  disk reads back the same through a read-only mount.
