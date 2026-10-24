<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0104. Thread cancellation points are system calls made as musl makes them, through a stub the signal handler can see

- Status: Accepted
- Date: 2026-10-24

## Context

POSIX has a deferred cancel end a thread at a cancellation point: while
it waits in one of the calls POSIX lists (`read`, `accept`, `sleep`,
`pthread_cond_wait`, `sem_wait`, `waitpid` and some forty more), or as
it comes to one, and nowhere else. mlibc's SIGCANCEL handler acted on a
deferred cancel only through a system layer function,
`BeforeCancellableSyscall`, which the Radix layer did not have, so a
deferred cancel ended a thread at `pthread_testcancel` alone. A thread
blocked in `read` was never cancelled, and mlibc's own `pthread_cancel`
test and four of libc-test's were left out of the `posix` boot test.

A point must act on a cancel only if its call has done nothing: a
`read` that took bytes, or an `accept` that made a descriptor, has to
return them, or they are lost. There are three known ways:

- Turn on asynchronous cancellation around each call that may wait, as
  glibc long did. A cancel that comes just after the call has done its
  work still ends the thread, and the work is lost (glibc's bug 12683).
- Have the kernel fail a call with "cancelled". The kernel would need to
  know the C library's cancel state, which is in user memory and changes
  with `pthread_setcancelstate` without a system call.
- musl's: make each point's system call from a short stub with labels
  around its `syscall` instruction. The stub reads the cancel state
  before the instruction, and the signal handler acts only on a thread
  it finds between the labels, where the call has not been made. glibc's
  fix for bug 12683 works the same way.

## Decision

- Points are made as musl makes them. `ridlc` gives each method's C
  function a `_with` form that takes the system call function
  (`radix_syscall_fn`), and the plain function calls it with
  `radix_syscall`, so it inlines as before. The Radix layer makes a
  point's call through `radix::cancellable`, the stub
  `__radix_cancellable_syscall` in `x86_64/cancel.S`, and every other
  call, the C library's own reads and writes among them, plainly.
- Between `__radix_cancel_begin` and `__radix_cancel_end` the stub
  reads the thread's cancel bits; when cancellation is enabled and a
  cancel is due it jumps to `__radix_cancel_now` rather than make the
  call. `__radix_cancel` then ends the thread, or, for
  `radix::cancellable_masked`, returns a status that maps to ECANCELED,
  so that a caller that holds something, a condition's mutex or `poll`'s
  copy of its entries, puts it back first and then acts.
- SIGCANCEL's handler asks a new system layer function,
  `CancelInContext`, about the context it interrupted. Between the
  labels, it moves the context to `__radix_cancel_now`, and the thread
  acts in its own frame once the handler returns. The handler has
  SA_RESTART, so a wait the kernel restarts (`Restart::Always`) is put
  back on the `syscall` instruction, between the labels; a wait the
  kernel ends for good (`NoHandler`) returns interrupted at the end
  label, where the caller reads the bits again. Anywhere else the
  handler leaves SIGCANCEL blocked in that context and sends it again,
  as musl does: a cancel that came while another signal's handler ran
  over a point arrives once that handler returns there.
- The points are POSIX's "shall occur" list as far as Radix has the
  calls, and each acts on a due cancel even when it need not wait, as
  POSIX asks and musl does (`open`, `close`, `sem_wait`, `pthread_join`
  of an ended thread, a zero sleep). Of the "may occur" list, none: stdio,
  `popen`, `pclose`, `wordexp`, `opendir` and `closedir` make plain
  calls, as on musl, so that a cancel does not leave a child or a stream
  to nobody.
- `system` is a point and cleans up as glibc's does: a cancel while its
  command runs kills the command and waits for it, and SIGINT and
  SIGQUIT, which it ignores, are counted across threads, so they come
  back only when the last thread's command is done.
- An exiting thread disables its cancellation, so that a point in a
  cleanup handler or a key's destructor goes on, and runs its cleanup
  handlers before its `thread_local` destructors, which they may use, as
  glibc does. `sigtimedwait` never takes the C library's own signals.
- The generic part is mlibc patch 0041, meant for upstream: the
  handler, `CancelInContext` and three more tags (`CancellableRead`,
  `CancellableWrite`, `CancellableFutexWait`), and `system`, `pclose`,
  `wordexp`, `closedir` and thread exit. The rest is the Radix layer's.

## Consequences

- A call that is no point costs what it did; a point's call costs a
  function call and a few instructions more.
- mlibc's `pthread_cancel` test and libc-test's `pthread_cancel`,
  `pthread_cancel-points`, `pthread_cancel-sem_wait` and
  `pthread_cond_wait-cancel_ignored` run in the `posix` boot test;
  `pthread_cancel-points` leaves out `shm_open`, which Radix lacks. The
  new `cancel` test cancels a thread in each of 26 waits, with a cancel
  due at 28 points, at calls that are no points, in a handler of another
  signal, and in `system`. Run on glibc 2.39, it differs only where it
  says it follows musl.
- The stub is x86-64's, as `radix_syscall` is. Another architecture
  needs its own, with the same labels and registers kept where
  `CancelInContext` expects them.
- The dynamic linker makes only plain calls: it has no threads to
  cancel.
- Testing the points found `sigwait` returning -1 where POSIX has an
  error number, and failing with EINTR where glibc's goes on waiting:
  patch 0042.
- A thread cancelled inside the resolver (`getaddrinfo`) leaves its
  socket open, until the resolver is redone with networking (Phase 12).
