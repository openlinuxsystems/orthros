<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0226. Programs serve filesystems through /devices/fuse, in Linux's FUSE protocol

- Status: Accepted
- Date: 2026-11-02

## Context

Phase 13f.1. Dukovo mounts shares from Windows networks (SMB), and later
NFS exports. The protocols are big, change often and are spoken over the
network to machines nobody here controls: their clients belong in a
program, with the network and the server's words kept out of the kernel,
not in the kernel as Linux's `cifs` is. The kernel needs a way for a
program to serve a filesystem that every other program then uses as any
other.

Chase chose, on 2026-11-02, a bridge in the kernel that speaks Linux's
FUSE protocol, over a protocol of our own: programs written for Linux's
FUSE, and libraries such as fuser and libfuse, then serve filesystems on
Dukovo with few changes, and the protocol's handling of signals, lost
programs and unmounts is one that a decade of programs already expect.

## Decision

- **The device.** `/devices/fuse` (Linux's 10:229, mode 0666, as on
  Linux) makes a connection each time it is opened. The program reads
  requests from the description and writes answers to it, in Linux's
  messages: a 40-byte header, the operation's structure and its names or
  data; an answer is a 16-byte header with the request's number and an
  errno, and the operation's structure. A read needs a buffer of 8 KiB
  at least, and of the largest request to be sure of one; a poll finds
  the description readable while a request waits.
- **The protocol's version.** The kernel offers 7.31 and takes a
  program's answer to `INIT` from 7.12 on, which the requests' structures
  have kept their size since. A write or read carries at most what INIT
  allows: the program's `max_write`, and 32 pages unless it takes
  `MAX_PAGES` and says more, within 4 KiB and 1 MiB.
- **Mounting.** radix-directory(2) gains `mount_fuse` (method 40): a
  path, the connection's slot, which must be open for reading and
  writing, a source named in the mounts list, and the mount flags but
  compression. It needs the directory's control right, which
  administrators' sessions hold, as mounting needs elsewhere. The
  filesystem kind is `fuse` (13). `INIT` is queued at the mount, and
  calls wait for its answer. A mount through a view is refused, as every
  mount is.
- **Nodes.** The kernel keeps one node for each node number the program
  names, and sends `FORGET` with the count of its names when the kernel
  lets go of it. A node's attributes are kept for as long as the answer
  that gave them allows. Files are opened for reading and for writing as
  needed, and released when the node goes; `CREATE` makes a file and its
  handle at once, and a program that answers it with ENOSYS gets
  `MKNOD` and `OPEN` after. A name made, removed or moved has its
  directories' attributes asked for again, their links and times having
  changed, as Linux's FUSE does. There is no page cache yet: reads and
  writes go to the program as they are made.
- **What a program cannot do.** A mount is never a way to the kernel's
  devices: a device node on it opens nothing (EACCES), as Linux's
  `nodev`. It is never a way to run as another user: set-ID bits are
  never shown, as `nosuid`. The owners and modes the program gives are
  what the kernel checks, as `default_permissions`, so a program cannot
  be asked about access it has not stated. These hold for every mount,
  without options to turn them off.
- **Signals and lost programs**, as Linux's FUSE: a signal that comes
  while a call waits sends `INTERRUPT` for its request, at once if the
  program has read it and as soon as it does if not, before the requests
  queued, and the call waits on for the answer. The program may answer
  EINTR, and the call fails with EINTR, not run again whatever SA_RESTART
  says; a call waiting for `INIT` waits through signals. Requests'
  numbers step by two, and an `INTERRUPT`'s is its call's with the
  lowest bit set, so that its own answer is told apart: ENOSYS says the
  program cannot stop requests, and none is sent again (fuser answers
  so), and EAGAIN that it does not have the request yet, and it is sent
  again. A thread that is
  being killed never waits: its request is taken back if the program has
  not read it, and abandoned if it has; an answer that names a node the
  kernel never took gets a `FORGET` at once, and a `RELEASE` for a handle
  `CREATE` made. When the program's last description goes, every call
  fails with ENOTCONN until the mount is unmounted. When the mount goes,
  the program reads the requests left (the `FORGET`s) and then fails
  with ENODEV, which tells it to end: a new error of files,
  `unmounted` (26), maps to it.
- **Extended attributes** go to the program; one that answers ENOSYS is
  not asked again.
- **The crate.** `radix-fuse` (kernel/fs/fuse) holds the protocol, the
  connection and the filesystem, with no `unsafe` code, and builds for
  the host, where its tests serve a filesystem from a thread: written
  data split as INIT says, an old program refused, a lost program,
  `INTERRUPT` for a signalled call, sent again or no more as the program
  answers it, and a node forgotten after a killed call.
- **fuser** 0.16.0 (MIT), from crates.io, is patched for Radix in the
  rust-crates port (ADR 0080): on nix 0.30, which the port has, Radix's
  errnos as Linux's, each answer gathered into one write, and no mount
  of its own, since Radix's is `mount_fuse`: a program serves the
  descriptor it was handed with `Session::from_fd`. The device takes an
  answer in one write, as Linux's does, and Radix's `writev` writes its
  buffers one at a time once they pass PIPE_BUF, so fuser's `writev` of
  a header and a read's data had to become one buffer. Its dependencies
  are permissive: page_size, smallvec and log (MIT OR Apache-2.0),
  zerocopy (BSD-2-Clause OR Apache-2.0 OR MIT) and memchr (Unlicense OR
  MIT).
- **fuse-test** (userspace/programs/fuse-test) serves a filesystem in
  memory with fuser and uses it, in the `fuse` boot test: files,
  directories, links, a large file, a root stamped for another user
  (ADR 0214) refused a private file and the owner of what it makes, no
  set-ID bits and no devices, an unmount that ends the program, and a
  mount whose program has gone.

## Consequences

- A share's client (13f.1b, SMB) is a program: its parsing of what a
  server sends is confined to it, and a fault in it ends its mount, not
  the system.
- Programs and libraries for Linux's FUSE need only their mount code
  changed, as fuser's was.
- Every call to a mount waits on a program: a program that never answers
  holds its callers until they are killed or it is, as on Linux.
- **Not yet:** a page cache for these files, with writeback and `mmap`
  of shared writable mappings; `READDIRPLUS`; the program's notices to
  the kernel (invalidations, poll); file locks, `ioctl` and `FLUSH` on
  each close; `fallocate` and `copy_file_range`; mounts by users who are
  not administrators, with `allow_other`; remounting; and a `writev`
  that is one write at any size, which C programs on libfuse need.
