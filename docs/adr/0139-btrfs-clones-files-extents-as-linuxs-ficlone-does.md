<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0139. Btrfs clones files' extents as Linux's `FICLONE` does

- Status: Accepted
- Date: 2026-10-28

## Context

Roadmap 10c.2 lists reflinks with snapshots. On Linux, `cp` of a file
within one Btrfs (coreutils 9 on, and uutils) shares the file's extents
with the copy through `FICLONE`, so a copy takes no room until one of
the two changes. ADR 0138 gave each data extent Linux's back
references, so an extent can now be held by more than one file.

## Decision

- **The requests.** A file's `control` takes Linux's `FICLONE`
  (`0x40049409`, the source's descriptor) and `FICLONERANGE`
  (`0x4020940d`, Linux's `struct file_clone_range`), and mlibc's
  `ioctl` passes them as Linux programs make them, the source's
  descriptor turned into its slot. The destination is opened for
  writing and the source for reading, as on Linux.
- **Linux's checks.** As Linux's `generic_remap_checks` and
  `btrfs_remap_file_range`: offsets in whole blocks; a length in whole
  blocks, but for a range that ends at the source's end and reaches the
  destination's end or past it; no range past the source's end; no
  overlapping ranges of one file; no clone between a file with
  checksums and one without (`EINVAL` for each). Another filesystem is
  `EXDEV`, a directory `EISDIR`, and a filesystem that cannot clone
  `EOPNOTSUPP`.
- **What a clone does.** The node writes both files' dirty pages back
  and holds both caches against writes, in address order as Linux's
  `lock_two_nondirectories` does. Past the destination's end the gap
  is zeroed first, as Linux's `btrfs_cont_expand` does. In one
  transaction, `clone_extents` drops what the destination held in the
  range, copies each of the source's extent items cut to the range
  (their offset and length changed, a hole left a hole), adds a data
  reference for each by tree, inode and the file offset the extent
  starts at, raises the size to the range's end if it was shorter, and
  counts the bytes the file now holds. The cloned range's pages leave
  the cache, to be read again.
- **Inline extents.** An inline extent clones only from offset 0 to
  offset 0 of a file with nothing after it, as Linux allows; otherwise
  the clone fails with `EINVAL`, and `cp` copies the bytes.
- **`cp`.** uutils' `cp` on Dukovo defaults to `--reflink=auto`, as on
  Linux: it tries `FICLONE` and copies when that fails;
  `--reflink=always` reports the failure and `--reflink=never` copies.

## Alternatives

- **`copy_file_range` that clones.** Linux's `copy_file_range` clones
  on Btrfs when it can. uutils' `cp` tries `FICLONE` first, which is the
  request every Linux tool knows; `copy_file_range` can share the same
  path later.
- **Dedupe (`FIDEDUPERANGE`).** Sharing ranges two files already hold
  the same, after comparing them, is for `duperemove` and comes when
  something here needs it.
- **Shared references (`SHARED_DATA_REF`) for clones.** Linux gives a
  clone a reference by tree, inode and offset; a reference by leaf only
  comes from a shared block's copy (ADR 0138).

## Consequences

- `clones` in radix-btrfs's host tests clones whole files, parts, past
  the end of a file cut within a block, within one file, parts of
  compressed extents and into another subvolume, against a model,
  changes and removes each side, and runs `btrfs check` after each.
- The `root` boot test clones a 1 MB file in `/users`, checks that the
  clone took no room and keeps its data as the source changes.
- A clone of a large file is one transaction over every extent of the
  range, in one call.
