<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0054. Files are reached through directory capabilities, with every path resolved beneath one

- Status: Accepted
- Date: 2026-10-22

## Context

Phase 7 gives Radix a virtual file system: one tree of filesystems
mounted on directories of each other. Every capability system has to
decide how programs name files in it. A global namespace that any
program can walk from `/`, with permission bits deciding what it may
open, is what Unix does, and it makes every program's reach as wide as
its user's. Capability systems instead hand each program the
directories it needs: WASI calls them preopened directories, FreeBSD's
Capsicum `openat` beneath a directory descriptor with `O_RESOLVE_BENEATH`,
and Fuchsia's component framework builds a namespace per component.

## Decision

- A file or directory is a kernel object, named by a capability like any
  other (radix-directory(2), radix-file(2)). The object is a place in the
  tree: a mount and a node of it.
- Every path is resolved beneath the directory it is given with. A path
  is names separated by `/`; `.` is the directory itself, `..` its
  parent but never above the starting directory, and a path that starts
  with `/` is refused with `outside`. So what a program can reach is
  exactly what its directory capabilities lead to.
- A directory capability's rights say what it allows: read (look up,
  list, open for reading), write (create, remove, rename, open for
  writing) and control (mount and unmount). `open` hands on at most the
  directory's own rights, so a capability cannot be widened by opening
  through it.
- The kernel keeps one mount table for the system's tree. A mount needs
  the control right; mounting again on the same directory stacks on top,
  as on Linux; a filesystem is unmounted only when nothing is mounted on
  it and nothing holds a place on it (`in_use`); a rename never crosses
  mounts. A removed file lives on until its last capability goes.
- Spark gets the root of the tree in `SLOT_ROOT` (radix-startup(2)) and
  gives each program the directories it needs: in Phase 7, the root, to
  the one program it starts.
- The VFS (`radix-vfs`), the block layer and FAT are compartments, with
  no `unsafe` code; the kernel only wires them in.

## Consequences

- A program's reach can be cut to one directory, and later phases can
  give each service just its own (spec 7.1's `/data/<service>`), with no
  separate sandboxing layer.
- POSIX programs, which expect `open("/etc/hosts")`, need their C
  library to resolve absolute paths against a root directory capability
  it holds. mlibc's Radix sysdeps do that in Phase 8, as WASI's libc
  does with its preopens.
- The mount table is one for the whole system; per-program views, if
  ever needed, come from handing out different directories rather than
  separate mount tables.
