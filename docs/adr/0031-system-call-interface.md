<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0031. Programs enter Radix with SYSCALL, through twelve system calls in registers, and every other operation is a capability method

- Status: Accepted
- Date: 2026-10-21

## Context

The roadmap's Phase 5 asks for `SYSCALL` entry and exit with safe
user-copy primitives, the dozen core system calls, and a random
capability that every process receives at start, backed by the kernel's
CSPRNG (ADR 0022). Spec 3 makes capabilities the only way to name kernel
objects (ADR 0023), so the open questions were how many operations get
system call numbers of their own, how arguments and errors travel, how
the kernel touches user memory without trusting it, and how a program
gets random numbers before it has any service to ask.

## Decision

- Programs enter with `syscall` and leave with `sysretq` only; there is
  no `int 0x80` gate. FMASK clears IF, TF, DF, AC, IOPL and NT, so the
  kernel always starts with interrupts off and SMAP in force. The stub
  saves every register and restores every register on the way out, and
  the kernel checks that the return address is a user address and the
  flags are ones a program may hold before `sysretq`.
- There are twelve system calls: the four of IPC (`cap_call`, `send`,
  `recv`, `reply_recv`), the two of notifications (`notify`, `wait`),
  `yield`, `map`, `unmap`, `ring_setup`, `ring_enter` and `debug_log`.
  Everything else is a method of an object, called with `cap_call` on its
  capability, its label `(interface << 16) | method`. Labels from
  0xffff0000 up are the methods every capability has (info, derive,
  move_to, delete, revoke, seal, unseal), so no endpoint protocol can
  collide with them.
- The number goes in `rax`; arguments and results in the same ten
  registers (`rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`, `r12` to `r15`), so a
  message of eight words, its capability and its info word fit both ways
  without memory. `rcx` and `r11` are overwritten; every other register
  keeps its value. A failed call leaves 0 in every result register.
- The status in `rax` is 0 or `(detail << 32) | (domain << 16) | code`.
  Domain 1 is the kernel, whose codes each map to an errno for the C
  library; later phases give services domains of their own.
- The kernel reads and writes user memory only through `copy_from_user`
  and `copy_to_user`: the range is checked against the user half without
  overflow, SMAP is lifted for one `rep movsb`, and a page fault there is
  fixed up into `bad_address`. Memory shared for longer (a ring's pages)
  is allocated by the kernel and accessed through its own mapping.
- The user half runs from 0x10000 to one page below its end: the pages
  below catch null pointers, and the last page stays unmapped so no
  `syscall` can sit where `sysretq` would return to a non-canonical
  address.
- A user-mode exception ends its thread with `EXIT_FAULT` and a log line;
  it never reaches the kernel's panic path.
- Every program receives a capability to the kernel's random number
  generator in slot 4 of its start-up page's layout, next to its thread,
  address space, memory and kernel log. Its `read` method fills a buffer
  (as getrandom(2) does) and `words` returns 64 bytes in registers.
  Random numbers are a capability rather than a system call so that a
  sandbox can be started without one, like any other authority.

## Consequences

- The kernel's interface stays small; new kinds of object add methods,
  not system calls, and their arguments go through the same decoding.
- A method costs one capability lookup more than a dedicated system call
  would; the benchmarks (ADR 0035) measure it as `cap_info`.
- A program can never see a kernel register value, since every register
  is restored or zeroed on the way out; the probe program checks this.
- Speculative-execution mitigations on entry (IBRS, retpolines,
  page-table isolation), which spec 3.3 asks for, are not in Phase 5;
  when they come, the benchmarks show what each costs.
- The calls and methods are defined in `abi/*.ridl` (ADR 0033), so their
  register layouts and errors cannot drift between the kernel, the
  libraries and the manual.
