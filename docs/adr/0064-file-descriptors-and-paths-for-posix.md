<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0064. A file descriptor is a capability slot, and the C library turns paths into paths beneath the root it holds

- Status: Accepted
- Date: 2026-10-22

## Context

Radix reaches files through directory capabilities, with every path
resolved beneath one and no absolute paths in the kernel (ADR 0054).
POSIX programs name files by absolute and relative paths, keep a working
directory, and hold small integers for open files that `dup`, `fork`
and `exec` pass along, with an offset and status flags shared between
the copies. The two have to meet without the kernel learning a global
namespace, and without a table in the C library that `fork` and `exec`
would have to copy.

## Decision

- **File descriptor _n_ is capability slot `SLOT_FD_BASE` + _n_**
  (radix-startup(2)). A file capability is an **open file
  description**: a kernel object with the offset and the status flags
  (`append`, `non_blocking`), so `dup`, `dup2` and `fcntl(F_DUPFD)` copy
  the capability into another slot and the copies share both, as POSIX
  says, and `fork` and `exec` carry descriptors without the C library
  doing anything (ADR 0063). FD_CLOEXEC is the slot's `close_on_exec`
  flag. The capability space grows by doubling as descriptors are
  opened (`grow`), up to the soft RLIMIT_NOFILE.
- A program starts with the root of its tree in `SLOT_ROOT`, and its
  working directory as a string in the auxiliary vector
  (`AT_RADIX_CWD`, radix-process(2)'s AUX_CWD). The C library keeps the
  working directory as a string, resolves `.` and `..` in a path against
  it **lexically**, and hands the kernel a path beneath `SLOT_ROOT`. A
  path relative to a directory descriptor (`openat` and the rest) goes
  to the kernel with that directory's capability, beneath which `..`
  cannot climb (`EXDEV`, the `outside` error).
- `chdir` checks the directory and keeps the new string; `exec` passes
  it on; `fchdir` fails with ENOSYS, as a description does not know its
  path.
- Pipes and terminals are file descriptions too, of nodes on file
  systems mounted nowhere, so every descriptor operation works on them.

## Consequences

- No descriptor table to keep in step: the capability space is the
  table, and the kernel's checks on capabilities are POSIX's EBADF.
- A program can reach only what its root leads to, so a sandboxed
  program's absolute paths stay inside its sandbox, as WASI's preopened
  directories do.
- Lexical `..` differs from POSIX only through symbolic links. When they
  came in Phase 9, resolution moved into the kernel (ADR 0081).
- `st_dev` is one number for every file system until the kernel says
  which one a file is on (docs/design/posix.md).
