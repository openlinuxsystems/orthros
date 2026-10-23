<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0043. System calls are fuzzed from user mode on every push, and every error a call returns must be in its documentation

- Status: Accepted
- Date: 2026-10-21

## Context

Phase 5's tests call every system call and method with chosen good and
bad arguments (the `user` boot test's probe) and run fixed pairs of
programs (`ipc`). The audit that closed the phase asked what no chosen
test can: whether any sequence of system calls a program can make
crashes the kernel, trips the lock checker or leaves an object behind.
The plan's test layers list a system-call fuzzer in QEMU among the
fuzzing jobs. The audit also found that the `raises` lists of
`abi/syscalls.ridl`, from which ridlc writes each call's ERRORS section
in its man page, missed errors the code returns for eleven of the
twelve calls: nothing compared the two.

## Decision

- A `fuzz` boot test (`selftest=fuzz`) starts eight groups of three
  fuzzer programs and a janitor on 2 CPUs with the lock checker on. A
  fuzzer makes 4,000 random system calls, methods and ring entries from
  a seed, with arguments drawn mostly from values at the edges of what
  the kernel accepts (slot numbers, rights masks, labels, lengths,
  addresses around the user half's ends and the program's own unmapped
  and read-only pages). Each group shares two endpoints (one badged), two
  notifications and a memory object, so calls that wait can meet a
  partner and capabilities travel. The janitor keeps letting go of
  fuzzers that wait, so the three cannot all end up waiting for each
  other. The design follows Trinity's and syzkaller's (argument
  generators that know each call's types, and a log that repeats a
  run); no code is taken from either.
- The seeds are in the log, and `fuzz-seed=N` on the kernel command line
  repeats them. The random generator is Marsaglia's xorshift64* seeded
  through SplitMix64 (Vigna), written from the published algorithms.
- ridlc generates `raises::syscall(number)` and `raises::method(label)`
  from the IDL, and the fuzzer checks every failed call against them: an
  error a call's documentation does not list fails the test, as does a
  failed call that returns anything but a kernel error and zero in its
  result registers (radix-ipc(7)).
- The test fails if a fuzzer exits with failures, fewer than half get
  through their calls, the capability store fails its audit, or the
  store or the object registry holds anything more than before. A group
  that overruns its 30 seconds logs the thread table, each CPU's run
  queue and the threads asleep on its timer, and a thread stopped inside
  a system call logs which one, so a hang names what waits for what.
- A panic now prints each stopped CPU's backtrace: the NMI that stops the
  other CPUs records where each was, which is how a deadlock with
  interrupts off on another CPU shows up.

## Consequences

- The fuzzer found three kernel bugs, fixed with it: a reply that failed
  (no room for a capability, say) dropped the caller it answered, which
  then waited for good; a thread's exit took its lock-checker lock with
  interrupts on, so a tick that switched it out there deadlocked its CPU,
  which showed only as another CPU's TLB shootdown timing out; and the
  documentation gaps above, now filled.
- Each run uses new seeds, so CI explores a little more on every push;
  a failure names the seed to repeat it locally.
- The fuzzer avoids the calls that would end its own program (unmapping
  its code, deleting its own thread or address space). Those paths are
  the `user` test's faults; a fuzzer with a process to sacrifice comes
  with Phase 6's processes, and longer runs under KVM on a dedicated
  runner stay in the plan.
- Adding an error to a call's code without adding it to the IDL now
  fails CI, so the man pages stay complete.
