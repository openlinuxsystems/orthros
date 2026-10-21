radix-scheduling(7) "OrthrOS" "OrthrOS Manual"

; SPDX-FileCopyrightText: 2026 The OrthrOS contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-scheduling - threads, scheduling classes, priority inheritance,
RCU, resource domains and the out-of-memory policy in the Radix kernel

# DESCRIPTION

Radix runs threads on every CPU. Real-time threads run first, by
priority; the rest share the CPUs fairly by weight. Threads are grouped
into resource domains, which share a CPU weight and a memory limit, and
when the machine runs out of memory the kernel stops whole domains, apps
first, and never the system's own.

This page describes the scheduler as of Phase 5 of the roadmap. Most
threads are the kernel's own and those of the self-tests (see
*radix-debug*(7)); from Phase 5 a thread can also run a program in user
mode, which gives up the CPU with *radix_yield*(2) and blocks in IPC (see
*radix-ipc*(7)). System calls for priorities and domains arrive with
processes in Phase 6. The design, with the argument that the scheduler is
correct, is in _docs/design/scheduler.md_.

# THREADS

A thread has a name of up to 23 bytes, a stack of 16 KiB with an
unmapped guard page below it, a save area for its extended processor
state (x87, SSE, AVX and, where the CPU has it, AVX-512), a scheduling
policy, the set of CPUs it may run on, and a resource domain. It is
*ready*, *blocked* (with the reason it waits), or *dead* once it has
exited; the *reaper* thread frees a dead thread's stack.

Every switch saves the outgoing thread's extended state and restores the
incoming one's (eager switching), with XSAVEOPT where the CPU has it. The
kernel itself never uses those registers.

The kernel's own threads are:

*kmain*
	The boot code, from the moment the scheduler starts. It exits at
	the end of boot, or waits in *kdb* with the *kdb* flag.

*idle/*_N_
	Runs on CPU _N_ when nothing else can. It waits with MWAIT where
	that works, so a waker only has to write to the CPU's run queue,
	and with HLT otherwise. It never appears in a run queue.

*work/*_N_
	Runs functions that other kernel code posts for every CPU.

*reaper*
	Frees the stacks of threads that have exited.

*supervisor*
	Restarts domains that the out-of-memory policy stopped (see *OUT OF
	MEMORY*).

# SCHEDULING CLASSES

Each CPU has one run queue. It runs the most urgent real-time thread if
there is one, else the fair thread the fair class picks, else its idle
thread.

*fair*, nice _-20_ to _19_
	The default class: EEVDF (earliest eligible virtual deadline
	first). A thread's share of a CPU is its weight over the weights
	of the threads it competes with; nice 0 weighs 1024 and each nice
	level weighs 1.25 times the next, so one level is worth about 10 %
	of a CPU between two otherwise equal threads. A thread runs for a
	slice of 3 ms before the next eligible thread with an earlier
	deadline takes over. A thread that sleeps keeps the service it was
	owed or had in advance (its lag, within about two slices), so
	sleeping neither earns credit nor costs any.

*FIFO*, priority _1_ to _99_
	Runs ahead of every fair thread. The highest priority runs; among
	equals the one that has waited longest, which keeps the CPU until
	it blocks, yields or a higher priority wakes up.

*round robin*, priority _1_ to _99_
	As FIFO, but after each quantum of 100 ms a thread goes to the back
	of its priority.

A thread preempted by a higher priority keeps its place at the front.
A policy can be changed while the thread runs; a fair thread keeps its
lag through the change.

# PREEMPTION

A CPU switches threads when the running one blocks, yields or exits, and
when something more urgent waits: a woken thread of higher priority, a
fair thread with an earlier deadline, or the end of a slice. The timer is
re-armed for the next of these, a sleeper's deadline and the tick
(100 Hz), so sleeps end on time without a faster tick. The switch happens
as the interrupt that asked for it returns, or as the last spin lock is
let go: spin locks and RCU read sections hold off preemption, nothing
else does.

# PLACEMENT, BALANCING AND AFFINITY

The kernel reads the CPUs' cores, packages and NUMA nodes from CPUID and
ACPI (MADT, SRAT, SLIT). Each CPU sees four *levels* around it: the
hardware threads of its core, its package (which shares the last-level
cache), its node and the whole machine. A package that spans nodes counts
as one package per node.

A woken thread goes to an idle CPU as close as possible to where it last
ran, else back there, else to the least busy CPU it may use. A real-time
thread goes to an idle CPU, else one running something less urgent.

Each CPU balances its load against the others level by level: every tick
for its core, less often for farther levels, and at every level whenever
it is about to go idle. It pulls a waiting thread from the busiest group
at a level only if that makes the load more even without reversing it, so
two CPUs never pass a thread back and forth, and it leaves threads whose
caches are probably still warm unless several attempts have failed.
Moves between nodes are tried least often.

A thread's affinity is the set of CPUs it may run on, all of them by
default. Changing it moves the thread at once if it is on a CPU it may no
longer use.

# WAITING AND MUTEXES

Spin locks are for short sections and hold off preemption (and, where an
interrupt handler shares the lock, interrupts). A mutex sleeps while
another thread holds it; it may not be taken with a spin lock held, with
interrupts off or in an interrupt handler.

A mutex's waiters are queued most urgent first, and letting go of the
mutex hands it straight to the first, so no thread can take it in
between. A real-time thread waiting for a mutex lends its priority to the
owner, and along the chain of mutexes the owner itself waits for, until
the owner lets go (priority inheritance). A low-priority thread holding
a lock that a high-priority one needs therefore runs at the high
priority, and a thread of middle priority cannot keep both waiting.
Fair waiters lend nothing; the fair class already bounds how long a fair
thread waits for the CPU.

The lock checker (see *radix-debug*(7) and the *lockdep* kernel flag in
*orthros-boot*(7)) reports locks taken in orders that can deadlock, the
moment the code runs.

# RCU

Read-copy-update lets readers follow pointers to shared data with no lock
at all. A read section only holds off preemption. A writer replaces an
object, then waits for a *grace period* before freeing the old one: every
CPU that runs threads must pass a quiescent state (switch threads, go
idle, or take a tick outside a read section). Read sections that began
before the grace period have then ended. Many writers waiting at once
share one grace period.

# RESOURCE DOMAINS

Resource domains group threads, after Linux's cgroups (v2). They form a
tree under the root domain, *system*, which holds the kernel's own
threads. Each domain is a kernel object that a capability names (kind
*domain*, see *radix-capabilities*(7)); deleting the last
capability to it removes it once its threads are gone. A domain has:

*CPU weight*, _1_ to _10000_ (default _100_)
	On each CPU, the fair class splits a domain's time between its own
	threads and its child domains in proportion to their weights, the
	domain's own threads together counting as one child of weight 100.
	Two domains of weights 200 and 100 with threads ready on one CPU
	get two thirds and one third of its fair time however many threads
	each has.

*memory limit*, in pages (default none)
	Charges to the domain and those below it may not pass it. A charge
	that would is refused, and the allocation fails with an error: a
	thread that cannot get its stack is not created. Nothing is killed.
	Today a thread's stack is charged; from Phase 6 a process's memory
	is.

*out-of-memory class*: *app*, *service* or *system*
	Which domains the out-of-memory policy may stop, and in which order.
	A domain's class cannot be more protected than its parent's.

*supervisor*
	Optionally, what restarts the domain after an out-of-memory stop.
	Spark, the service manager, takes this over in Phase 6; until then
	a kernel stand-in runs it on the *supervisor* thread.

# OUT OF MEMORY

When the frame allocator cannot find memory for an allocation that may
sleep, even after the heap has given back what it caches, the kernel
stops a whole domain:

. It picks the domain: *app* domains before *service* domains, and in a
  class the one using the most memory. *system* domains, which hold the
  trusted computing base, are never stopped.
. It asks every thread of that domain and the domains below it to stop,
  wakes them, and logs a warning:
  *Out of memory: stopped domain `name` (app class, N pages in use, K
  threads)*.
. If the thread that asked for memory is one of them, its allocation
  fails at once so it can unwind. Otherwise the kernel waits up to a
  second for the domain's threads to exit, then tries the allocation
  again.
. Once the threads are gone, a domain with a supervisor is restarted:
  *Out of memory: restarting domain `name`*.

If only *system* domains are left, the allocation fails, its caller gets
an error, and the kernel logs *Out of memory: no app or service domain is
left to stop*. The kernel never kills a thread without saying so, and
never panics only because memory ran out. Allocations that cannot sleep
(in interrupt handlers or under spin locks) fail at once without stopping
anything.

# LIMITS

- 256 CPUs.
- 256 resource domains, at most 4 levels below *system*.
- 32 locks held at once per CPU, and 512 lock classes, before the lock
  checker turns itself off.
- 16 mutexes along a chain that priority is lent through.
- 8 restarts waiting for the supervisor at once.

# DIAGNOSTICS

At boot the kernel logs which classes run, the timer, how idle CPUs wait
and which extended state every switch saves (*Scheduler:*), whether the
lock checker runs (*Lock checker:*), and how many CPUs run threads on how
many NUMA nodes. *kdb*'s *threads*, *sched*, *domains* and *locks*
commands show the threads, the run queues, the domains and the lock
checker at any time (see *radix-debug*(7)).

# SEE ALSO

*radix-ipc*(7), *radix-debug*(7), *radix-capabilities*(7), *orthros-boot*(7), *ob*(1)
