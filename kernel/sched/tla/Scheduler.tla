----------------------------- MODULE Scheduler -----------------------------
(***************************************************************************)
(* A model of how the Radix kernel blocks, wakes and switches threads     *)
(* (kernel/radix/src/sched: mod.rs, wait.rs; the switch is                 *)
(* kernel/arch/x86_64/src/thread.rs).                                      *)
(*                                                                         *)
(* Threads wait for a condition the way `wait::wait_until` does: under the *)
(* wait table's lock they check it and, if it does not hold, queue         *)
(* themselves; then they block; then they leave the table and check        *)
(* again. A waker makes a thread's condition true, then, holding the       *)
(* table's lock, takes every waiter off it and wakes each one: it sets the *)
(* thread's `wake_pending` flag and, under the thread's lock, makes a      *)
(* blocked thread ready on some CPU's run queue. Blocking, under the       *)
(* thread's lock and its run queue's, sleeps only if no wake came since    *)
(* the thread queued itself.                                               *)
(*                                                                         *)
(* No lock is held across a switch. A CPU that switches picks the next     *)
(* thread under its run queue's lock, then lets go of the outgoing         *)
(* thread's context (its `on_cpu` word) and only then takes the incoming   *)
(* one's, waiting while another CPU still holds it. Meanwhile other CPUs   *)
(* may wake the outgoing thread, pull it to their queue and pick it.       *)
(* CPUs preempt and balancers pull at any moment, which covers every       *)
(* order in which timer ticks, wake-up preemption and idle CPUs act.       *)
(*                                                                         *)
(* The invariants say that a thread's stack is in use by one CPU at most   *)
(* (OneStackUser), that a blocked thread is on no run queue and a ready    *)
(* one on exactly one (QueuesConsistent), and that a thread whose          *)
(* condition holds is never left blocked with no wake on its way           *)
(* (WakeOnTheWay). The liveness property AllFinish says every wait ends,   *)
(* given a fair scheduler. docs/design/scheduler.md maps the model to the *)
(* code.                                                                   *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Threads,    \* the threads that wait
    Cpus,       \* the CPUs
    Rounds,     \* how many waits each thread does
    None        \* no switch under way

ASSUME Rounds \in Nat \ {0}

\* Each CPU's idle thread: it never moves and has no `on_cpu` word to share.
Idle(c) == <<"idle", c>>
Idles == {Idle(c) : c \in Cpus}
Runnable == Threads \cup Idles

\* A switch under way on a CPU: from `prev` to `next`, and whether `prev`'s
\* context has been let go of.
Switches == [prev : Runnable, next : Runnable, released : BOOLEAN]

VARIABLES
    run,        \* each thread's state under its lock: "ready" or "blocked"
    pending,    \* wake_pending
    rq,         \* each CPU's run queue (the running thread included)
    cur,        \* rq.current: what each CPU runs, or is switching to
    sw,         \* the switch under way on each CPU, or None
    oncpu,      \* each thread's `on_cpu` word
    pc,         \* where each thread is in its wait loop
    round,      \* which of its waits each thread is in
    waiters,    \* the wait table
    signals,    \* how many of each thread's waits the waker has ended
    table,      \* who holds the wait table's lock: a thread, "waker" or None
    waker       \* the waker's state

vars == <<run, pending, rq, cur, sw, oncpu, pc, round, waiters, signals, table, waker>>

Wakers == [phase : {"signal", "take", "next", "pend", "lock", "done"},
           todo : SUBSET Threads, t : Threads \cup {None}]

TypeOK ==
    /\ run \in [Threads -> {"ready", "blocked"}]
    /\ pending \in [Threads -> BOOLEAN]
    /\ rq \in [Cpus -> SUBSET Threads]
    /\ cur \in [Cpus -> Runnable]
    /\ sw \in [Cpus -> Switches \cup {None}]
    /\ oncpu \in [Threads -> BOOLEAN]
    /\ pc \in [Threads -> {"check", "block", "after", "done"}]
    /\ round \in [Threads -> 1..Rounds]
    /\ waiters \subseteq Threads
    /\ signals \in [Threads -> 0..Rounds]
    /\ table \in Threads \cup {"waker", None}
    /\ waker \in Wakers

\* CPU `c` runs `t`'s code: switched to it, or not yet off its stack.
OnStack(c, t) ==
    \/ cur[c] = t /\ sw[c] = None
    \/ sw[c] # None /\ sw[c].prev = t /\ ~sw[c].released

\* `t` runs its own code on CPU `c`, between switches.
RunsOn(c, t) == cur[c] = t /\ sw[c] = None

\* The thread's condition: the waker has ended its current wait.
Condition(t) == signals[t] >= round[t]

\* At first each thread is ready on the first CPU's queue (any order of
\* start-up reaches the states balancing then reaches) and nothing runs.
Init ==
    /\ run = [t \in Threads |-> "ready"]
    /\ pending = [t \in Threads |-> FALSE]
    /\ rq = [c \in Cpus |-> IF c = CHOOSE x \in Cpus : TRUE THEN Threads ELSE {}]
    /\ cur = [c \in Cpus |-> Idle(c)]
    /\ sw = [c \in Cpus |-> None]
    /\ oncpu = [t \in Threads |-> FALSE]
    /\ pc = [t \in Threads |-> "check"]
    /\ round = [t \in Threads |-> 1]
    /\ waiters = {}
    /\ signals = [t \in Threads |-> 0]
    /\ table = None
    /\ waker = [phase |-> "signal", todo |-> {}, t |-> None]

----------------------------------------------------------------------------
(* The waiting threads (wait_until).                                       *)

\* Under the table's lock: done if the condition holds, else queued.
Check(c, t) ==
    /\ RunsOn(c, t) /\ pc[t] = "check" /\ table = None
    /\ IF Condition(t)
       THEN /\ pc' = [pc EXCEPT ![t] = IF round[t] = Rounds THEN "done" ELSE "check"]
            /\ round' = [round EXCEPT ![t] = IF @ = Rounds THEN @ ELSE @ + 1]
            /\ UNCHANGED waiters
       ELSE /\ waiters' = waiters \cup {t}
            /\ pc' = [pc EXCEPT ![t] = "block"]
            /\ UNCHANGED round
    /\ UNCHANGED <<run, pending, rq, cur, sw, oncpu, signals, table, waker>>

\* reschedule(Block), under the thread's lock and its CPU's queue lock:
\* carries on if a wake came, else sleeps and the CPU switches away.
Block(c, t) ==
    /\ RunsOn(c, t) /\ pc[t] = "block"
    /\ pc' = [pc EXCEPT ![t] = "after"]
    /\ IF pending[t]
       THEN /\ pending' = [pending EXCEPT ![t] = FALSE]
            /\ UNCHANGED <<run, rq, cur, sw>>
       ELSE /\ run' = [run EXCEPT ![t] = "blocked"]
            /\ rq' = [rq EXCEPT ![c] = @ \ {t}]
            /\ \E next \in rq'[c] \cup {Idle(c)} :
                  /\ cur' = [cur EXCEPT ![c] = next]
                  /\ sw' = [sw EXCEPT ![c] = [prev |-> t, next |-> next, released |-> FALSE]]
            /\ UNCHANGED pending
    /\ UNCHANGED <<oncpu, round, waiters, signals, table, waker>>

\* Back from blocking: leaves the table, to check again.
After(c, t) ==
    /\ RunsOn(c, t) /\ pc[t] = "after" /\ table = None
    /\ waiters' = waiters \ {t}
    /\ pc' = [pc EXCEPT ![t] = "check"]
    /\ UNCHANGED <<run, pending, rq, cur, sw, oncpu, round, signals, table, waker>>

----------------------------------------------------------------------------
(* The CPUs.                                                               *)

\* Something else on the queue should run (a tick, a wake-up, an idle CPU
\* finding work): the CPU switches to it. The outgoing thread stays ready.
Preempt(c, next) ==
    /\ sw[c] = None /\ next \in rq[c] /\ next # cur[c]
    /\ cur' = [cur EXCEPT ![c] = next]
    /\ sw' = [sw EXCEPT ![c] = [prev |-> cur[c], next |-> next, released |-> FALSE]]
    /\ UNCHANGED <<run, pending, rq, oncpu, pc, round, waiters, signals, table, waker>>

\* The switch saves the outgoing context and lets go of it.
Release(c) ==
    /\ sw[c] # None /\ ~sw[c].released
    /\ oncpu' = IF sw[c].prev \in Threads THEN [oncpu EXCEPT ![sw[c].prev] = FALSE] ELSE oncpu
    /\ sw' = [sw EXCEPT ![c].released = TRUE]
    /\ UNCHANGED <<run, pending, rq, cur, pc, round, waiters, signals, table, waker>>

\* Then takes the incoming context, once no other CPU holds it.
Acquire(c) ==
    /\ sw[c] # None /\ sw[c].released
    /\ sw[c].next \in Threads => ~oncpu[sw[c].next]
    /\ oncpu' = IF sw[c].next \in Threads THEN [oncpu EXCEPT ![sw[c].next] = TRUE] ELSE oncpu
    /\ sw' = [sw EXCEPT ![c] = None]
    /\ UNCHANGED <<run, pending, rq, cur, pc, round, waiters, signals, table, waker>>

\* A balancer on `to` pulls a ready thread that `from` is not running, with
\* both queues locked: perhaps one `from` is still switching away from.
Pull(from, to, t) ==
    /\ from # to /\ t \in rq[from] /\ t # cur[from]
    /\ rq' = [rq EXCEPT ![from] = @ \ {t}, ![to] = @ \cup {t}]
    /\ UNCHANGED <<run, pending, cur, sw, oncpu, pc, round, waiters, signals, table, waker>>

----------------------------------------------------------------------------
(* The waker: makes a condition true, then wake_all.                       *)

Signal(t) ==
    /\ waker.phase = "signal" /\ signals[t] < Rounds
    \* It ends a wait only once the thread is in it, so each signal ends
    \* one wait (a later one would otherwise find its condition true).
    /\ signals[t] < round[t] /\ pc[t] # "done"
    /\ signals' = [signals EXCEPT ![t] = @ + 1]
    /\ waker' = [waker EXCEPT !.phase = "take"]
    /\ UNCHANGED <<run, pending, rq, cur, sw, oncpu, pc, round, waiters, table>>

\* wake_all takes the table's lock and every waiter off it.
Take ==
    /\ waker.phase = "take" /\ table = None
    /\ table' = "waker"
    /\ waker' = [waker EXCEPT !.phase = "next", !.todo = waiters]
    /\ waiters' = {}
    /\ UNCHANGED <<run, pending, rq, cur, sw, oncpu, pc, round, signals>>

WakerDone ==
    /\ waker.phase = "signal" /\ \A t \in Threads : signals[t] = Rounds
    /\ waker' = [waker EXCEPT !.phase = "done"]
    /\ UNCHANGED <<run, pending, rq, cur, sw, oncpu, pc, round, waiters, signals, table>>

\* Each waiter in turn, still holding the table's lock.
NextWaiter ==
    /\ waker.phase = "next"
    /\ IF waker.todo = {}
       THEN /\ table' = None
            /\ waker' = [phase |-> "signal", todo |-> {}, t |-> None]
       ELSE /\ \E t \in waker.todo :
                  waker' = [phase |-> "pend", todo |-> waker.todo \ {t}, t |-> t]
            /\ UNCHANGED table
    /\ UNCHANGED <<run, pending, rq, cur, sw, oncpu, pc, round, waiters, signals>>

\* wake(): the flag first, so a thread about to block does not sleep ...
Pend ==
    /\ waker.phase = "pend"
    /\ pending' = [pending EXCEPT ![waker.t] = TRUE]
    /\ waker' = [waker EXCEPT !.phase = "lock"]
    /\ UNCHANGED <<run, rq, cur, sw, oncpu, pc, round, waiters, signals, table>>

\* ... then, under the thread's lock, a blocked thread becomes ready on
\* the queue of the CPU that placement picks.
Wake ==
    /\ waker.phase = "lock"
    /\ LET t == waker.t IN
       IF run[t] = "blocked"
       THEN /\ run' = [run EXCEPT ![t] = "ready"]
            /\ pending' = [pending EXCEPT ![t] = FALSE]
            /\ \E c \in Cpus : rq' = [rq EXCEPT ![c] = @ \cup {t}]
       ELSE UNCHANGED <<run, pending, rq>>
    /\ waker' = [waker EXCEPT !.phase = "next", !.t = None]
    /\ UNCHANGED <<cur, sw, oncpu, pc, round, waiters, signals, table>>

----------------------------------------------------------------------------

Finished == waker.phase = "done" /\ \A t \in Threads : pc[t] = "done"

Next ==
    \/ \E c \in Cpus, t \in Threads : Check(c, t) \/ Block(c, t) \/ After(c, t)
    \/ \E c \in Cpus, next \in Runnable : Preempt(c, next)
    \/ \E c \in Cpus : Release(c) \/ Acquire(c)
    \/ \E from, to \in Cpus, t \in Threads : Pull(from, to, t)
    \/ \E t \in Threads : Signal(t)
    \/ WakerDone \/ Take \/ NextWaiter \/ Pend \/ Wake
    \/ Finished /\ UNCHANGED vars

\* What one thread does in its own code, and what the waker does.
ThreadStep(t) == \E c \in Cpus : Check(c, t) \/ Block(c, t) \/ After(c, t)
WakerStep ==
    \/ \E t \in Threads : Signal(t)
    \/ WakerDone \/ Take \/ NextWaiter \/ Pend \/ Wake

\* A fair scheduler: a thread that keeps getting a CPU gets to run its
\* code, and one that keeps waiting on a queue gets picked. Switches and
\* the waker finish what they start. Balancing need not happen.
Fairness ==
    /\ \A t \in Threads : SF_vars(ThreadStep(t)) /\ SF_vars(\E c \in Cpus : Preempt(c, t))
    /\ \A c \in Cpus : WF_vars(Release(c)) /\ WF_vars(Acquire(c))
    /\ WF_vars(WakerStep)

Spec == Init /\ [][Next]_vars /\ Fairness

----------------------------------------------------------------------------
(* Properties.                                                             *)

\* No two CPUs ever use one thread's stack (`on_cpu` hands it over).
OneStackUser ==
    \A t \in Threads : Cardinality({c \in Cpus : OnStack(c, t)}) <= 1

\* A CPU on a thread's stack holds its context.
StackUserHoldsContext ==
    \A c \in Cpus, t \in Threads : OnStack(c, t) => oncpu[t]

\* Blocked threads are on no queue, ready ones on exactly one, and a CPU's
\* current thread is ready on its own queue.
QueuesConsistent ==
    /\ \A t \in Threads :
          IF run[t] = "blocked"
          THEN \A c \in Cpus : t \notin rq[c]
          ELSE Cardinality({c \in Cpus : t \in rq[c]}) = 1
    /\ \A c \in Cpus : cur[c] \in Threads => cur[c] \in rq[c]

\* A blocked thread whose condition holds has a wake on the way: it is
\* still in the table, or the waker is about to wake it.
WakeOnTheWay ==
    \A t \in Threads :
        run[t] = "blocked" /\ Condition(t) =>
            \/ t \in waiters
            \/ waker.phase \in {"next", "pend", "lock"} /\ t \in waker.todo \cup {waker.t}

\* Every wait ends.
AllFinish == <>Finished

Symmetry == Permutations(Threads)
=============================================================================
