<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0180. The TCP/IP stack runs in the kernel, on smoltcp

- Status: Accepted; smoltcp is patched, and a closed socket's memory comes back, since [0182](0182-the-kernel-patches-smoltcp-so-socket-memory-comes-back.md)
- Date: 2026-10-30

## Context

Phase 12 brings networking. The plan builds the stack from smoltcp and
extends it for production use (Phase 12a). Two things were open: where
the stack runs, and how smoltcp fits the kernel.

**Where it runs.** A network stack is the code any machine on the
network can send data to.

- In a separate program, as Fuchsia, QNX and Redox run theirs, a bug in
  it takes over that program only, which can restart without a reboot.
  Every packet then crosses into that program and back: one more hop,
  which a stack that matches Linux usually pays for with CPUs that do
  nothing but poll for packets. Servers can spare them; laptops cannot.
- In the kernel, as Linux, the BSDs and Windows run theirs, a bug in it
  is a bug in the kernel. The disks' drivers and Btrfs already run there
  (ADR 0127), and every benchmark row must match Linux (ADR 0107).

Chase chose the kernel on 2026-10-30, after the trade-off above.

**smoltcp in the kernel.** smoltcp 0.14 (0BSD) is a TCP/IP stack in safe
Rust (`#![deny(unsafe_code)]`) for systems without an operating system:
no threads, no locks and, without its `alloc` feature, no allocation.
It has window scaling, timestamps, CUBIC and selective acknowledgments
on the receive side. The kernel is built without Rust's infallible
allocation (ADR 0017): with `alloc`, smoltcp and the `managed` crate it
keeps its buffers in call `Vec::push` and `Box::new`, which do not exist
there, and the kernel's global allocator refuses every request anyway.
A third-party crate in the kernel becomes part of the trusted computing
base, which takes a decision record (docs/coding-standard.md).

## Decision

- **The stack runs in the kernel**, in a crate of its own,
  `kernel/net` (`radix-net`), which is `#![forbid(unsafe_code)]` like
  every parser of data from outside the kernel. Network cards' drivers
  run in the kernel too, as disks' do; a card's driver may also run as
  a program (the userspace e1000 driver of 12a.4) on the same rings.
- **smoltcp 0.14.0 from crates.io, without `alloc`.** Every buffer is
  lent to it as a slice that lives as long as what uses it: the stack's
  receive and transmit frames and its socket slots, from the kernel's
  heap. Only the protocols in use are built: Ethernet, IPv4, ICMP
  sockets and echo replies for now; later parts add UDP, TCP and IPv6.
  The neighbour cache holds 64 entries, as each one is looked for in
  turn. smoltcp's own dependencies come with it into the trusted
  computing base: `heapless` with `hash32` and `stable_deref_trait`,
  `bitflags` and `cfg-if` (MIT or Apache-2.0), `managed` (0BSD) and
  `byteorder` (MIT or Unlicense). 0BSD joins the kernel's license list
  (`kernel/deny.toml`): it grants everything and asks for nothing, not
  even the notice.
- **One thread does an interface's work.** Each card's thread (`net0`,
  `net1` ...) polls its stack when the card's interrupt wakes it, when
  the stack's next timer is due, and every 10 ms otherwise. Others reach
  the stack through its lock and wake the thread when they gave it work.
  smoltcp itself has no locks and is called only under that lock.
- **Frames are copied once each way**, between the card's own slots and
  the stack's frame buffers (ADR 0181). Zero-copy paths and offloads
  come with the work on throughput in 12a.3.
- **Sockets that come and go need their memory back.** smoltcp gives a
  socket's buffers no way out once it holds them. Before programs get
  sockets (12a.2), smoltcp is patched or extended so that a closed
  socket's buffers return to the heap; until then only the kernel's own
  sockets exist, and they live as long as the kernel.

## Consequences

- A bug in the stack is a kernel bug. Every parser of packets is safe
  Rust, fuzzed in CI from 12a.3, and the post-Phase-12 audit goes over
  the stack and smoltcp's code paths as it does the kernel's own.
- The stack can later move out of the kernel without changing programs:
  they reach it only through sockets and the kernel's capabilities.
- The plan's answer to this question (row 4 of its table of the spec's
  gaps) is a compartment: a crate that forbids unsafe code, with
  its own capability set, restartable after a panic. 12a.1 gives it the
  crate and a thread per interface; the capability set comes with
  sockets in 12a.2, and restarting the stack after a panic is left to
  the post-Phase-12 audit, which decides it against what it costs.
- smoltcp walks every socket on each poll and keeps neighbours in a
  list. 12a.3 measures this against Linux and fixes what the benchmark
  rows show, in smoltcp upstream where it can.
- Until `netd` configures interfaces (12a.4), the first card takes the
  address and router of the `ip=` and `gateway=` kernel flags, and
  `selftest=net` pings the router.
