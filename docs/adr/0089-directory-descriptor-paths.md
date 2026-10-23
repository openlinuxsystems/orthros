<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0089. The C library notes the path of each directory descriptor, for `..` beneath it and fchdir

- Status: Accepted
- Date: 2026-10-23

## Context

The kernel resolves every path beneath a directory capability and never
above it (ADR 0054), so `openat(fd, "..")` fails with EXDEV when `fd` is
the directory it climbs out of. A capability is a place in the tree, not
a path, so `fchdir` has had no path to make the working directory, and
failed with ENOSYS (docs/design/posix.md).

Programs that walk a tree by descriptors need both. libarchive's tree
walk, which `tar -c` runs, opens each directory beneath the last one,
climbs back with `openat(fd, "..")` when it is done with it, and
returns to where it started with `fchdir`; the first tar archive of a
directory with a subdirectory in it failed. gnulib's fts, in GNU grep
and others, climbs the same way.

The C library holds the root directory (`SLOT_ROOT`), from which it
already resolves every absolute path and every path relative to the
working directory, which it keeps as a string. A path from the root
reaches whatever a `..` from a directory beneath it would, so knowing a
directory descriptor's path is enough.

## Decision

- **The C library notes the path by which each directory descriptor was
  opened**, beneath the root: the path it resolved, or, for a path
  beneath another directory descriptor, that descriptor's path with this
  one joined on. A path with a `..` in it is replaced by the kernel's
  canonical one (`real_path`), so the path of a directory that a walk
  climbs back to does not grow with each `..`.
- `dup`, `dup2` and `F_DUPFD` copy the path to the new descriptor;
  `close` forgets it, and a descriptor made any other way has none.
- **A path beneath a directory descriptor that has a `..` among its
  names goes to the kernel from the root**, as the descriptor's path and
  then the path. Other paths beneath a descriptor still go to the kernel
  with its capability, which works even if the directory has been moved.
- **`fchdir` changes to the descriptor's path** as `chdir` would, so the
  working directory stays canonical. A directory descriptor with no path
  still fails with ENOSYS; any other file fails with ENOTDIR.
- The paths are kept in a pool of 64 entries of a page each, which only
  the entries ever used take memory for, given out and taken back with
  atomic operations only: `close` and `dup2` are async-signal-safe, and
  a child calls them after fork while a lock another thread of its parent
  held may stay held. When the pool is full a new directory descriptor
  gets no path.
- The kernel is unchanged: a program that holds only a directory
  capability, and not the root, still cannot climb out of it.

## Consequences

- `openat(fd, "..")` and `fchdir` work as on Linux for directory
  descriptors a program opened itself, which is how tree walks use them.
  Opening a directory costs no more, but for a path with a `..`, which
  costs one more call. `posix_spawn_file_actions_addfchdir` works too,
  so mlibc's own test of it runs again on Radix.
- A descriptor's path is the one it was opened by. If the directory is
  renamed or moved while it is open, a `..` beneath it and `fchdir` go
  by the old path, which may then name another directory or none; on
  Linux they follow the directory.
- Directory descriptors that came from before exec, or that were opened
  while 64 others had paths, have none: `..` above them still fails
  with EXDEV and `fchdir` with ENOSYS.
- A program given a narrower root than the system's, as later phases
  give services, climbs only within that root, as its absolute paths
  stay within it.
