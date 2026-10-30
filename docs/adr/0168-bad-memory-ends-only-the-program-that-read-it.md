<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0168. Bad memory ends only the program that read it, and its page leaves use

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11d, its fifth part. ADR 0145 turned machine-check reporting on
and made the #MC handler panic on every error the CPU cannot go on after
by itself, including those in a program's memory, since the kernel could
not take a page out of use. Memory that goes bad is the error a long-running
machine meets most, and on a machine whose CPU can contain it, only the
code that reads it need stop.

Intel's software error recovery (IA32_MCG_CAP's SER_P, on Xeons) says,
for an uncorrected error, whether it was signalled (S), and whether the
code that met it must not go on (AR, "SRAR"), or need not (SRAO, such as
a patrol scrub finding bad memory); an unsignalled one is UCNA. AMD's
SUCCOR (CPUID 0x80000007's EBX bit 1, on Zen, the Framework's among them)
raises #MC when code consumes poisoned data, which the status's Poison
bit marks, with the data's physical address. Linux grades errors by a
table of these bits (`arch/x86/kernel/cpu/mce/severity.c`): a corrupt
processor state panics; an SRAR in a program, a data load or instruction
fetch whose address is known, ends the program with SIGBUS
(`BUS_MCEERR_AR`, `si_addr` the page, `si_addr_lsb` 12) and retires the
page (`memory_failure`), replacing the program's mapping with an entry
that gives SIGBUS on the next touch; an SRAR in the kernel panics, unless
a copy from user memory can be ended instead; SRAO and UCNA retire their
page when they are read from the bank; and an error with another lost
(overflow) panics where an action was required. Without recovery, an
uncorrected error panics in the kernel and lets a program go on if its
code can restart. Retired pages are counted in `/proc/meminfo`'s
`HardwareCorrupted`.

The Dell's Kaby Lake has no SER_P, so its uncorrected errors still panic
in the kernel; QEMU's CPUs report SER_P, which its `mce` monitor command
uses to inject errors, and the Framework's Zen 5 has SUCCOR.

## Decision

- **`radix-mca` grades the errors** (`triage`), by Linux's rules cut to
  what the kernel does: go on, end the program that was running, naming
  the page if the CPU says where it is to within a page, or stop the
  machine. Intel's SRAR in user mode ends the program when it is a data
  load or instruction fetch with its address and more about it valid and
  no error lost; one that was not about the interrupted code (RIPV set,
  EIPV clear) lets it go on; any other SRAR stops the machine. SRAO and
  UCNA let the code go on. AMD's uncorrected errors with SUCCOR end a
  program and stop the kernel. Without recovery the rules of ADR 0145
  stay, but a program whose code can restart goes on, as in Linux. Then,
  if no error asked for more, code that cannot restart ends its program,
  or in the kernel stops the machine. `bad_page` names the page an
  uncorrected error, or AMD's deferred one, is about: Intel's
  IA32_MCi_MISC must say the address is physical and valid to a page or
  finer; AMD's status must name poisoned or deferred data.
- **A machine check that ends a program moves off its stack.** The #MC
  handler runs on its own stack, where nothing may sleep. It ends the
  machine check, copies the program's registers to the top of the
  thread's kernel stack, where any other exception from user mode leaves
  them, and goes on there as the entry stub would: the fault handler
  hears of a read of bad memory (`UserFaultKind::Memory`) and the way
  back to user mode delivers its signal.
- **The fault handler** first reads the CPU's banks, before the thread can
  move to another CPU: the errors are logged and their pages retired, as
  by the `mce` thread's poll. It then takes the page out of the program's
  private anonymous memory, where it is mapped, and sends SIGBUS with
  `BUS_MCEERR_AR`, the page's address and `si_addr_lsb` 12. Like any fault
  signal, it ends the program if it has no handler or blocks it.
- **Retiring a page** (`Frames::retire`): the frame allocator lists the
  frame, at most 512 (2 MiB), and takes it out of its zone at once if it
  is free, from a CPU's cache or run too; a frame in use leaves when its
  block is freed, and no allocation hands one out meanwhile. A frame listed
  is never handed out again. `/processes/meminfo` counts the retired memory
  as `hardware_corrupted`.
- **A retired page leaves the program's memory** for a swap entry no swap
  area holds (`swap::POISONED`): a fork shares it, an unmap drops it, and
  a fault on it gives SIGBUS with `BUS_MCEERR_AR` instead of reading it
  back, so a handler that goes on meets the fault, not the machine check.
  Shared memory, file pages and memory lent to a device keep the frame
  mapped: their next read meets the bad memory again and ends the same
  way.
- The `mce` thread's poll retires the page of each uncorrected error it
  reads, SRAO and UCNA among them, which Linux calls a late kill: a
  program that maps it ends only if it reads it.

## Consequences

- The boot test `machine-check-program` runs a program reading a page of
  its own on an emulated Intel CPU. With the machine stopped where a CPU
  runs it in user mode, QEMU injects an SRAR naming the page's physical
  address; the program must get SIGBUS with the page's address and
  `si_addr_lsb` 12, fault again without a machine check on the next read,
  and see 8 KiB of `hardware_corrupted`, the other page being an SRAO
  injected before, which the poll retired. The host tests grade Intel's,
  AMD's and recovery-less errors, and retire frames free, cached, in a
  CPU's run and in use.
- On the Framework, a program that consumes poison ends instead of the
  machine; the Dell's errors behave as before. Neither is checked on the
  machine yet.
- Not yet: dropping a bad page from the file cache, so that a file read
  in the kernel does not meet it (it is retired once evicted), and a
  copy from user memory in the kernel ending its call rather than the
  machine. Those panic, as before. Error counts by memory controller
  (EDAC), CMCI and AMD's thresholds come in the next part of 11d.
