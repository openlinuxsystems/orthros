\* SPDX-FileCopyrightText: 2026 The Dukovo contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

------------------------------- MODULE Timers -------------------------------
(***************************************************************************)
(* A model of how the timer thread finds the timers it must fire           *)
(* (kernel/radix/src/user/posix/timer.rs: `set`, `register`, `changed`,    *)
(* `run`, `expire_due` and `unregister`), with the wait table's            *)
(* `wait_until_or_timeout` and `wake_all` (sched/wait.rs) and the          *)
(* scheduler's `block` and `wake` beneath them.                            *)
(*                                                                         *)
(* Each process has one timer, and a thread of its sets it up to `Rounds`  *)
(* times, or turns it off. Setting it puts the process in `ARMED`, the     *)
(* list the timer thread looks at, unless it is there, and sets the timer  *)
(* with the list still locked; turning it off only clears it. Either then  *)
(* moves `CHANGES` and wakes the timer thread. A set timer expires when    *)
(* its time comes. The timer thread reads `CHANGES`, takes the list, and   *)
(* looks at each process's timer in turn: it fires an expired one, notes   *)
(* a set one for its next deadline, and takes a process whose timer is     *)
(* off out of the list, after checking again under the list's lock that    *)
(* it is off. Then it waits until `CHANGES` moves or, if it saw a timer    *)
(* set, until its deadline; a deadline is the first expiry it saw, which   *)
(* a timer set again later may have moved, so it may pass at any moment.   *)
(*                                                                         *)
(* Each critical section is one step: the list's and the process's locks   *)
(* (`unregister` holds both), and the wait table's. `CHANGES` moves by a   *)
(* read-modify-write and is read with acquire, after the timer is set      *)
(* under its process's lock, so the model's interleaving covers every      *)
(* execution. A timer with an interval is set again as it fires, under     *)
(* the same lock, and its process stays in the list; here a timer fires    *)
(* once. A POSIX timer waiting for its signal to be taken counts as set,   *)
(* so it is the same.                                                      *)
(*                                                                         *)
(* Listed says a process whose timer is set is in the list;                *)
(* EveryTimerFires says an expired timer fires (or is turned off). Both    *)
(* fail if a setter lets the list go before it sets the timer: the thread  *)
(* can then see the timer off and take the process out after the setter    *)
(* found it listed, as the `signal` test's `rearms` part once showed.      *)
(* EveryTimerFires also fails if the thread reads `CHANGES` after it       *)
(* looks.                                                                  *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Procs,      \* processes, each with one timer
    Rounds      \* how many times each process's thread sets or clears it

VARIABLES
    timer,      \* each process's timer: "off", "set" or "expired"
    list,       \* ARMED
    owner,      \* who holds ARMED's lock: a process's thread, or "none"
    changes,    \* CHANGES
    queued,     \* the timer thread is queued in the wait table
    asleep,     \* the scheduler has the timer thread blocked
    pending,    \* the scheduler holds a wake for its next block
    seen,       \* the CHANGES the timer thread read last
    snap,       \* the processes it took from the list, still to look at
    target,     \* the process it is taking out of the list
    deadline,   \* it saw a timer set, so it waits with a deadline
    passed,     \* that deadline has passed
    tpc,        \* where the timer thread is
    spc,        \* where each process's thread is
    rounds      \* how many times each process's thread has set or cleared

vars == <<timer, list, owner, changes, queued, asleep, pending, seen, snap, target,
          deadline, passed, tpc, spc, rounds>>

ThreadSteps == {"seen", "take", "look", "unregister", "wait", "block", "asleep", "woken"}
SetterSteps == {"idle", "register", "set", "unlock", "changed", "wake", "done"}
None == "none"

TypeOK ==
    /\ timer \in [Procs -> {"off", "set", "expired"}]
    /\ list \subseteq Procs
    /\ owner \in Procs \cup {None}
    /\ changes \in Nat
    /\ queued \in BOOLEAN
    /\ asleep \in BOOLEAN
    /\ pending \in BOOLEAN
    /\ seen \in Nat
    /\ snap \subseteq Procs
    /\ target \in Procs \cup {None}
    /\ deadline \in BOOLEAN
    /\ passed \in BOOLEAN
    /\ tpc \in ThreadSteps
    /\ spc \in [Procs -> SetterSteps]
    /\ rounds \in [Procs -> 0..Rounds]

Init ==
    /\ timer = [p \in Procs |-> "off"]
    /\ list = {}
    /\ owner = None
    /\ changes = 0
    /\ queued = FALSE
    /\ asleep = FALSE
    /\ pending = FALSE
    /\ seen = 0
    /\ snap = {}
    /\ target = None
    /\ deadline = FALSE
    /\ passed = FALSE
    /\ tpc = "seen"
    /\ spc = [p \in Procs |-> "idle"]
    /\ rounds = [p \in Procs |-> 0]

\* The scheduler's wake of the timer thread.
SchedWake ==
    IF asleep
    THEN /\ asleep' = FALSE
         /\ UNCHANGED pending
    ELSE /\ pending' = TRUE
         /\ UNCHANGED asleep

----------------------------------------------------------------------------
(* A process's thread (`set`, and `changed`).                              *)

\* A timer set needs the list; one turned off does not.
Choose(p) ==
    /\ spc[p] = "idle"
    /\ IF rounds[p] = Rounds
       THEN /\ spc' = [spc EXCEPT ![p] = "done"]
            /\ UNCHANGED timer
       ELSE \/ /\ spc' = [spc EXCEPT ![p] = "register"]
               /\ UNCHANGED timer
            \/ /\ timer' = [timer EXCEPT ![p] = "off"]
               /\ spc' = [spc EXCEPT ![p] = "changed"]
    /\ UNCHANGED <<list, owner, changes, queued, asleep, pending, seen, snap, target,
                   deadline, passed, tpc, rounds>>

\* `register`: the list locked, the process put in it.
Register(p) ==
    /\ spc[p] = "register" /\ owner = None
    /\ owner' = p
    /\ list' = list \cup {p}
    /\ spc' = [spc EXCEPT ![p] = "set"]
    /\ UNCHANGED <<timer, changes, queued, asleep, pending, seen, snap, target,
                   deadline, passed, tpc, rounds>>

\* The timer, under the process's lock, the list still locked.
Set(p) ==
    /\ spc[p] = "set"
    /\ timer' = [timer EXCEPT ![p] = "set"]
    /\ spc' = [spc EXCEPT ![p] = "unlock"]
    /\ UNCHANGED <<list, owner, changes, queued, asleep, pending, seen, snap, target,
                   deadline, passed, tpc, rounds>>

Unlock(p) ==
    /\ spc[p] = "unlock"
    /\ owner' = None
    /\ spc' = [spc EXCEPT ![p] = "changed"]
    /\ UNCHANGED <<timer, list, changes, queued, asleep, pending, seen, snap, target,
                   deadline, passed, tpc, rounds>>

Changed(p) ==
    /\ spc[p] = "changed"
    /\ changes' = changes + 1
    /\ spc' = [spc EXCEPT ![p] = "wake"]
    /\ UNCHANGED <<timer, list, owner, queued, asleep, pending, seen, snap, target,
                   deadline, passed, tpc, rounds>>

\* `wake_all`'s critical section.
WakeAll(p) ==
    /\ spc[p] = "wake"
    /\ IF queued
       THEN /\ SchedWake
            /\ queued' = FALSE
       ELSE UNCHANGED <<queued, asleep, pending>>
    /\ spc' = [spc EXCEPT ![p] = "idle"]
    /\ rounds' = [rounds EXCEPT ![p] = rounds[p] + 1]
    /\ UNCHANGED <<timer, list, owner, changes, seen, snap, target, deadline, passed,
                   tpc>>

Setter(p) ==
    Choose(p) \/ Register(p) \/ Set(p) \/ Unlock(p) \/ Changed(p) \/ WakeAll(p)

\* Its time comes.
Expire(p) ==
    /\ timer[p] = "set"
    /\ timer' = [timer EXCEPT ![p] = "expired"]
    /\ UNCHANGED <<list, owner, changes, queued, asleep, pending, seen, snap, target,
                   deadline, passed, tpc, spc, rounds>>

----------------------------------------------------------------------------
(* The timer thread (`run` and `expire_due`).                              *)

Seen ==
    /\ tpc = "seen"
    /\ seen' = changes
    /\ tpc' = "take"
    /\ UNCHANGED <<timer, list, owner, changes, queued, asleep, pending, snap, target,
                   deadline, passed, spc, rounds>>

\* The list, locked: what it holds now.
Take ==
    /\ tpc = "take" /\ owner = None
    /\ snap' = list
    /\ deadline' = FALSE
    /\ passed' = FALSE
    /\ tpc' = "look"
    /\ UNCHANGED <<timer, list, owner, changes, queued, asleep, pending, seen, target,
                   spc, rounds>>

\* One process's timers, under its lock: an expired one fires, a set one
\* is noted for the deadline, and a process with none set goes on to
\* `unregister`.
Look ==
    /\ tpc = "look"
    /\ IF snap = {}
       THEN /\ tpc' = "wait"
            /\ UNCHANGED <<timer, snap, target, deadline>>
       ELSE \E p \in snap :
              LET after == IF timer[p] = "expired" THEN "off" ELSE timer[p]
              IN /\ timer' = [timer EXCEPT ![p] = after]
                 /\ deadline' = (deadline \/ after = "set")
                 /\ snap' = snap \ {p}
                 /\ IF after = "off"
                    THEN /\ target' = p
                         /\ tpc' = "unregister"
                    ELSE UNCHANGED <<target, tpc>>
    /\ UNCHANGED <<list, owner, changes, queued, asleep, pending, seen, passed, spc,
                   rounds>>

\* `unregister`: the list's lock, then the process's: out only if its
\* timer is still off.
Unregister ==
    /\ tpc = "unregister" /\ owner = None
    /\ list' = IF timer[target] = "off" THEN list \ {target} ELSE list
    /\ target' = None
    /\ tpc' = "look"
    /\ UNCHANGED <<timer, owner, changes, queued, asleep, pending, seen, snap, deadline,
                   passed, spc, rounds>>

\* `wait_until_or_timeout`'s critical section: the condition, the
\* deadline, then the queue.
Wait ==
    /\ tpc = "wait"
    /\ IF changes # seen \/ passed
       THEN /\ tpc' = "seen"
            /\ UNCHANGED queued
       ELSE /\ queued' = TRUE
            /\ tpc' = "block"
    /\ UNCHANGED <<timer, list, owner, changes, asleep, pending, seen, snap, target,
                   deadline, passed, spc, rounds>>

\* `sleep_until_inner`'s block, which a deadline that has passed ends at
\* once.
Block ==
    /\ tpc = "block"
    /\ IF pending \/ passed
       THEN /\ pending' = FALSE
            /\ tpc' = "woken"
            /\ UNCHANGED asleep
       ELSE /\ asleep' = TRUE
            /\ tpc' = "asleep"
            /\ UNCHANGED pending
    /\ UNCHANGED <<timer, list, owner, changes, queued, seen, snap, target, deadline,
                   passed, spc, rounds>>

Wake ==
    /\ tpc = "asleep" /\ ~asleep
    /\ tpc' = "woken"
    /\ UNCHANGED <<timer, list, owner, changes, queued, asleep, pending, seen, snap,
                   target, deadline, passed, spc, rounds>>

\* The deadline passes, and wakes the thread if it sleeps.
Timeout ==
    /\ deadline /\ ~passed
    /\ passed' = TRUE
    /\ asleep' = FALSE
    /\ UNCHANGED <<timer, list, owner, changes, queued, pending, seen, snap, target,
                   deadline, tpc, spc, rounds>>

\* Off the table, then the condition again.
Woken ==
    /\ tpc = "woken"
    /\ queued' = FALSE
    /\ tpc' = "wait"
    /\ UNCHANGED <<timer, list, owner, changes, asleep, pending, seen, snap, target,
                   deadline, passed, spc, rounds>>

Thread == Seen \/ Take \/ Look \/ Unregister \/ Wait \/ Block \/ Wake \/ Woken

----------------------------------------------------------------------------

\* Nothing is left to do: every process's thread is done and the timer
\* thread sleeps with no deadline.
Settled ==
    /\ \A p \in Procs : spc[p] = "done"
    /\ tpc = "asleep" /\ asleep /\ ~deadline

Next ==
    \/ \E p \in Procs : Setter(p) \/ Expire(p)
    \/ Thread \/ Timeout
    \/ Settled /\ UNCHANGED vars

Spec ==
    /\ Init /\ [][Next]_vars
    /\ WF_vars(Thread)
    /\ WF_vars(Timeout)
    /\ \A p \in Procs : WF_vars(Setter(p)) /\ WF_vars(Expire(p))

\* A process whose timer is set or expired is in the list.
Listed == \A p \in Procs : timer[p] # "off" => p \in list

\* Asleep, the timer thread is queued.
Queued == asleep => queued

\* Every expired timer fires, or is turned off.
EveryTimerFires == \A p \in Procs : timer[p] = "expired" ~> timer[p] = "off"

=============================================================================
