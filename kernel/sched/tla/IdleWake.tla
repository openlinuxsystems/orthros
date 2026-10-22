\* SPDX-FileCopyrightText: 2026 The Dukovo contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

----------------------------- MODULE IdleWake ------------------------------
(***************************************************************************)
(* A model of how an idle CPU waits and how other CPUs wake it            *)
(* (kernel/radix/src/sched/mod.rs: `idle_loop`, `poll` and `kick`).        *)
(*                                                                         *)
(* A waker sets the idle CPU's `need_resched`, then reads its `polling`    *)
(* flag: if the CPU polls (or waits in MWAIT, which watches the `wake`     *)
(* word) it writes `wake`; otherwise it sends an interrupt. The idle CPU,  *)
(* with interrupts masked, checks `need_resched` and then either polls     *)
(* (under a hypervisor without MWAIT, ADR 0037) or goes straight to its    *)
(* sleep. Polling sets `polling`, spins with interrupts on until           *)
(* `need_resched` is set or its time is up, clears `polling` and checks    *)
(* `need_resched` again. Sleeping sets `polling` only for MWAIT, checks    *)
(* `need_resched` once more and halts (STI; HLT, so an interrupt that is   *)
(* already pending ends the halt at once) or waits in MWAIT.              *)
(*                                                                         *)
(* Every flag access the protocol relies on is sequentially consistent in  *)
(* the code, so the model's interleaving of single accesses covers every   *)
(* execution. Whether the CPU has MWAIT and whether it polls are chosen    *)
(* at the start, so one run checks all four combinations, and a poll may   *)
(* end at any moment, which covers every polling limit.                   *)
(*                                                                         *)
(* NoLostWakeup says a CPU never sleeps with `need_resched` set and        *)
(* nothing on its way to wake it; EventuallyRuns says it always ends up    *)
(* switching to the woken thread. Neither holds if the waker reads         *)
(* `polling` before it sets `need_resched`, or if the idle CPU clears      *)
(* `polling` after its last check.                                        *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Wakers      \* CPUs that wake the idle one

VARIABLES
    mwait,      \* the CPU waits with MONITOR and MWAIT (else HLT)
    pollFirst,  \* it polls before sleeping (a virtual CPU without MWAIT)
    need,       \* its need_resched
    polling,    \* its polling flag
    wake,       \* its wake word
    irq,        \* an interrupt is pending for it
    ipc,        \* where the idle CPU is in its loop
    wpc         \* where each waker is in `kick`

vars == <<mwait, pollFirst, need, polling, wake, irq, ipc, wpc>>

IdleSteps == {"top", "poll", "unpoll", "polled", "prepare", "check", "asleep", "woken",
              "run"}
WakerSteps == {"set", "read", "done"}

TypeOK ==
    /\ mwait \in BOOLEAN
    /\ pollFirst \in BOOLEAN
    /\ need \in BOOLEAN
    /\ polling \in BOOLEAN
    /\ wake \in {0, 1}
    /\ irq \in BOOLEAN
    /\ ipc \in IdleSteps
    /\ wpc \in [Wakers -> WakerSteps]

Init ==
    /\ mwait \in BOOLEAN
    /\ pollFirst \in {FALSE, ~mwait}
    /\ need = FALSE
    /\ polling = FALSE
    /\ wake = 0
    /\ irq = FALSE
    /\ ipc = "top"
    /\ wpc = [w \in Wakers |-> "set"]

----------------------------------------------------------------------------
(* The idle CPU (idle_loop and poll).                                      *)

\* With interrupts masked: switch if a thread is waiting, else start
\* waiting with the wake word cleared.
Top ==
    /\ ipc = "top"
    /\ IF need
       THEN /\ ipc' = "run"
            /\ UNCHANGED wake
       ELSE /\ wake' = 0
            /\ ipc' = IF pollFirst THEN "poll" ELSE "prepare"
    /\ UNCHANGED <<mwait, pollFirst, need, polling, irq, wpc>>

\* Polling sets the flag, then spins with interrupts on: an interrupt is
\* taken and handled, and the poll ends when need_resched is seen or at any
\* moment (its time is up).
StartPoll ==
    /\ ipc = "poll" /\ ~polling
    /\ polling' = TRUE
    /\ UNCHANGED <<mwait, pollFirst, need, wake, irq, ipc, wpc>>

TakeInterruptWhilePolling ==
    /\ ipc = "poll" /\ polling /\ irq
    /\ irq' = FALSE
    /\ UNCHANGED <<mwait, pollFirst, need, polling, wake, ipc, wpc>>

EndPoll ==
    /\ ipc = "poll" /\ polling
    /\ ipc' = "unpoll"
    /\ UNCHANGED <<mwait, pollFirst, need, polling, wake, irq, wpc>>

\* Interrupts masked again: clear the flag, then look once more.
Unpoll ==
    /\ ipc = "unpoll"
    /\ polling' = FALSE
    /\ ipc' = "polled"
    /\ UNCHANGED <<mwait, pollFirst, need, wake, irq, wpc>>

Polled ==
    /\ ipc = "polled"
    /\ ipc' = IF need THEN "run" ELSE "prepare"
    /\ UNCHANGED <<mwait, pollFirst, need, polling, wake, irq, wpc>>

\* Going to sleep: only MWAIT watches the wake word, so only it may say it
\* polls.
Prepare ==
    /\ ipc = "prepare"
    /\ polling' = mwait
    /\ ipc' = "check"
    /\ UNCHANGED <<mwait, pollFirst, need, wake, irq, wpc>>

Check ==
    /\ ipc = "check"
    /\ ipc' = IF need THEN "woken" ELSE "asleep"
    /\ UNCHANGED <<mwait, pollFirst, need, polling, wake, irq, wpc>>

\* STI; HLT or MWAIT: a pending interrupt ends the sleep (and is handled),
\* and so does a write to the wake word under MWAIT.
WakeUp ==
    /\ ipc = "asleep"
    /\ irq \/ (mwait /\ wake = 1)
    /\ irq' = FALSE
    /\ ipc' = "woken"
    /\ UNCHANGED <<mwait, pollFirst, need, polling, wake, wpc>>

Woken ==
    /\ ipc = "woken"
    /\ polling' = FALSE
    /\ ipc' = "top"
    /\ UNCHANGED <<mwait, pollFirst, need, wake, irq, wpc>>

Idle ==
    \/ Top \/ StartPoll \/ TakeInterruptWhilePolling \/ EndPoll \/ Unpoll \/ Polled
    \/ Prepare \/ Check \/ WakeUp \/ Woken

----------------------------------------------------------------------------
(* The wakers (kick).                                                      *)

Set(w) ==
    /\ wpc[w] = "set"
    /\ need' = TRUE
    /\ wpc' = [wpc EXCEPT ![w] = "read"]
    /\ UNCHANGED <<mwait, pollFirst, polling, wake, irq, ipc>>

Read(w) ==
    /\ wpc[w] = "read"
    /\ IF polling
       THEN /\ wake' = 1
            /\ UNCHANGED irq
       ELSE /\ irq' = TRUE
            /\ UNCHANGED wake
    /\ wpc' = [wpc EXCEPT ![w] = "done"]
    /\ UNCHANGED <<mwait, pollFirst, need, polling, ipc>>

Waker(w) == Set(w) \/ Read(w)

----------------------------------------------------------------------------

Done == ipc = "run" /\ \A w \in Wakers : wpc[w] = "done"

Next ==
    \/ Idle
    \/ \E w \in Wakers : Waker(w)
    \/ Done /\ UNCHANGED vars

Spec == Init /\ [][Next]_vars /\ WF_vars(Idle) /\ \A w \in Wakers : WF_vars(Waker(w))

\* Asleep with need_resched set, the CPU has an interrupt pending, or a
\* written wake word it watches, or a waker that has yet to signal.
NoLostWakeup ==
    (ipc = "asleep" /\ need) =>
        \/ irq
        \/ mwait /\ wake = 1
        \/ \E w \in Wakers : wpc[w] # "done"

\* Once a waker has set need_resched, the idle CPU switches.
EventuallyRuns == need ~> (ipc = "run")

=============================================================================
