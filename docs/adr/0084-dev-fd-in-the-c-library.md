<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0084. The C library gives /dev/fd, /dev/stdin, /dev/stdout and /dev/stderr

- Status: Accepted
- Date: 2026-10-23

## Context

Process substitution (`diff <(sort a) <(sort b)`, `done < <(command)`)
gives a command a pipe by a name, `/dev/fd/N`, which the command opens
as a file (ADR 0083). Scripts also write to `/dev/stderr` and read
`/dev/stdin` by name. Linux makes `/dev/fd` a link to `/proc/self/fd`,
whose entries are links that open the file again. The BSDs have
fdescfs, where opening `/dev/fd/N` copies descriptor N, as `dup` does.

Radix has no filesystem that is different for each process. Its devfs
is the same for everyone, and its `/proc` has its own text files
(radix-proc(5)). A descriptor is a capability slot that the C library
maps (ADR 0068). Every program, C or Rust, reaches files through the C
library, as Radix has no other system call interface for them.

## Decision

The C library's Radix layer gives the names itself. `open`, `openat`,
`stat`, `lstat`, `fstatat`, `access` and `faccessat` take
`/dev/fd/N` as descriptor N, and `/dev/stdin`, `/dev/stdout` and
`/dev/stderr` as 0, 1 and 2. The name is matched after the path is
resolved against the working directory, so `cd /dev; cat fd/3` works
too.

Opening a name works as on the BSDs:

- it copies the descriptor, as `dup` does, so the copy shares the open
  file description and its offset;
- it never gives more access than the descriptor has: opening a
  read-only descriptor for writing fails with `EACCES`;
- `O_CLOEXEC` is honored, and `O_TRUNC` does nothing;
- `O_CREAT | O_EXCL` fails with `EEXIST`, `O_DIRECTORY` on anything
  but a directory fails with `ENOTDIR`, and a directory opened for
  writing fails with `EISDIR`.

`stat` of a name is `fstat` of its descriptor. A descriptor that is not
open has no name, so it gives `ENOENT`. Only the number as it is
written counts, so `/dev/fd/01` gives `ENOENT`.

## Consequences

- Process substitution and the scripts that use these names work in
  every program, with no kernel change.
- On Linux, `echo x > /dev/stdout` truncates a file that standard
  output was redirected to, and starts writing at its beginning. On
  Dukovo it writes on from the shared offset, as on the BSDs. This is
  the safer behavior.
- Listing `/dev/fd` (`ls /dev/fd`) is not given, and neither is
  `readlink`. A path to these names that goes through another directory
  descriptor (`openat(dev, "fd/3")`), a `..` or a symbolic link is not
  matched, and gives `ENOENT`.
- If Radix ever has a filesystem that is different for each process,
  these names move there and this code is removed.
