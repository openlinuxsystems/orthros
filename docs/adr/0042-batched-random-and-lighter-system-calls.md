<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0042. Per-CPU random generators work a batch at a time, and system calls and wake-ups skip work they do not need

- Status: Accepted
- Date: 2026-10-21

## Context

With ADR 0041 every row of the performance gate (ADR 0039) passed, but
some only just: 64 random bytes took 1.00 times Linux's time on two AMD
runners, a capability method 1.01 on another, and a same-CPU channel
round trip 1.03 on an Intel one. Noise of a few percent between runs
could fail the gate on any of them. Instruction traces of one operation
each (QEMU's debugger stub and single steps, as for ADR 0040) showed:

- `random.words` took 3,339 kernel instructions, 2,618 of them in two
  ChaCha20 blocks: one to make the next key and 32 output bytes, one
  for the other 32 bytes. Linux's `getrandom` runs two blocks for 64
  bytes as well.
- Every system call held preemption off to read the running thread
  from its CPU's scheduler slot, and copied its results three times on
  the way out. A capability method took 426 instructions.
- Waking a thread computed which CPU it should run on from every CPU's
  load: it took the online set's lock and built and copied a table of
  `MAX_CPUS` loads, 3,640 of the 11,499 instructions of a same-CPU
  channel round trip, although the benchmark's programs are pinned to
  one CPU each.

## Decision

- `radix_random::Batch` runs its generator a batch at a time: one
  request of eight ChaCha20 blocks, the first 32 bytes of which are the
  next key, and keeps the other 480 bytes to give out as asked,
  zeroing each as it goes. This is the buffered form of Bernstein's
  fast-key-erasure design, which OpenBSD's `arc4random` and Linux's
  batched `get_random_u64` and `vgetrandom` also use (no code is taken).
  Forward secrecy is unchanged: the key that made a batch is gone once
  it is made, the bytes given out are zero, and the bytes still in
  memory are ones the key could make anyway. Reseeding throws the rest
  of a batch away. `Local`, the per-CPU generator for programs, uses a
  batch; `words` copies with a fixed size; requests larger than a batch
  go to the generator directly.
- The architecture keeps a word for the running thread in its per-CPU
  block (`gs:[40]` on x86-64; `CpuLocal::running_word`), written at
  every switch. `sched::running` reads it in one instruction, which a
  migration cannot split, without holding preemption off.
- A system call writes its results straight into the saved argument
  registers (`syscall::results`), and the answer of a method is matched
  in place, not through `?`, which copied it.
- The capability cache's miss path is a separate cold function, so a
  hit needs less of the stack and fewer saved registers.
- `choose_cpu` sends a thread allowed on one CPU there if it is online,
  and a thread whose last CPU is allowed and idle back to it, which is
  what the topology would choose first, before reading any other CPU's
  load or taking the online set's lock.

## Consequences

- Measured in instructions, under QEMU's TCG: `random.words` 2,250 (from
  3,339), a capability method 369 (from 426), a same-CPU channel round
  trip 6,572 (from 11,499). Every system call is about 30 instructions
  shorter. The `ob bench` numbers under KVM are in the pull request.
- Each CPU's generator holds up to 480 bytes of future output in memory,
  as OpenBSD's and Linux's do; the per-CPU generators take 144 KiB of
  static memory for `MAX_CPUS` CPUs, 120 KiB of it batches.
- `PerCpu::with` is gone; code that needs the current CPU's value holds
  preemption off itself, or uses `PerCpu::with_own`.
- Finding the object a capability names still takes the object
  registry's lock and clones a reference, about 90 instructions of every
  IPC system call; a registry read under RCU is the fix if Phase 6's
  services show it matters, as `docs/design/ipc.md` already notes.
