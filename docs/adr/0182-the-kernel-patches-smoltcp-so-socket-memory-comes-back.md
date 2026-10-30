<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0182. The kernel patches smoltcp, so that a socket's memory comes back

- Status: Accepted
- Date: 2026-10-30

## Context

smoltcp, the kernel's TCP/IP stack (ADR 0180), is built without `alloc`,
so every buffer a socket has is lent to it as a `&'a mut` slice. Its
sockets keep those slices for as long as they live, and smoltcp 0.14.0
has no way to take a socket or a buffer apart again:
`SocketSet::remove` gives the socket back, but nothing gives back what
it was made with. The kernel's own sockets live as long as the kernel,
so 12a.1 could lend them memory forever. Programs open and close
sockets all the time (12a.2), and a closed socket's buffers must go back
to the heap.

smoltcp's socket set also panics when it is full, and when a handle is
not one of its sockets or names a socket of another kind. A kernel
that panics because a program opened one socket too many is broken.

The phase's prep doc keeps smoltcp as a pinned fork, with its fixes
offered upstream as the mlibc patches are. The ports already patch
crates from crates.io for Radix (ADR 0080), but the ports build after
the kernel, with a toolchain of their own, and the kernel builds without
them. Copying smoltcp's source into the repository would add 57,000
lines that are not Dukovo's and hide what was changed.

Getting memory back from a `&'static mut` slice takes `unsafe` code,
and holding the slice again is not all it takes. Rust lets a function
count on what its `&mut` arguments point to staying there until it
returns (Miri's borrow models call those references protected), so
freeing memory that a still-running call was given a reference to, even
inside a struct, is undefined behavior. A safe "here is the slice back,
free it" call would let safe code do exactly that.

## Decision

- **The kernel patches crates from crates.io the way the ports do, from
  its own directory.** `kernel/patches/crates.toml` lists each crate
  with its version, the SHA-256 of its `.crate` file and its license,
  and `kernel/patches/<name>-<version>.patch` holds the change, with a
  header that says what it does and why. `ob crates` fetches each crate
  once, checks its hash, applies the patch and puts the result in
  `kernel/patched/<name>-<version>` (ignored by git), where
  `[patch.crates-io]` in kernel/Cargo.toml takes it from in place of
  crates.io's. A crate already there from the same file and patch is
  left alone. Every Cargo command `ob` runs on the kernel does this
  first, as does `ob setup`; CI's cargo-deny job runs `ob crates`
  before it checks the kernel.
- **smoltcp 0.14.0 is the first, with one patch** (0BSD, as smoltcp is,
  to be offered upstream):
  - `into_buffers` on TCP, UDP and ICMP sockets and `into_storage` on
    `RingBuffer` and `PacketBuffer` give the socket or buffer up and
    return what it was made with;
  - `SocketSet::try_add`, `try_get`, `try_get_mut` and `try_remove`
    return an `Option` (and `try_add` the socket that did not fit)
    where `add`, `get`, `get_mut` and `remove` panic. `radix-net` uses
    only these.
- **Memory is lent through leases** (`radix_mm::lease`). `lend` turns a
  vector into the `&'static mut` slice smoltcp is given and a `Lease`,
  which knows the allocation and never touches it. `Lease::end` frees
  the memory, given back where the slice is, and refuses any other
  address or length. It is `unsafe`: its caller promises that the slice
  is not used again, and that no call still running was given it as an
  argument. The slices come back as the return values of the calls that
  take a socket apart (`Pinger::close`, and the sockets of 12a.2b), and
  are turned into raw pointers before anything else is done with them,
  which keeps that promise. A lease dropped without its slice leaks the
  memory, which is safe.
- **The kernel's self-test pinger is the first socket whose memory comes
  back**: `selftest=net` lends its buffers, closes the pinger after its
  pings, and fails if the memory does not come back.

## Consequences

- The kernel builds smoltcp from crates.io's file with a change that is
  short and in one place, and that a version bump re-applies or shows
  to be stale. Editors' Rust tools need `ob setup` or `ob crates` once
  before they understand kernel/Cargo.toml, and so do Cargo commands
  run by hand on the kernel.
- `ob` needs `patch` and `curl` to build the kernel, not only the ports;
  `ob doctor` says so.
- cargo-deny checks the licenses of the patched crates as before, from
  their Cargo.toml, and `ob`'s license manifest records each as the
  crates.io crate it is, patched, with the hash from the list. Its
  RustSec advisory check covers only crates from a registry, so it no
  longer sees smoltcp: a patched crate's version is checked against
  RustSec when it changes, and the post-Phase-12 audit adds a check of
  its own.
- The lease's unit tests pass under Miri with both of its borrow models
  (Stacked and Tree Borrows), run by hand when this was written; CI does
  not run Miri. Both models catch a version of `end` that frees the
  memory while it holds the slice as its argument, which is why `end`
  takes a raw pointer.
