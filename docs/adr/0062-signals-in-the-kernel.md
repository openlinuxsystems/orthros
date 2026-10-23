<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0062. The kernel delivers signals on the way back to user mode, with Linux's numbers and frame

- Status: Accepted
- Date: 2026-10-22

## Context

POSIX signals interrupt a thread between two of its instructions, run a
handler on its stack, and end, stop or continue whole processes. Only
the kernel can do that: it alone sees faults, knows where a thread is
when another process sends it a signal, and can end a wait early. The C
library has to describe the same signals, handler arguments and saved
context to the programs it builds, and mlibc already has Linux's
headers for them (`abi-bits`, ADR 0068).

## Decision

- Signals are **numbered as on Linux** (SIGHUP 1, SIGKILL 9, SIGRTMIN
  32, the last 64) and a set is a 64-bit word, so the C library's Linux
  headers describe them. A process has one action per signal; each
  thread has a mask, its pending signals and an alternate stack
  (radix-signal(2), radix-process(2)'s `signal_` methods). The rules,
  sets, actions, default actions and pending queues, are the
  `radix-posix` crate's, tested on the host.
- Sending does what the signal does at once (SIGCONT continues, SIGKILL
  and a default that ends the process end it), then makes it pending
  for the process or one thread, and wakes a thread that does not block
  it, so that a wait it is in ends as `interrupted`.
- A thread takes its pending signals **on its way back to user mode**,
  after every system call, interrupt and fault. A handler gets a frame
  laid out as **Linux's on x86-64**: the restorer's address, a
  `ucontext_t` with every register, the mask and the alternate stack,
  the `siginfo_t`, then the XSAVE area for x87, SSE and AVX, 64-byte
  aligned, leaving the 128-byte red zone alone. The restorer calls
  `signal_return`, which restores everything from the context the
  handler may have changed, through the slow return path.
- A wait a signal interrupts fails with `interrupted` (EINTR), unless
  the handler has SA_RESTART or no handler runs, when the call runs again
  from the start. Calls that wait for a time or for a signal (`poll`,
  `sigsuspend`, `sigtimedwait`, timed futex waits and so `nanosleep`)
  run again only when no handler ran, as on Linux.
- Faults raise signals for the thread that faulted: a page fault that
  cannot be resolved is SIGSEGV (SIGBUS when the file a page is filled
  from cannot be read), an undefined instruction SIGILL, a division
  error SIGFPE, a breakpoint SIGTRAP, a misaligned access SIGBUS. A
  fault whose signal has no handler, or is blocked, ends the process,
  killed by that signal; one that finds no memory for a page ends it as
  SIGKILL would.
- Real-time signals queue with their values, up to 1024 per process,
  and are delivered lowest number first, in order within a number
  (`sigqueue`, POSIX timers).

## Consequences

- mlibc's `sigaction`, `sigprocmask`, `sigsuspend`, `sigaltstack`,
  `sigtimedwait`, `sigqueue` and `ucontext` work unchanged, as do
  programs that read the context's registers.
- Every return to user mode checks one flag, which a signal sent while
  the thread decides it has nothing to do can never miss: the flag is
  set and cleared with the process's POSIX state and the thread's
  signals both locked.
- The frame copies the whole XSAVE area, so a handler costs a few
  hundred bytes of stack more than on a CPU without AVX; programs with
  their own small alternate stacks need MINSIGSTKSZ's Linux value.
