<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0133. Btrfs uncompresses a run's extents on two CPUs, through the kernel's `join`

- Status: Accepted
- Date: 2026-10-27

## Context

With ADR 0132, a sequential read of a zstd file of text, 1 MiB in eight
extents of 128 KiB, took 2522 µs on Radix and 1303 µs on Linux on the
CI runner (`posix_btrfs_read_zstd_mib`): about 1980 µs uncompressing on
the reader's CPU, and 540 µs waiting for the disk, a request for each
extent. Linux sends a read-ahead window's extents to the disk together,
and its read completions uncompress them in Btrfs's `endio` work queue,
which is unbound: its workers run on whichever CPUs are free, so the
benchmark VM's two CPUs uncompress at once while the reader waits.

The Btrfs compartment has no `unsafe` and no threads of its own: it
cannot hand work to another CPU by itself.

## Decision

- **`radix_vfs::join(a, b)`**: runs `a` on the caller's CPU and `b` on
  another when one is free, and returns when both are done. It may
  sleep, so the caller holds no spin lock; `a` may borrow what is not
  `Send` (the page cache's `more`), `b` only what is. Without a kernel to
  give it one (host tests of a compartment), it runs `a`, then `b`; host
  tests give it one of `std::thread::scope`.
- **Helpers** (`kernel/radix/src/join.rs`): every CPU has a thread,
  `help/N`, pinned to it in the fair class at nice 0, as Linux's
  kworkers are. A caller claims the idle helper of another CPU (a
  compare-and-swap from idle), leaves it the address of `b`, which lives
  on the caller's stack, marks it posted and wakes it; with no helper
  idle, it runs both itself. After `a`, it takes `b` back if the helper
  has not started it (posted to idle, a compare-and-swap) and runs it;
  otherwise it waits until the helper is done. So `b` outlives every use
  the helper makes of it, a busy CPU costs the caller only the hand-off,
  and a join never waits for a helper that has not started. Handing
  over the address is the kernel crate's one more `unsafe` site.
  `kernel/radix/tla/Join.tla` checks that each half runs exactly once,
  only while its caller is in the join, that a helper serves one caller
  at a time, and that every join returns. The kernel gives the
  compartments `join` only with two CPUs or more.
- **Runs of compressed extents**: a page cache run (ADR 0132) that starts
  in a compressed extent takes the compressed extents that follow it in
  the file as far as the run goes, each whole, up to eight (1 MiB). The
  first half of them, by count, are read and uncompressed on the
  reader's CPU, with the filesystem's workspace, and the second half on
  another, with a second workspace, made at the first such run when
  memory allows (without it, the reader does all). Extents that follow
  each other on the disk too are read in one request, as many as a
  workspace's 128 KiB holds. An extent that cannot be read or
  uncompressed ends the run before it, and fails it only when it holds
  the page asked for, as before.

Rejected:

- **Reading ahead in the background**, as Linux's asynchronous read-ahead
  does: a run would be read and uncompressed while the reader copies the
  last, which needs pages that are in the cache but not yet read, and a
  way to wait for them. The two halves of a run are most of the time,
  with none of that.
- **Threads in the compartment**: the compartment would need `unsafe`, or
  the scheduler's interface, to hand a borrowed half to another thread.
  `join` keeps both in the kernel, behind one function a compartment can
  call.
- **A queue of jobs for any helper**: work-stealing pools spread work
  wider, but a run splits in two, and one helper on another CPU is
  enough for the two CPUs of the benchmark's VM and most laptops' cores.

## Consequences

- Uncompressing a run of eight extents takes about half as long on two
  idle CPUs; on one CPU, or with the other busy, it is as before but for
  the hand-off.
- A second workspace, about 400 KiB, is kept by a filesystem that reads
  compressed runs on a machine of two CPUs or more, and dropped by its
  shrinker with the first.
- The helpers' time counts to the root domain, not the reader's.
