<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Scheduler

This note describes the Radix scheduler as of Phase 4: threads and their
contexts, the run queues and the scheduling classes, how threads block,
wake and switch between CPUs, placement and load balancing, mutexes with
priority inheritance, RCU, the lock checker, and resource domains with the
out-of-memory policy. It ends with the semi-formal argument that the
scheduler is correct (spec 15) and the checks behind it. The user-facing
description is `radix-scheduling(7)`; `radix-debug(7)` covers the
self-tests, the lock checker's reports and the `threads`, `sched`,
`domains` and `locks` commands of `kdb`. The decisions behind the design
are ADRs 0027 to 0030.

## Crates

```text
kernel/sched            radix-sched: the policy, as plain data structures with
                        no locks and no hardware: nice weights (weight.rs), CPU
                        sets (cpuset.rs), the EEVDF fair queue (fair.rs), the
                        real-time queue (rt.rs), the topology and the placement
                        and balancing decisions (topology.rs); host tests,
                        property tests and Kani proofs (proofs.rs)
kernel/sched/tla        Scheduler.tla: the TLA+ model of blocking, waking and
                        the switch handoff, with two TLC configurations
kernel/lib/lockdep      radix-lockdep: the lock-order checker
kernel/lib/sync         radix-sync: spin locks, mutexes, RCU read sections and
                        the hooks through which they reach the scheduler
kernel/arch/x86_64      thread.rs: contexts and the switch; fpu.rs: extended
                        state; lib.rs: idle with MWAIT or HLT
kernel/radix            sched/: the mechanism (threads, run queues, the switch,
                        waiting, balancing, RCU grace periods, lock hooks);
                        percpu.rs; domains.rs; oom.rs; work.rs;
                        schedtest.rs: the Phase 4 self-tests
```

`radix-sched` and `radix-lockdep` forbid `unsafe` and build for the host,
where their tests run. The kernel side has one new `unsafe` site, the
call into the context switch (`sched::switch`), whose contract is that
both contexts are valid and the outgoing one is the running thread's.

## Threads and contexts

A `Thread` (`sched/thread.rs`) is shared through `ThreadRef`, an `Arc` in
the kernel heap. The registry holds every thread until the `reaper`
thread frees it, so references dropped elsewhere (in run queues, the wait
table, timers) are never the last, and nothing is freed in an interrupt
handler or under a run-queue lock. A thread's scheduling state (`State`:
ready, blocked with a reason, or dead; its policy, affinity, lag and lent
priority) sits under the thread's own spin lock. What a run queue needs
while it holds only its own lock (the class the thread is queued in, its
weight, the CPUs it may use) is copied into atomics written with both
locks held.

A context (`arch/x86_64/src/thread.rs`) is a stack and a one-page
*context block*: the saved stack pointer, an `on_cpu` word, and the
extended-state save area. A new thread's stack (16 KiB), block and an
unmapped guard page below them are one area of the mapping window, so an
overflow faults on the guard. Each CPU starts with an idle thread made
from its start-up stack; the boot code becomes the thread `kmain`.

**Eager extended state.** Every switch saves the outgoing thread's x87,
SSE, AVX and AVX-512 registers (those the CPU has; XCR0) with XSAVEOPT,
or XSAVE, or FXSAVE without XSAVE, and restores the incoming thread's.
Lazy switching would trap a thread's first use after a switch instead,
which leaked one thread's registers to another through speculative
execution (CVE-2018-3665) and costs a trap per switch for any thread that
computes. The kernel itself is built without SSE, so only threads' own
code touches the registers.

## Run queues and classes

Each CPU has one run queue (`sched/queue.rs`) under one spin lock, taken
with interrupts off. The running thread stays in its queue, marked as
running. `Rq::pick` takes the most urgent real-time thread, else the fair
queue's pick, else the CPU's idle thread, which is in no queue.

**Fair class.** `FairQueue` (`fair.rs`) is EEVDF (Stoica and
Abdel-Wahab, 1995). Each entity has a weight `w` (1024 at nice 0; each
nice level weighs 1.25 times the next, computed in `weight.rs`), a
virtual runtime `v` that advances by `t · 1024 / w` while it runs for
real time `t`, and a virtual deadline, `v` plus its slice (3 ms) in
virtual time. The queue's virtual time `V` is the weight-averaged `v`. An
entity's lag `V − v` is the service it is owed; it is *eligible* when its
lag is not negative, and the queue runs the eligible entity with the
earliest deadline. An entity that leaves takes its lag along, clamped to
about two slices, and comes back with it. A new entity starts at lag 0
with half a slice.

A fair thread that yields gives up the rest of its slice, as in Linux
since 6.18: if it is eligible, its `v` moves on to its deadline and it
gets a new deadline a slice later, and until the next pick it loses ties
of deadline to the others (otherwise the running entity wins them, which
saves needless switches). Moving only the deadline, as Radix did before
ADR 0041, kept the CPU with a thread that was owed time while it was the
only eligible one: two programs yielding to each other on one CPU could
make tens of thousands of yields without a switch, up to the lag limit.

The queue keeps `W = Σ w` and `S = Σ w (v − v₀)` for a base `v₀`, the
smallest `v` (moved by `rebase`), so `V = v₀ + S / W` and eligibility,
`(v − v₀) · W ≤ S`, are exact integer arithmetic in `i128`. Virtual times
are `u64` that may wrap; only differences, as `i64`, are compared. The
divisions (`mul_div`, `avg_vruntime`) are done in 64 bits whenever the
numbers fit, which they nearly always do, and in 128 bits otherwise: the
answer is the same, and x86-64 divides 128-bit numbers in a library call
several times dearer, on every switch (ADR 0040).

**Real-time classes.** `RtQueue` (`rt.rs`) holds FIFO and round-robin
threads, priorities 1 to 99. The highest priority runs; among equals the
one queued first. A round-robin thread goes to the back of its priority
after each 100 ms quantum. A thread preempted by a higher priority keeps
its place at the front. A real-time thread's effective priority is the
higher of its own and the one lent to it (see *Mutexes and priority
inheritance*); a fair thread with a loan is queued as real-time.

**Domain weights.** A fair thread's weight in the queue is its nice
weight scaled by the CPU weights of its resource domains (see *Resource
domains*). On each CPU a domain's time is split between its own threads
and its child domains in proportion to their weights, its own threads
together counting as one child of the default weight 100. `Rq::reweigh`
works the shares out again whenever the fair queue's membership changes,
and `FairQueue::reweight` keeps each entity's lag across the change. The
weights are scaled to sum to 1024 per queued thread. When every queued
thread is in one domain, as is usual, the domain's branch of the tree
cancels out and each thread's weight is its part of their nice weights,
which `reweigh` works out without building the tree. This is a flat
approximation of a hierarchy of queues: it gives the right shares among
the threads that are runnable on one CPU, and the balancer evens the
thread counts between CPUs, but it does not move time between CPUs to
even out domains.

## Switching

A CPU switches only in `reschedule`, for one of four reasons: the running
thread blocks, yields or exits, or something more urgent waits
(`need_resched`, which a timer interrupt or a wake-up sets). With the
thread's lock (for block and exit) and the run queue's lock held, it
updates the thread's state, charges the running thread's time, picks the
next thread, makes it `rq.current`, and lets go of both locks. Only then
does it call the context switch, so **no lock is held across a switch**.

The switch saves the callee-saved registers on the outgoing stack,
stores the stack pointer in the outgoing block, clears its `on_cpu`
word, then spins until it can set the incoming block's `on_cpu` from 0
to 1 (`lock cmpxchg`, `pause`), loads that block's stack pointer and pops
the incoming registers. Interrupts are off throughout, and between
clearing one word and setting the other the switch touches no stack. A
thread that another CPU picks while this CPU is still switching away from
it (it was woken, or pulled by a balancer, meanwhile) is simply waited
for. Before the switch the CPU records an RCU quiescent state and hands
the outgoing thread's held mutexes to the lock checker (`switch_out`);
after it, the incoming thread takes its own back (`switch_in`); with the
checker off, neither does anything. `reschedule` finds the running
thread through its CPU's `current` word, borrowed, rather than cloning
the run queue's reference, which would cost a lock and two atomic counts
per switch. A thread
that exits is put on the reaper's list before its last switch, and the
reaper frees its stack only once its `on_cpu` word is clear, that is once
the switch away from it has saved it. The thread its CPU switches to
wakes the reaper then, so the reaper never looks too early and waits,
and wakes it on that CPU, as Linux frees a task on the CPU it ended on:
waking it where it last ran, an idle CPU as often as not, cost an
interrupt and a halt for every thread that ended, each an exit to the
host in a virtual machine. The reaper looks again after a millisecond
only if a dead thread's lock was held while it looked.

**Preemption.** Spin locks and RCU read sections raise the CPU's
preemption count; nothing else holds off preemption. `need_resched` is
acted on as the interrupt that set it returns, if the count is 0, or when
the last spin lock is let go: the guard restores the interrupt flag
first and then drops the count, so the reschedule it may trigger runs
with interrupts in the state the code expects. A wake-up that should
preempt a thread on another CPU kicks it with a reschedule IPI, or,
when that CPU is idle in MWAIT, with a write to the line it monitors. An
idle CPU says it polls that line only when it waits with MWAIT, or while
it polls `need_resched` in a virtual machine (below); one halted with
HLT always gets the IPI.

**Idle in a virtual machine** (ADR 0037). Under a hypervisor, halting is
an exit to the host and so is the IPI that ends the halt. A virtual CPU
without MWAIT therefore first polls `need_resched`, with interrupts on
and preemption off, and says so in `polling`, so that a waker writes
the flag instead of sending an IPI. How long it polls adapts as Linux's
cpuidle-haltpoll governor does (`radix_sched::haltpoll`): the limit
starts at 0, doubles (from 50 µs, up to 200 µs) when an idle period
outlasted it but ended within 200 µs, and halves when one lasted longer.
A halt runs with preemption off too, so that the interrupt that ends it
leaves the switch to the idle loop, which ends the idle period first: a
switch on the interrupt's way out left the period open while other
threads ran, which counted their time as idle, in `/proc/stat` and in
what the limit learnt.
The waker sets `need_resched` before it reads `polling`; the poller
clears `polling` before it reads `need_resched` again; both are
sequentially consistent, so one of them sees the other. The TLA+ model
`kernel/sched/tla/IdleWake.tla` checks that no wake-up is lost with
polling, MWAIT and HLT, and that either ordering reversed loses one.

**Timers.** Each CPU has a list of its sleeping threads and their
deadlines, with room for every thread made ahead of time. The local APIC
timer runs one-shot (TSC-deadline where available) and is armed for
the earliest of the next sleeper's deadline, the running thread's slice
end and the 100 Hz tick, so sleeps end on time without a fast tick. It is
written only when that deadline is earlier than the one it is armed for
(ADR 0037): a write is an exit under a hypervisor, and a switch usually
leaves the next deadline where it was. A timer armed too early fires,
finds nothing due and is armed again. A deadline in nanoseconds is
turned into the first counter value at which the clock reads it, rounding
up: the clock's scale is rounded down, so scaling by the counter's rate
would land early, by tens of microseconds a day after boot, and the
timer would fire again and again until the clock caught up. Each CPU
counts its timer writes, and `ob bench` reports them per operation.
Under KVM a CPU that answers a TLB shootdown late also has its timer fire
every 250 µs for a while, so that interrupts a host holds for a running
virtual CPU are delivered sooner (ADR 0109); those firings are not ticks.

## Blocking and waking

Blocking and waking meet under the thread's lock:

- `wake(t)` first sets `t.wake_pending`, then takes `t`'s lock. If `t`
  is blocked it clears the flag, makes `t` ready and queues it on the CPU
  that placement picks (`enqueue_ready`), kicking that CPU if `t` should
  preempt what runs there. If `t` is not blocked yet, the flag stays set.
- `block` takes the running thread's lock and swaps `wake_pending` to
  false. If it was set, a wake came since the caller decided to sleep,
  and `block` returns at once. Otherwise it marks the thread blocked,
  takes it off its queue and switches away.

`block` may also return without cause, so every wait loops on its
condition. The general form is `wait::wait_until(key, reason,
condition)`: under the wait table's lock it checks the condition and, if
it does not hold, queues the thread under `key`; it then blocks; then it
takes itself off the table and checks again. Whoever makes the condition
true calls `wake_all(key)` afterwards, which takes the table's lock and
wakes every thread queued under `key`. `wait_until_or_timeout` adds a
deadline, sleeping on the CPU's timer list as well.

The wait table is one vector with room for every thread (grown in
`make_room` before a thread is created, outside every lock), since a
thread waits at most once at a time. It never allocates under its lock.

**Lock order.** The wait table, then the registry or a thread's lock,
then a run queue's lock. A run queue's lock is never held while taking
another run queue's except in the balancer, which takes two in CPU order.

## Placement and balancing

`Topology` (`topology.rs`) describes each CPU by its core, package and
NUMA node, read from the APIC IDs (CPUID's topology leaves) and ACPI's
SRAT and SLIT. Each CPU has four *levels*: the CPUs of its core, of its
package, of its node, and the machine. Each level holds the one below: a
package that spans nodes (AMD's NPS modes, or QEMU's default topology
when it emulates nodes) counts as one package per node, since its
last-level caches do not span nodes either. Levels that add no CPUs are
skipped.

**Placement.** A waking fair thread goes to an idle CPU it may use, as
close as possible to where it last ran (walking the levels outwards),
else back there, else to the least busy CPU it may use
(`select_cpu`). A real-time thread goes to an idle CPU near its last one,
else its last one if that runs something less urgent, else the CPU
running the least urgent thing below its priority (`select_rt_cpu`).
Two cases need none of that and skip it (ADR 0042): a thread allowed on
one CPU goes there if it is online, and a thread whose last CPU is idle
and allowed goes back to it, which is what both functions would choose
first; only otherwise does the waker read every CPU's load.

**Balancing.** Each CPU walks its levels from the closest out
(`sched/balance.rs`). At a level it compares its own group (the level
below, intersected with this one) with the other groups there, using
counts of runnable threads, and pulls one waiting thread from the busiest
group's busiest CPU if `should_pull(from, from_cpus, to, to_cpus)` holds:

```text
(from − 1) · to_cpus ≥ (to + 1) · from_cpus
```

that is, after the move the source group still has at least as much load
per CPU as the destination. A busy CPU tries a level every
`interval_ticks` ticks (1 for the core, 2 for the package, 8 for the
node, 16 for the machine); an idle CPU tries every level before it
sleeps. Only threads waiting in a queue move, never the running one, and
threads that ran within the level's cache window (`is_cache_hot`: 0.5 ms
in a package, 5 ms across nodes) stay unless attempts at that level have
failed several times in a row.

**Affinity.** A thread's affinity is a `CpuSet`. `set_affinity` updates
it under the thread's lock. A queued thread on a CPU it may no longer use
moves at once; a running one is kicked, and `reschedule` moves it as it
is preempted.

## Mutexes and priority inheritance

`radix_sync::Mutex` is a word holding the owner's token (its thread
pointer) and a *waiters* bit. Uncontended, taking and letting go are one
compare-and-swap each. Contended, `mutex_wait` (`sched/wait.rs`) takes
the wait table's lock, sets the waiters bit (taking the mutex instead if
it was let go meanwhile), queues the thread with the priority it lends,
and blocks until it is the owner. With the bit set the owner can let go
only through `mutex_wake`, which needs the table, so the owner seen under
the table's lock stays the owner while the waiter queues.

`mutex_wake` hands the mutex straight to the most urgent waiter (highest
priority, then first come): it writes that waiter's token into the word,
keeping the waiters bit if others remain, makes the new owner the owner
of the remaining waiters, and wakes it. Nobody can take the mutex in
between (no barging).

**Priority inheritance.** A real-time waiter lends its effective
priority to the owner (`lend`). The owner's loan (`boost`) is the
highest priority among the waiters of all mutexes it holds
(`lent_to`), and if the owner itself waits for a mutex, its new
effective priority is lent on along that chain, up to 16 owners. When
the owner lets go, it keeps only what waiters on its other mutexes lend
it, and the new owner inherits the loans of the remaining waiters. A
waiter whose own priority changes (`priority_changed`) recomputes its
owner's loan from scratch. Fair waiters lend nothing: among fair threads
EEVDF bounds every wait for the CPU already, and lending a nice level
would have no clean meaning across domains.

Because a handoff goes to the most urgent waiter, a real-time waiter
always wins over a fair one. The `work/N` workers are therefore fair
(nice −20), not real-time: a real-time worker taking a mutex in a loop
would starve a fair thread waiting for the same mutex (ADR 0027).

## RCU

A read section (`radix_sync::rcu::read_lock`) raises the preemption count
and nothing else, so it costs two per-CPU increments. A CPU that switches
threads, goes idle, or takes a tick that interrupted code with a
preemption count of 0 (and not another interrupt handler) cannot be
inside a read section that began earlier: it has passed a *quiescent
state* (`sched/rcu.rs`).

A grace period starts with the set of CPUs that run threads and ends when
each has passed a quiescent state; CPUs that stop running threads drop
out. `synchronize` waits for the grace period after the one under way, if
one is: a reader that began after the running one started might still be
inside. Grace periods run one at a time and are shared, so many callers
cost one grace period. With one CPU, `synchronize` returns at once: a
thread that can call it is not in a read section, and no other CPU runs
one. Where sleeping is not allowed it also returns at once, since there
is nothing safe to do; such callers are bugs.

## Per-CPU data

`PerCpu<T>` (`percpu.rs`) holds one value per CPU, each on its own cache
line. `PerCpu::this` gives the current CPU's value to code that cannot
move meanwhile (interrupts off, a spin lock held or preemption held
off), and `PerCpu::with_own` holds preemption off itself. Values can also
be reached by index, which the scheduler uses to look at and lock other
CPUs' run queues. The running thread is found another way: each switch
also writes it to a word of the architecture's per-CPU block (`gs:[40]`
on x86-64), which one instruction reads, so finding it, as every system
call does, holds nothing off; whatever CPU the code is on when it reads
the word, the thread running there is its own (ADR 0042).

## The lock checker

`radix-lockdep` is a lock-order checker after Linux's lockdep. Every spin
lock and mutex acquisition goes through `radix_sync`'s hooks to the
checker when it is on. When it is off, the kernel tells `radix_sync`
(`set_checking`), and the locks skip those two calls altogether: a
same-CPU round trip takes some twenty locks, and the calls were a sixth
of its instructions (ADR 0040). It keeps a graph over lock *classes* (every lock
created at one source location, plus the subclass `lock_nested` names)
with an edge `A → B` when `B` was taken while `A` was held, in the same
context. Taking a lock whose class would close a cycle is reported at
once, with the places both orders were seen. It also reports recursion
within a class, a class taken in an interrupt handler and elsewhere with
interrupts on (directly or through a chain), and sleeping where it is not
allowed (a mutex or a switch with a spin lock held, interrupts off, or in
an interrupt handler).

Each CPU has a stack of held locks. Spin locks never outlive a thread's
time on a CPU; mutexes do, so the scheduler moves them into the thread at
`switch_out` and back at `switch_in`. Everything is in fixed tables (512
classes, 32 held locks per CPU, 8 saved mutexes per thread). The first
report, or running out of room, turns the checker off: after one report
the graph may be inconsistent, and follow-on reports would hide the first.
It is on by default in debug builds and off in release builds; the
`lockdep` flag overrides either (ADR 0028). The boot tests run every
Phase 4 case and `bios-usb` with it on, and fail on any report except in
the `lockdep` case, which seeds them.

## Resource domains

A resource domain (`domains.rs`) is a node in a tree under the root
domain `system`, which holds the kernel's threads. It has a CPU weight (1
to 10,000, default 100), an optional memory limit in pages, an
out-of-memory class (`app`, `service` or `system`; never more protected
than its parent's), an optional supervisor, and counters: pages charged
and their peak, refused charges, threads and out-of-memory stops. Domains
are kernel objects of kind `ResourceDomain`: `caps::create_domain` makes
one together with its first capability, `caps::domain_at` finds the
domain a capability names, and deleting the last capability takes the
domain off the table; it is freed once its last thread and reference let
go (ADR 0029).

Charges go up the tree: a charge that would pass the limit of the domain
or of any domain above it is refused, and the caller's allocation fails
with an error. A thread's stack is charged to its domain (and a spawn
over the limit fails with `OverLimit`), and since Phase 6 so are the
kernel objects a program makes with `process.create`
(`docs/design/processes.md`). A thread counts in its domain until it exits, not until its
last reference goes, so "the domain has no threads" means its threads
have stopped running.

## The out-of-memory policy

When the frame allocator cannot satisfy an allocation of up to 2³ pages
that may sleep, even after the heap has handed back its caches, it calls
the policy (`oom.rs`; `Frames::set_oom`). The policy runs on the thread
that asked:

1. If it may not sleep, or it is being stopped itself, the allocation
   fails.
2. It takes the policy mutex. If another thread stopped a domain while
   this one waited, it returns to retry the allocation.
3. It chooses the domain to stop: the lowest class below `system` (apps,
   then services), then the most memory charged, with the domains below
   it, skipping domains being stopped and those without threads of their
   own (such as `services`, whose threads are all in its children, which
   are chosen one at a time). If there is none, it logs an error and the
   allocation fails.
4. It marks that domain and every domain below it as stopping, asks
   their threads to stop (`request_stop`) and wakes them, logs a warning
   naming the domain, its class, its usage and its thread count, and
   queues a restart if the domain has a supervisor.
5. If the asking thread is in that domain, its allocation fails at once,
   so it can unwind. Otherwise it waits up to a second for the domain's
   threads to exit (`wait_until_or_timeout` on the domain's key, which
   the last thread's exit wakes), then the allocator retries.

The `supervisor` thread waits for queued restarts, waits up to five
seconds for each domain to empty, clears the stopping marks and calls the
domain's supervisor function. Spark takes this over for the services it
runs in Phase 9, when it starts supervising them.

Stopping is cooperative: kernel threads check `should_stop` in their
loops, the wake gets them out of whatever they wait for, and timed sleeps
return early once it is set. User threads stop at their next entry into
the kernel or return from an interrupt, which needs no cooperation from
them, and since Phase 6 their process ends with them.

## Why the scheduler is correct

This is the semi-formal argument the roadmap asks for. Each step names
the check that backs it (see *Verification*).

**1. A thread runs on at most one CPU at a time.** A CPU executes on a
thread's stack only between taking its `on_cpu` word (0 to 1 by
compare-and-swap) and clearing it, and the switch touches no stack
between clearing the outgoing word and taking the incoming one. So two
CPUs on one stack would both have taken the same word without either
clearing it, which the compare-and-swap excludes. *(TLA+:
`OneStackUser` and `StackUserHoldsContext`; the `fpu` self-test, where a
thread on two CPUs at once would corrupt its registers.)*

**2. Switches cannot deadlock.** No lock is held across a switch. The
only waiting inside one is for the incoming `on_cpu` word, which the
holder clears in the first part of its own switch, before it waits for
anything. So of two CPUs switching towards each other's threads, each
lets go of its outgoing context before waiting, and both proceed. A
thread picked while another CPU switches away from it is waited for only
as long as that switch's first half. *(TLA+: the liveness property
`AllFinish` under weak fairness of the switch steps.)*

**3. The queues are consistent.** A blocked thread is on no run queue; a
ready one is on exactly one; a CPU's current thread is on its own queue.
Every change of a thread's state happens under its lock together with the
run-queue change that matches it (`reschedule`, `enqueue_ready`), the
balancer moves only threads that are not current, and a thread that may
no longer stay on its CPU is moved under its own lock after the switch.
*(TLA+: `QueuesConsistent`; the queues' `check()` audits in the host
tests and the `sched-fair` self-test.)*

**4. No wake-up is lost.** Suppose thread `t` waits for condition `C`
under key `k`, and a waker makes `C` true and calls `wake_all(k)`. The
waiter checks `C` and queues itself under the table's lock; the waker
takes the same lock after making `C` true. Either the waiter's check
comes after the waker's change, and it sees `C` and does not block, or it
comes before, and `t` is in the table when the waker takes the lock and
is woken. That wake sets `wake_pending` before taking `t`'s lock, and
`block` swaps the flag under `t`'s lock: if the wake's lock section comes
first, `t` is not blocked yet, so the flag stays set and `block` returns;
if `block`'s comes first, `t` is blocked and the wake makes it ready. In
both orders `t` ends up ready, and back in its loop it re-checks `C`.
*(TLA+: `WakeOnTheWay`, an invariant saying every blocked thread whose
condition holds is still in the table or in the waker's hands, and
`AllFinish`. The model fails both when `block` ignores the flag and when
the wake does not set it.)*

**5. The fair queue always has a pick.** For a non-empty queue let `m`
be the entity with the smallest `v`, so `v₀ = v_m` after `rebase`. Then
`(v_m − v₀) · W = 0 ≤ Σ w (v − v₀) = S`, since every term is a positive
weight times a non-negative difference. So `m` is eligible and `pick`
returns some entity. The arithmetic is exact: differences fit `i64`
because lags are clamped and slices are short, and their products with
`W` fit `i128`. *(Host tests: `the_queue_stays_consistent`, a property
test that runs random enqueues, dequeues, charges and reweights and
asserts after every step that the books are exact and some entity is
eligible, now with yields among the steps. The product of symbolic
64-bit values is beyond Kani in reasonable time, so this step is algebra
plus tests; see ADR 0030.)*

**6. Fair threads get their share.** Stoica and Abdel-Wahab show that
under EEVDF, for entities that stay runnable, every entity's lag stays
within a bound set by the slices in the queue (the largest request);
clamping the lag on leaving (`lag_limit`) keeps it bounded across sleeps.
So over any interval an entity's service differs from its weighted share
by a bounded number of slices. Domain weights only change the weights
the queue sees, and reweighting keeps `V` and each lag, so the bound
holds through changes too. *(Host tests: `service_follows_the_weights`,
a property test over random nice levels with an 8-slice tolerance, and
`reweighting_keeps_the_average`; the `sched-fair` self-test measures
shares on real hardware or QEMU within 20 %.)*

**7. The most urgent real-time thread runs.** `RtQueue::pick` returns
the highest priority queued, first come among equals *(Kani:
`real_time_picks_the_most_urgent_first_comer`; host test
`picks_follow_priority_then_arrival`)*, and real-time threads are picked
before fair ones. A wake-up that makes a more urgent thread ready sets
`need_resched` on the target CPU and kicks it, and the switch happens as
soon as that CPU's preemption count is 0; spin-locked sections are short
and bounded, so the delay is bounded too. A sleeping real-time thread's
timer fires on time because the timer is re-armed for the earliest
deadline. *(The `sched-rt` self-test: wake-ups preempt by priority in
every round, and a timer wake on a busy CPU is late by under 1 ms in the
median and 3 ms at the 90th percentile.)*

**8. Priority inversion is bounded.** Let `H` wait for a mutex owned by
`L` while `M` has a priority between theirs. Under the table's lock `H`
lends its priority to `L` and to every owner along the chain `L` waits
on (up to 16), so while `H` waits, `L` and each owner on the chain run at
least at `H`'s priority, above `M`, and `M` cannot preempt them. When `L`
lets go, the mutex goes straight to the most urgent waiter, `H` (or one
more urgent), with no window for another thread to take it. `H`
therefore waits only for the critical sections along the chain, as if
`M` did not exist. Loans are recomputed from the waiters actually queued
at each handoff and each priority change, so they never outlive their
reason. *(The `sched-pi` self-test, with one mutex and with a chain of
two; Kani `real_time_picks_the_most_urgent_first_comer` for the boosted
pick; host test `boosting_moves_an_entry_up`.)*

**9. Balancing settles.** A pull needs `(f − 1) · t_c ≥ (t + 1) · f_c`
for source load `f` on `f_c` CPUs and destination load `t` on `t_c`. The
reverse pull right after would need `(t + 1 − 1) · f_c ≥ (f − 1 + 1) ·
t_c`, that is `t · f_c ≥ f · t_c`; but the first inequality gives `f ·
t_c ≥ t · f_c + f_c + t_c > t · f_c`. So no pull is ever undone by the
next one. Moreover each pull strictly lowers `Φ = Σ_groups load² /
cpus`: its change is `(1 − 2f) / f_c + (2t + 1) / t_c`, and from the
condition `(2t + 1) / t_c < 2 (t + 1) / t_c ≤ 2 (f − 1) / f_c < (2f −
1) / f_c`. `Φ` is a non-negative quantity on a finite set of values, so
without new arrivals balancing stops after finitely many pulls.
*(Host tests: `pulling_never_reverses_the_imbalance`, exhaustive for loads
up to 48 and groups up to 12 CPUs, and the property test
`pulling_evens_without_reversing`; the `sched-balance` self-test spreads
eight threads from one CPU to 2/2/2/2 over two nodes.)*

**10. The topology levels nest.** Placement and balancing assume each
level's span contains the one below. A package that spans nodes would
break that, so `span(Package)` is "same package and same node", which is
a subset of "same node". *(Host test: `levels_grow_outwards`, including a
package split across nodes.)*

**11. RCU readers are safe.** Let a reader begin a read section before a
grace period `G` starts. It holds the preemption count above 0 until it
ends, so its CPU cannot switch, idle, or count a tick as quiescent until
then; `G` waits for that CPU, so `G` ends after the reader. `synchronize`
waits for a grace period that starts after it was called, so every reader
that could have seen the old pointer has finished before the writer
frees it. *(The `rcu` self-test: readers on three CPUs never see an
object freed too early over 100 replacements.)*

**12. The lock checker finds every cycle it has seen.** The graph has an
edge for every pair of classes taken nested in one context. A deadlock
between locks needs a cycle of such nestings among the threads involved;
each nesting was taken at least once to be possible, so the checker has
seen all its edges by the time the last one is taken, and it checks for
a cycle at every new edge. It reports orders that can deadlock whether or
not they ever race. It cannot see orders that no run has exercised.
*(The `lockdep` self-test seeds an AB-BA deadlock between spin locks,
one between mutexes, and a mutex under a spin lock, each half on its own
thread, and checks each is reported; the host tests of `radix-lockdep`
drive the checker directly.)*

**13. The out-of-memory policy never stops the system and never
panics.** `choose` skips `system` domains, and `stop` acts only on the
chosen domain and those below it, whose classes are no more protected
(`system` cannot sit below `app` or `service`). If nothing can be
chosen, the allocation fails and its caller handles the error; the
policy has no panic path. Every stop is logged. It terminates: one
thread runs it at a time, a stopped domain is skipped by later choices,
the wait is bounded by a second, and a thread being stopped returns at
once. *(The `oom` self-test: an app is stopped before a service, a
service is stopped only once no app is left and is restarted by its
supervisor, no `system` thread is asked to stop, and the memory comes
back.)*

## Verification

| Check | What it establishes | Where |
| --- | --- | --- |
| TLA+ `Scheduler.tla` | Steps 1 to 4: one CPU per stack, no deadlocked switch, consistent queues, no lost wake-up, every wait ends | `ob test tla` |
| Kani `nice_levels_are_a_quarter_apart` | Each nice level weighs 1.25 times the next, within rounding; weights fall strictly | `ob test kani` |
| Kani `cpu_sets_insert_and_remove_one_cpu` | Inserting or removing a CPU changes exactly that CPU | `ob test kani` |
| Kani `real_time_picks_the_most_urgent_first_comer` | Step 7: the real-time pick | `ob test kani` |
| Property `the_queue_stays_consistent` | Step 5: exact books, always an eligible entity | `ob test unit` |
| Property `service_follows_the_weights` | Step 6: service within 8 slices of the weighted share | `ob test unit` |
| Property `picks_follow_priority_then_arrival` | Step 7 for queues of up to 40 | `ob test unit` |
| Exhaustive `pulling_never_reverses_the_imbalance`, property `pulling_evens_without_reversing` | Step 9 | `ob test unit` |
| Property `placement_respects_affinity` | A woken thread goes only to a CPU it may use | `ob test unit` |
| Host tests of `radix-lockdep` | Step 12: cycles, recursion, interrupt safety, sleeping in atomic context | `ob test unit` |
| Boot tests `fpu`, `sched-fair`, `sched-rt`, `sched-pi`, `sched-balance`, `lockdep`, `rcu`, `oom` | Each step on 4 CPUs, under QEMU with KVM in CI | `ob test boot` |

**The TLA+ model.** `kernel/sched/tla/Scheduler.tla` models threads
that each wait for a condition several times, a waker that makes the
conditions true and calls `wake_all`, and CPUs that preempt to any
queued thread, pull any queued thread that is not current to another
CPU, and switch in two steps (let go of the outgoing context, then take
the incoming one). The mapping to the code:

| Model | Code |
| --- | --- |
| `Check`, `After` | `wait_until`'s two sections under the table's lock |
| `Block` | `reschedule(Block)`: the `wake_pending` swap, then dequeue and pick, under the thread's and the queue's lock |
| `Signal`, `Take`, `NextWaiter` | making the condition true, then `wake_all` taking the table and its waiters |
| `Pend`, `Wake` | `wake`: setting `wake_pending`, then the section under the thread's lock |
| `Preempt` | `reschedule(Preempt)` from a tick, a kick or an idle CPU finding work |
| `Release`, `Acquire` | the two halves of the context switch around `on_cpu` |
| `Pull` | `balance` and `enqueue_ready` moving a queued thread |

Each action is atomic in the model because the code it stands for runs
under the lock that serializes it with every action it races: the
table's lock for `Check`, `After` and `Take`; a thread's lock for `Block`
and `Wake`. The model leaves out what has no bearing on these
properties: which thread a CPU picks (any queued one may be), weights,
priorities and timers. `Scheduler.cfg` checks the invariants with three
threads, two CPUs and two waits each under symmetry (about 5.4 million
distinct states, two minutes on 4 CPUs); `SchedulerLiveness.cfg` checks
`AllFinish` as well with two threads, without symmetry (about 180,000
states). The fairness assumptions are those of a working scheduler: a
thread that keeps being able to run its code, or keeps being pickable,
eventually does, and a started switch or wake finishes. Seeding a bug
breaks the right property: letting `block` ignore `wake_pending`, or the
waker skip setting it, breaks `WakeOnTheWay`; letting a CPU take a
context without waiting breaks `OneStackUser`; letting the balancer move
a running thread breaks `QueuesConsistent`; and without fairness in
picking, `AllFinish` fails.

## Limits

- 256 CPUs (`MAX_CPUS`), 256 resource domains at most 4 levels below
  `system`, priority chains of 16, 8 pending restarts.
- Queues are vectors scanned on every pick. That is fine for the short
  queues of Phase 4; a tree replaces the scan when queues grow long.
- Domain weights apply per CPU (see *Run queues and classes*); a domain
  with threads on fewer CPUs than another is not compensated across CPUs.
- The timing checks of `sched-rt` assume KVM or real hardware. Under TCG
  they pass on an idle machine with room to spare, but a loaded host can
  delay a virtual CPU past them.
- MWAIT idle is used where CPUID offers it and works. QEMU does not offer
  it by default, so the boot tests exercise HLT; the first boot on real
  hardware is its first run.
- Stopping is cooperative for kernel threads. A program's thread (Phase
  5) stops on its next way back to user mode, at the latest after the
  next tick, or when a wait it is in ends.
