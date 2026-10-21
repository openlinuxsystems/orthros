<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0033. The system call interface is defined once in ridl, and ridlc generates the Rust bindings, C headers and man(2) pages

- Status: Accepted
- Date: 2026-10-21

## Context

Phase 5 asks for `ridlc`, an IDL compiler generating Rust, C and man(2)
output. The interface of Phase 5 is twelve system calls, the methods of
fifteen kinds of object, the kernel's errors and the layouts of shared
memory (the start-up page, a ring's header and entries). The kernel, the
Rust library, the C library to come in Phase 8 and the manual each need
it, and hand-written copies drift. Existing IDLs (protocol buffers, Cap'n
Proto, Fuchsia's FIDL, seL4's CAmkES) describe messages in memory and
bring a runtime or a toolchain of their own; none says which register an
argument travels in, which errors a call raises, or what a man page
should say.

## Decision

- The interface lives in `abi/*.ridl`, one module per file, in a small
  language of our own: constants, flags, enums, structures, system calls
  and interfaces with numbered methods, with doc comments that become
  the manual. Types are primitive integers, slots, addresses, fixed
  arrays, flags, enums, structures, and byte buffers passed by address
  and length (`in bytes`, `out bytes`).
- `ridlc` (`tools/ridlc`) parses and checks the files and writes three
  outputs: the Rust bindings of `radix-abi` (from its build script, so the
  kernel's build always uses the current definitions), C headers with
  `static inline` wrappers (installed by `ob build` under
  `out/sysroot/include/radix/`), and one scdoc man(2) page per system call
  and per module (built by `ob docs`).
- The checks refuse rather than warn: every item, member and parameter is
  documented; names and numbers are unique; a call's arguments and
  results fit the ten registers; a structure has no hidden padding; every
  error raised exists; the object kind an interface is for exists.
- The generated Rust gives the kernel `decode` functions that check every
  argument, so the kernel never acts on an invalid enum value or flag,
  and gives programs wrappers generic over a `Syscalls` trait, so the
  bindings are tested on the host against a mock.
- `ridlc` has no dependencies, since it runs inside the kernel's build.

## Consequences

- An interface change is one edit to a `.ridl` file; the kernel, the
  libraries and the manual follow it at their next build, and an
  undocumented addition does not build at all.
- The language grows only as the interface needs it; Phase 6's services
  will need interfaces over endpoints with their own protocols, which
  ridl describes the same way.
- The generated pages are not committed, so the manual's section 2 is
  only in `out/` and in images; `ob docs` builds it.
- Stability rules (versions per interface, what a compatible change is)
  are recorded per interface (`version 1` today) and enforced once there
  is a release to stay compatible with (Phase 14).
