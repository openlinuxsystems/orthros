<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0047. A process is a capability-named object that its maker builds, that ends with its threads or its maker, and that is reaped by deleting its last capability

- Status: Accepted
- Date: 2026-10-21

## Context

Until Phase 5 the kernel's test scaffolding (`user::launch`, ADR 0036)
made each program's capability space, address space and thread by hand,
and nothing tied them together: a fault in one thread ended that thread
only, and nobody could learn that a program had finished except by
watching its thread. Phase 6 needs real programs started from the boot
archive, by Spark and later by any loader in user space, so the kernel
needs an object that owns a program's resources, ends them together and
reports how the program ended, with no global process table and no
`fork`: a program must be able to build another one from capabilities
alone (Zircon's `zx_process_create` and seL4's hand-built threads are
the two ends of that range).

## Decision

- A **process** is a kernel object of kind `process` (radix-process(2)).
  It owns a capability space of 16 to 4,096 slots, an address space, a
  hidden *keep* space of 32 slots for capabilities its maker keeps for
  it (the frames of its image, so that it cannot unmap its own code by
  accident), and its threads. The holder of its capability with the
  control right builds it from outside with ten methods: `create`,
  `set_name`, `address_space`, `give`, `share`, `keep`, `start`, `exit`,
  `notify_exit` and `info`. There is no `fork` and no implicit
  inheritance: a program gets exactly the capabilities its maker gives.
- A process is **created**, **running** once its first thread starts,
  **ending** while its threads stop, and **exited** with a code. It
  ends when its last thread exits (with that thread's code), when a
  fault stops any of its threads (`EXIT_FAULT`), when a holder of the
  control right calls `exit`, when its last capability is deleted
  (`EXIT_STOPPED`), or when the process it was made for ends.
- Ending stops every thread at its next entry into the kernel; the last
  one to go empties both capability spaces, which frees everything the
  process held, lets go of its address space and signals the
  notification `notify_exit` named. What is left is the number and the
  exit code, until the last capability goes: that **reaps** it. Waiting
  for a child is waiting on that notification.
- A process made with `create` **belongs to the process it was made for**
  and ends with it. So whatever a program makes goes when it ends, even
  a process that holds the only capability to itself, which nothing could
  otherwise reach or end. Spark and the self-tests' programs are made by
  the kernel and belong to no process.
- Every object `create` makes is **charged** to that process's account
  in its resource domain (ADR 0029), by the bytes the kernel uses for it:
  a process's object, spaces and top page table; an endpoint; a
  notification. The charge goes when the object does.
- Each program receives its own process capability in slot
  `SLOT_PROCESS` with the read and control rights but not copy or
  transfer, so it can end itself and start threads but cannot hand
  control of itself to anyone.
- Ending a process can be triggered under the capability store's lock
  (its last capability went), but finishing it needs the store, so such
  processes are linked through themselves on a pending list that the
  store drains before its lock goes. Ending never allocates.

## Consequences

- Spark (ADR 0050) and any user-space loader (radix-rt's `spawn`, ADR
  0051) build programs the same way the kernel does, and the kernel's
  loader is only a convenience for the first one.
- A program cannot leave anything behind: the `fuzz` boot test now
  makes processes and calls every process method at random, and checks
  that the store and the object registry hold the same afterwards.
- Lower page tables are not charged yet, a process's name is at most 15
  bytes, and every process runs in the root domain until Spark makes
  domains for services (Phase 9); `docs/design/processes.md` lists these
  limits.
- The POSIX process model of Phase 8 (`fork`, `exec`, `waitpid`,
  process groups) is built on these objects in user space.
