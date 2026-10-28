<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0135. Hard links, named pipes, device nodes, socket names on disk, and a change time

- Status: Accepted
- Date: 2026-10-28

## Context

Before the root moves to Btrfs (Phase 10c.3), the files it holds have to
work as they do on Linux. Until now `link` failed with `EPERM`,
`mkfifo` with `ENOSYS` and `mknod` with `EPERM`. `stat` gave
`st_nlink` as 1 (2 for a directory), `st_ctim` as `st_mtim` (ADR 0076)
and `st_rdev` as 0. A socket could be named only on tmpfs (ADR 0091).
Package managers, `tar`, `cp -a`, `rsync -H`, `git` and test suites make
hard links. Shells, `make -j` job servers and daemons make named pipes.
Backup tools look at `st_ctim`, and `find -links` and `du` count names.

Btrfs keeps all of these. An inode item holds a link count, a change
time and a device number, and a file's names in a directory are in its
`INODE_REF` item for that directory, or, past what one item holds, in
`INODE_EXTREF` items. Linux keeps `rdev` as its own `dev_t`: the major
number above 20 bits of the minor. The kernel's `stat` method already
returned eight words, as many as a method's results can hold.

## Decision

- **The VFS** `Stat` gains `links`, `changed` and `device`. `Node` gains
  `link(name, node)`, which makes `name` another name of a file (not a
  directory) on the same filesystem, and `make_node(name, kind,
  device)`, which makes a named pipe, a device node, an empty file or a
  socket's name with no socket. Device numbers in the VFS and the ABI
  hold the major number in their high 32 bits.
- **The ABI** (directory and file interfaces, version 5):
  - `stat` writes a `FileStatus` record into a buffer, in place of its
    eight results. The record has room for the new fields and for later
    ones.
  - New methods `link` (with `follow`, as `AT_SYMLINK_FOLLOW`) and
    `make_node`.
  - A new file error `too_many_names` (`EMLINK`).

  The C library's `linkat`, `mkfifoat`, `mknodat` and `stat` use them.
  `st_ctim`, `st_nlink` and `st_rdev` come from the filesystem.
- **Btrfs**:
  - Hard links: a name in the file's `INODE_REF` item for the
    directory, or, once that item fills a tree block, in an
    `INODE_EXTREF`, as Linux puts them, with the link count and the
    change time, up to Linux's 65535 names a file. Names Linux kept in
    extended references can now be removed and renamed (ADR 0130 left
    them unchanged). A disk made without extended references refuses a
    name past a full item with `too_many_names`, as Linux does.
  - FIFO, character, block and socket inodes, the device number kept
    as Linux keeps it.
  - `stat` gives the inode's link count, the later of its change time
    and the page cache's modification time, and its device number.
  - A directory has one name, as Btrfs counts it.
- **tmpfs** counts a node's names and keeps a change time, set by a new
  name, a removed one, a rename and `set_times`. Contents changes are
  its modification time, which `stat` gives when it is later.
  Directories say 1 link, as on Btrfs: a count of subdirectories is not
  kept, and 1 tells `find` not to use it. Each name past a file's first
  counts against the tmpfs's limit of files, as on Linux, since each
  holds memory until it is removed.
- **Opening** a filesystem's special node opens what it stands for, on
  a read-only mount too, as on Linux:
  - A **named pipe** opens an end of the one pipe its opens share while
    any is open. The kernel finds the pipe by the node, which the pipe
    holds.
  - An open only to read waits for a writer, and one only to write
    waits for a reader. With `O_NONBLOCK`, a read open succeeds at once
    and a write open with no reader fails with `ENXIO`. An open to read
    and write waits for nothing, as on Linux.
  - A **device node** opens the device in `/devices` that has Linux's
    number for it: `null` 1:3, `zero` 1:5, `random` 1:8, `urandom` 1:9,
    `kmsg` 1:11, `tty` 5:0, `ptmx` 5:2 and `fb0` 29:0. Any other number
    fails with `ENXIO`.
  - A **socket's name** fails with `ENXIO`.
- **Socket names on Btrfs** are socket inodes on the disk. What the
  kernel binds to one is kept in memory while the filesystem is
  mounted, as on Linux. After a remount, connecting through the name
  fails with `ECONNREFUSED`, and binding it again needs it removed.
- FAT and exFAT have none of these (`EPERM`, as Linux's vfat).

Rejected:

- **More results words for `stat`**: a method's results are eight
  words, and a ninth would need a second call on every `stat`.
- **Device nodes standing for any device the kernel has**: block
  devices and the input, sound and GPU nodes have no fixed numbers in
  Dukovo. `/devices` is where programs open them. Numbers for them can
  come when a program needs a node elsewhere.
- **Counting a directory's subdirectories in tmpfs**, as Linux's tmpfs
  does: more bookkeeping on every directory change, for a count that
  only `find`'s leaf optimisation reads, and Btrfs does not keep it.

## Consequences

- `ln`, `mkfifo`, `mknod`, `tar` with hard links, `cp -a`, `rsync -H`
  and shells' named pipes work on Btrfs and tmpfs, and Linux's `btrfs
  check` passes on disks Radix gives them.
- `stat` copies an 80-byte record to the caller where it returned
  registers. Linux copies 144 bytes for its `struct stat`.
- The devices in `/devices` still give 0 as their device number, so a
  copy of `/devices/null` with `cp -a` makes a node that opens nothing.
  Giving them their numbers is a later change of devfs alone.
- Changing the ABI rebuilt the C library and every C port.
- A file's names in one directory can grow its reference item to a whole
  tree block. That showed that the tree writer, inserting a key already in
  a full leaf, split the leaf first and could then insert the key a second
  time; it now refuses such an insert before any split.
