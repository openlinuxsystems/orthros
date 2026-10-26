<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0081. Symbolic links are resolved by the kernel, beneath the directory a path starts from

- Status: Accepted; the targets of `/etc` and `/var` are changed by [0113](0113-plain-names-for-the-root-folders.md)
- Date: 2026-10-23

## Context

Until Phase 9 no filesystem had symbolic links. The C library resolved
`..` in a path itself, by dropping the name before it from the string
(ADR 0064). That is only correct in a tree without links.

Phase 9 needs links:

- uutils' core utilities are one program, and Unix runs a utility like
  that through a link by the utility's name (`ls -> coreutils`);
- scripts start with `#!/bin/sh` and `#!/usr/bin/env`;
- programs look for `/etc/hosts`, `/usr/share/man` and `/var/log`,
  which Dukovo keeps under `/system` (the plan's spec 7.1);
- `ln -s`, `readlink`, `realpath`, `find -L` and `tar` have to work.

On Unix, a link's absolute target is resolved from the process's root.
In Radix a path is resolved beneath a directory capability (ADR 0054),
and a program may hold several of them. That decision says the walk
never goes above the directory it starts from. Links have to keep that
true.

## Decision

- **Links are nodes of their filesystem.**
  - tmpfs keeps a link's target with the node, and counts it in the
    filesystem's usage.
  - The boot archive keeps ustar's symbolic links (type `2`). `ob` writes
    a link in the sysroot as a link in the archive, so a port can
    install one with `ln -s`.
  - FAT has no links, and Btrfs (Phase 10) will have its own.
- **The kernel follows links while it walks a path**, beneath the
  directory the path starts from:
  - A relative target goes on from the link's directory.
  - An absolute target goes on from the starting directory, as if it
    were the root. This is Linux's `RESOLVE_IN_ROOT`. For a program
    whose root capability is the tree's root, it is the same as Unix.
    For a sandboxed program, it keeps the link inside the sandbox.
  - `..` steps back along the names actually walked. It is never
    worked out from the text of the path.
  - At most 40 links are followed (Linux's limit); more fails with
    `too_many_links` (ELOOP).
  - The last name of a path is followed too, unless the call asks not
    to with `no_follow`. A trailing `/` always follows it.
  - Removing or renaming a link works on the link, never on its target.
  - Creating a file through a link creates the target.
- **The directory interface gets three calls and a flag**
  (radix-directory(2), version 3):
  - `make_link` makes a link;
  - `read_link` reads a link's target;
  - `real_path` gives the canonical path, every link followed, as names
    from the starting directory;
  - `no_follow` on `open`, `stat` and `set_times` makes them act on a
    link that is the last name.
- **The C library leaves resolution to the kernel.**
  - mlibc passes the kernel absolute paths beneath the root, and relative
    ones joined to the working directory, with no lexical `..`.
  - `realpath`, and `chdir` (which keeps the working directory
    canonical), ask the kernel through a new `Realpath` sysdep
    (mlibc patch 0024).
  - `symlink`, `readlink`, `lstat`, `O_NOFOLLOW` and `AT_SYMLINK_NOFOLLOW`
    map onto the calls above.
- **Spark makes five compatibility links at boot**, in the root's tmpfs,
  after the mounts:
  - `/bin -> system/bin`;
  - `/sbin -> system/bin`;
  - `/usr -> system`;
  - `/etc -> system/etc`;
  - `/var -> system/var`.

  `/system` stays the only place Dukovo keeps its own files.

## Consequences

- `/bin/sh`, `/usr/bin/env` and `/usr/share/man` work for scripts and
  for ported programs, without a second copy of anything.
- `..` through a link means what POSIX says it means, so the limit ADR
  0064 recorded for lexical `..` is gone.
- A link's absolute target means "beneath the directory the path started
  from". A program that resolves links itself against `/` sees the same
  thing whenever its root is the tree's root.
- Hard links still fail with EPERM until Btrfs.
- `fchdir` still fails with ENOSYS, since a description does not know
  its path.
