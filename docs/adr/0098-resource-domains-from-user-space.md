<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0098. Spark makes the services' resource domains through capabilities, and restarts them itself

- Status: Accepted
- Date: 2026-10-23

## Context

Resource domains came in Phase 4 (ADR 0029, with the scheduler of ADR
0027): groups of processes with a CPU weight, a memory
limit and an out-of-memory class, which the kernel stops whole when the
machine runs out of memory. Only the kernel could make one, and a
stand-in supervisor in the kernel restarted a stopped domain. The plan
moved "the kernel interface for making resource domains and setting
priorities" to Phase 9, for Spark to give each service a domain of its
own and to take over the supervisor's job.

## Decision

- Resource domains get an interface, radix-resource-domain(2), on the
  ResourceDomain capabilities that already existed: `create` (a child
  domain with a name, weight, memory limit and class no more protected
  than its parent's), `set_weight`, `set_limit`, `info`,
  `create_process` (a process in the domain, which its forked children
  share and cannot leave) and `stop` (every thread of the domain and
  the domains below it is asked to stop).
- The kernel makes a `services` domain, of the service class, below its
  root domain, and gives Spark a capability with every right to it in
  startup slot 9. Spark makes a domain for each service below it, named
  after the service.
- Domains that programs make have no supervisor in the kernel. When the
  out-of-memory policy or `stop` ends one, its processes end as if
  killed by SIGKILL (`EXIT_STOPPED`), the stop is counted in `info`'s
  `oom_stops` when it was the policy's, and the domain takes new threads
  again once its last thread has gone. Spark, which hears that its
  service ended, starts it again in the same domain.
- The kernel's own stand-in supervisor stays for domains it makes.

## Consequences

- Each service's CPU share and memory are its own: a runaway service is
  limited by its file's `memory` and `weight`, and the out-of-memory
  policy stops the biggest service domain, which Spark restarts, rather
  than any single process.
- The restart policy lives in one place, Spark, for every way a service
  can end.
- A domain is only as deep as DEPTH_MAX (4) below the root, and its name
  is at most 23 bytes, the limit service names inherit.
- Phase 13's user sessions will get domains the same way, from their
  login's supervisor, of the app class.
