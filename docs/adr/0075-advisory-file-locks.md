<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0075. Advisory file locks are kept by the kernel in one table: BSD's, POSIX's and Linux's open file description locks

- Status: Accepted
- Date: 2026-10-22

## Context

Programs coordinate through file locks: package managers, databases
such as SQLite, mail tools, editors' swap files, Cargo's build
directory, and Rust's `File::lock`. Three kinds are in use. BSD's
`flock` locks a whole file for an open file description. POSIX's record
locks (`fcntl`'s `F_SETLK`) lock ranges for a process, and have the
well-known trap that closing any descriptor of the file lets go of all
the process's locks on it. Linux's open file description locks
(`F_OFD_SETLK`) lock ranges for a description, without that trap. A
lock must be seen by every process that opens the file, by any path, and
must go when its owner does, however the owner ends, so the kernel has
to keep them.

## Decision

- radix-file(2)'s **`lock`** takes, changes or lets go of a lock, and
  **`lock_holder`** finds the one in the way (`F_GETLK`). A lock's owner
  is an open file description (`flock`'s whole-file locks and Linux's
  range locks) or a process (POSIX's). Whole-file locks meet only each
  other; range locks of both owners meet each other, as on Linux. Locks
  are advisory: they never stop a read or a write.
- What a file's list of locks becomes is **`radix_vfs::locks`**, pure
  code that the host tests, against a model of every byte's owner and
  kind as well: one owner's locks never overlap, a new lock replaces the
  owner's old ones on its range and joins those of its kind that it
  touches, as POSIX says.
- The kernel keeps the locks of every file that has any in **one
  table**, by the address of the file's node, pinned by a weak reference
  while it has locks. The table's lock is a **leaf**, taken after every
  other, so that locks can be let go of wherever an owner goes: when a
  description's last reference goes, when a process deletes any
  capability of the file (POSIX's rule, which `close` and `exec`'s
  closing of close-on-exec descriptors both are), and when a process
  ends. A fork's child has none of its parent's process locks.
- A lock in the way fails with `EAGAIN`, or is waited for until a lock
  of that file goes, or a signal comes. Before a process lock waits, the
  chain of processes waiting for each other's locks is followed, ten
  links at most, as Linux does: if it leads back to the caller,
  `EDEADLK`.

## Consequences

- `flock`, `fcntl`'s locks, `lockf` and Rust's `File::lock` work as on
  Linux; the `locks` C test and `std-test` check them, between
  processes, across `fork`, with waits a signal interrupts and with
  deadlocks.
- One table is a single point of contention when many files are locked
  at once. It holds only files with locks, and its work is a short scan;
  a table per node can replace it when a profile shows the need.
- Mandatory locks and leases (`F_SETLEASE`) are not supported, as on
  Linux by default; `fcntl` fails them with `EINVAL`.
