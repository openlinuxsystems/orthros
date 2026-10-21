<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0029. Resource domains are capability-named kernel objects, and running out of memory stops whole domains

- Status: Accepted
- Date: 2026-10-21

## Context

Spec 4.1 makes resource domains (cgroup-style groups with a CPU weight
and a memory limit) first-class kernel objects, and the roadmap's Phase 4
adds an out-of-memory policy: apps are stopped first, system services
last, and Spark restarts what it supervises. Spec 3.4 and 16.1 forbid
silent kills in the trusted computing base, and the kernel must not
panic only because memory ran out. Spark and user processes arrive in
Phase 6, so Phase 4 has only kernel threads to apply this to.

## Decision

- Domains form a tree under the root domain `system`. Each has a CPU
  weight (1 to 10,000, default 100), an optional memory limit in pages,
  an out-of-memory class (`app`, `service` or `system`, never more
  protected than its parent's) and an optional supervisor. Up to 256
  domains, at most four levels below `system`.
- Domains are objects of the capability kind `ResourceDomain`: created
  together with their first capability, found through a capability, and
  taken off the table when the last capability goes. Kernel code can
  also make domains without a capability for its own use.
- Charges go up the tree, and a charge past any limit is refused with an
  error; nothing is killed for passing a limit. Phase 4 charges thread
  stacks; Phase 6 charges process memory.
- When the machine runs out of memory for an allocation that may sleep,
  the policy stops one whole domain at a time: the lowest class below
  `system`, then the largest user of memory. It asks the domain's
  threads to stop, logs a warning naming the domain, its class and its
  usage, fails the allocation at once if the asking thread is in that
  domain, and otherwise waits up to a second before the allocator
  retries. `system` domains are never stopped; if nothing else is left,
  the allocation fails with an error and the kernel logs it.
- A supervised domain is restarted once its threads are gone. Until Spark
  exists, a kernel thread named `supervisor` does this.

## Consequences

- Programs in Phase 6 get their domains the way they get every other
  resource, through capabilities, and a supervisor can hold a domain's
  capability to restart it.
- The policy never panics and never stops the trusted computing base;
  every stop is in the log.
- Stopping is cooperative for kernel threads. Phase 6 stops user threads
  at their next kernel entry, without their cooperation.
- Stopping a whole domain loses more work than killing one thread would,
  in exchange for leaving no domain half-running. Revisit if services
  need a finer policy, such as asking a domain to shrink first.
