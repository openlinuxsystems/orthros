\* SPDX-FileCopyrightText: 2026 The Dukovo contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

-------------------------------- MODULE Poll --------------------------------
(***************************************************************************)
(* A model of how `poll` waits for the objects it looks at                 *)
(* (kernel/radix/src/user/posix/poll.rs: `wait_ready`, `Polling` and       *)
(* `Polled::changed`), with the wait table's `wait_until` and `wake_all`   *)
(* (sched/wait.rs) and the scheduler's `block` and `wake` beneath them.    *)
(*                                                                         *)
(* Each object starts not ready and is made ready once, by its own         *)
(* changer, which then calls `changed`: if the object's count of pollers   *)
(* is not zero, it moves the generation and wakes every thread waiting on  *)
(* the generation's key. A poller first looks at its objects without       *)
(* counting itself, and returns if one is ready. Otherwise it counts       *)
(* itself among the pollers of each, one object at a time; then, until it  *)
(* finds one ready, it reads the generation, looks, and waits for the      *)
(* generation to move. Its wait locks the wait table and either finds the  *)
(* generation moved and returns, or queues itself; then it sleeps, and a   *)
(* wake before the sleep makes the sleep return at once. After a wake it   *)
(* takes itself off the table and checks again. Once it has found an       *)
(* object ready it stops counting itself. Which objects each poller looks  *)
(* at is chosen at the start, so one run checks every choice.              *)
(*                                                                         *)
(* Each critical section of the wait table's lock and each atomic access   *)
(* is one step. The accesses the argument relies on are ordered as         *)
(* sequential consistency orders them: the poller's count is a             *)
(* sequentially consistent read-modify-write, and `changed` has a full     *)
(* barrier between the change and its read of the count (or the change is  *)
(* itself such a read-modify-write, a notification's), so the model's      *)
(* interleaving of single accesses covers every execution. Readiness only  *)
(* grows, so a look at the objects one after another finds what one look   *)
(* at them all, at its start or at its end, would: a look is one step.     *)
(* Signals and deadlines end a wait sooner, by the same condition, and are *)
(* left out.                                                               *)
(*                                                                         *)
(* NoLostWakeup says a poller never sleeps while an object it looks at is  *)
(* ready, unless that object's changer is still on its way to wake it;     *)
(* EveryPollEnds says every poll ends, as every object is made ready in    *)
(* the end. NoLostWakeup fails if a poller counts itself after it looks,   *)
(* or reads the generation after it looks, or if `changed` reads the count *)
(* before the change. Reading the generation before counting is safe: a    *)
(* wait then only returns more often.                                      *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
    Pollers,    \* threads that poll
    Objects     \* what they look at, each made ready once by its changer

VARIABLES
    watch,      \* the objects each poller looks at, chosen at the start
    ready,      \* each object is ready
    count,      \* each object's count of pollers
    gen,        \* GENERATION
    queued,     \* the pollers queued in the wait table on its key
    asleep,     \* the scheduler has the poller blocked
    pending,    \* the scheduler holds a wake for its next block
    seen,       \* the generation each poller read last
    todo,       \* the objects each poller has still to count itself in
    ppc,        \* where each poller is
    cpc         \* where each object's changer is

vars == <<watch, ready, count, gen, queued, asleep, pending, seen, todo, ppc, cpc>>

PollerSteps == {"first", "count", "generation", "look", "wait", "block", "asleep",
                "woken", "uncount", "done"}
ChangerSteps == {"change", "read", "move", "wake", "done"}

TypeOK ==
    /\ watch \in [Pollers -> SUBSET Objects]
    /\ ready \in [Objects -> BOOLEAN]
    /\ count \in [Objects -> 0..Cardinality(Pollers)]
    /\ gen \in 0..Cardinality(Objects)
    /\ queued \subseteq Pollers
    /\ asleep \in [Pollers -> BOOLEAN]
    /\ pending \in [Pollers -> BOOLEAN]
    /\ seen \in [Pollers -> 0..Cardinality(Objects)]
    /\ todo \in [Pollers -> SUBSET Objects]
    /\ ppc \in [Pollers -> PollerSteps]
    /\ cpc \in [Objects -> ChangerSteps]

Init ==
    /\ watch \in [Pollers -> SUBSET Objects \ {{}}]
    /\ ready = [o \in Objects |-> FALSE]
    /\ count = [o \in Objects |-> 0]
    /\ gen = 0
    /\ queued = {}
    /\ asleep = [p \in Pollers |-> FALSE]
    /\ pending = [p \in Pollers |-> FALSE]
    /\ seen = [p \in Pollers |-> 0]
    /\ todo = [p \in Pollers |-> {}]
    /\ ppc = [p \in Pollers |-> "first"]
    /\ cpc = [o \in Objects |-> "change"]

AnyReady(p) == \E o \in watch[p] : ready[o]

----------------------------------------------------------------------------
(* A poller (`poll` and `wait_ready`).                                     *)

\* The first look, counted nowhere.
First(p) ==
    /\ ppc[p] = "first"
    /\ IF AnyReady(p)
       THEN /\ ppc' = [ppc EXCEPT ![p] = "done"]
            /\ UNCHANGED todo
       ELSE /\ ppc' = [ppc EXCEPT ![p] = "count"]
            /\ todo' = [todo EXCEPT ![p] = watch[p]]
    /\ UNCHANGED <<watch, ready, count, gen, queued, asleep, pending, seen, cpc>>

\* `Polling::new`: one object's count at a time.
Count(p) ==
    /\ ppc[p] = "count"
    /\ \E o \in todo[p] :
          /\ count' = [count EXCEPT ![o] = count[o] + 1]
          /\ todo' = [todo EXCEPT ![p] = todo[p] \ {o}]
          /\ ppc' = [ppc EXCEPT ![p] = IF todo[p] = {o} THEN "generation" ELSE "count"]
    /\ UNCHANGED <<watch, ready, gen, queued, asleep, pending, seen, cpc>>

Generation(p) ==
    /\ ppc[p] = "generation"
    /\ seen' = [seen EXCEPT ![p] = gen]
    /\ ppc' = [ppc EXCEPT ![p] = "look"]
    /\ UNCHANGED <<watch, ready, count, gen, queued, asleep, pending, todo, cpc>>

Look(p) ==
    /\ ppc[p] = "look"
    /\ ppc' = [ppc EXCEPT ![p] = IF AnyReady(p) THEN "uncount" ELSE "wait"]
    /\ UNCHANGED <<watch, ready, count, gen, queued, asleep, pending, seen, todo, cpc>>

\* `wait_until`'s critical section: the condition, then the queue.
Wait(p) ==
    /\ ppc[p] = "wait"
    /\ IF gen # seen[p]
       THEN /\ ppc' = [ppc EXCEPT ![p] = "generation"]
            /\ UNCHANGED queued
       ELSE /\ queued' = queued \cup {p}
            /\ ppc' = [ppc EXCEPT ![p] = "block"]
    /\ UNCHANGED <<watch, ready, count, gen, asleep, pending, seen, todo, cpc>>

Block(p) ==
    /\ ppc[p] = "block"
    /\ IF pending[p]
       THEN /\ pending' = [pending EXCEPT ![p] = FALSE]
            /\ ppc' = [ppc EXCEPT ![p] = "woken"]
            /\ UNCHANGED asleep
       ELSE /\ asleep' = [asleep EXCEPT ![p] = TRUE]
            /\ ppc' = [ppc EXCEPT ![p] = "asleep"]
            /\ UNCHANGED pending
    /\ UNCHANGED <<watch, ready, count, gen, queued, seen, todo, cpc>>

Wake(p) ==
    /\ ppc[p] = "asleep" /\ ~asleep[p]
    /\ ppc' = [ppc EXCEPT ![p] = "woken"]
    /\ UNCHANGED <<watch, ready, count, gen, queued, asleep, pending, seen, todo, cpc>>

\* Off the table (if `wake_all` has not taken it off), then the condition
\* again.
Woken(p) ==
    /\ ppc[p] = "woken"
    /\ queued' = queued \ {p}
    /\ ppc' = [ppc EXCEPT ![p] = "wait"]
    /\ UNCHANGED <<watch, ready, count, gen, asleep, pending, seen, todo, cpc>>

\* `Polling`'s drop.
Uncount(p) ==
    /\ ppc[p] = "uncount"
    /\ count' = [o \in Objects |-> IF o \in watch[p] THEN count[o] - 1 ELSE count[o]]
    /\ ppc' = [ppc EXCEPT ![p] = "done"]
    /\ UNCHANGED <<watch, ready, gen, queued, asleep, pending, seen, todo, cpc>>

Poller(p) ==
    First(p) \/ Count(p) \/ Generation(p) \/ Look(p) \/ Wait(p) \/ Block(p) \/ Wake(p)
    \/ Woken(p) \/ Uncount(p)

----------------------------------------------------------------------------
(* An object's changer: the change, then `Polled::changed`.                *)

Change(o) ==
    /\ cpc[o] = "change"
    /\ ready' = [ready EXCEPT ![o] = TRUE]
    /\ cpc' = [cpc EXCEPT ![o] = "read"]
    /\ UNCHANGED <<watch, count, gen, queued, asleep, pending, seen, todo, ppc>>

Read(o) ==
    /\ cpc[o] = "read"
    /\ cpc' = [cpc EXCEPT ![o] = IF count[o] > 0 THEN "move" ELSE "done"]
    /\ UNCHANGED <<watch, ready, count, gen, queued, asleep, pending, seen, todo, ppc>>

Move(o) ==
    /\ cpc[o] = "move"
    /\ gen' = gen + 1
    /\ cpc' = [cpc EXCEPT ![o] = "wake"]
    /\ UNCHANGED <<watch, ready, count, queued, asleep, pending, seen, todo, ppc>>

\* `wake_all`'s critical section: every queued poller off the table and
\* woken; one not asleep yet keeps the wake for its block.
WakeAll(o) ==
    /\ cpc[o] = "wake"
    /\ asleep' = [p \in Pollers |-> IF p \in queued THEN FALSE ELSE asleep[p]]
    /\ pending' = [p \in Pollers |->
                       IF p \in queued /\ ~asleep[p] THEN TRUE ELSE pending[p]]
    /\ queued' = {}
    /\ cpc' = [cpc EXCEPT ![o] = "done"]
    /\ UNCHANGED <<watch, ready, count, gen, seen, todo, ppc>>

Changer(o) == Change(o) \/ Read(o) \/ Move(o) \/ WakeAll(o)

----------------------------------------------------------------------------

Done == (\A p \in Pollers : ppc[p] = "done") /\ (\A o \in Objects : cpc[o] = "done")

Next ==
    \/ \E p \in Pollers : Poller(p)
    \/ \E o \in Objects : Changer(o)
    \/ Done /\ UNCHANGED vars

Spec ==
    /\ Init /\ [][Next]_vars
    /\ \A p \in Pollers : WF_vars(Poller(p))
    /\ \A o \in Objects : WF_vars(Changer(o))

\* Asleep, a poller is queued, and each object it looks at that is ready
\* has a changer still to wake it.
NoLostWakeup ==
    \A p \in Pollers :
        asleep[p] =>
            /\ p \in queued
            /\ \A o \in watch[p] : ready[o] => cpc[o] \in {"read", "move", "wake"}

\* A poller waiting is counted in every object it looks at.
Counted ==
    \A p \in Pollers :
        ppc[p] \in {"generation", "look", "wait", "block", "asleep", "woken"} =>
            \A o \in watch[p] : count[o] >= 1

\* Every poll ends.
EveryPollEnds == \A p \in Pollers : <>(ppc[p] = "done")

=============================================================================
