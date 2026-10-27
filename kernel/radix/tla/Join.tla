\* SPDX-FileCopyrightText: 2026 The Dukovo contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

-------------------------------- MODULE Join --------------------------------
(***************************************************************************)
(* A model of `join` (kernel/radix/src/fs/helpers.rs): a caller runs one   *)
(* half of some work itself and hands the other half to a helper thread,   *)
(* which another CPU runs; the half lives on the caller's stack, so the    *)
(* caller returns only once no helper can touch it.                        *)
(*                                                                         *)
(* A caller claims an idle helper (idle to claimed, one compare-and-swap), *)
(* gives it the half, marks it posted and wakes it; with none idle it runs *)
(* both halves itself. After its own half it takes the other back if the   *)
(* helper has not started it (posted to idle, a compare-and-swap) and runs *)
(* it; otherwise it waits until the helper is done and marks it idle       *)
(* again. A helper waits until it is posted, starts (posted to running, a  *)
(* compare-and-swap that fails if the caller took the half back), runs the *)
(* half, marks itself done and wakes its caller.                           *)
(*                                                                         *)
(* Both wait with `wait_until`: the condition is checked with the wait     *)
(* table locked and the waiter queued in the same step, and `wake_all`     *)
(* takes every waiter on its key off the table; a waiter goes on only once *)
(* it is off the table, then checks again. A wake that comes before the    *)
(* sleep is not lost (Futex.tla checks the scheduler's part of that), and  *)
(* a waiter may also wake for no reason, taking itself off.                *)
(*                                                                         *)
(* RunsOnce says each half runs exactly once per join; NoUseAfterReturn    *)
(* says a helper runs a half only while its caller is still in that join;  *)
(* OneCaller says a helper serves one caller at a time; EveryJoinEnds says *)
(* every join returns. A helper that starts without its compare-and-swap  *)
(* runs a half its caller took back, and a caller that returns without     *)
(* waiting for a started half returns before it ran.                       *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
    Callers,    \* threads that join
    Helpers,    \* helper threads, one on each CPU
    Rounds,     \* how many joins each caller makes
    None        \* no helper, or no half

VARIABLES
    hstate,     \* each helper's state
    hjob,       \* the half each helper has: <<caller, round>>, or None
    hpc,        \* where each helper is
    cpc,        \* where each caller is
    mine,       \* the helper each caller claimed, or None
    round,      \* each caller's join
    ran,        \* how often each caller's other half ran in this join
    queued      \* the threads on the wait table

vars == <<hstate, hjob, hpc, cpc, mine, round, ran, queued>>

States == {"idle", "claimed", "posted", "running", "done"}
CallerSteps ==
    {"pick", "post", "own", "steal", "wait", "free", "release", "end"}
HelperSteps == {"wait", "take", "run", "finish"}
Threads == Callers \union Helpers

TypeOK ==
    /\ hstate \in [Helpers -> States]
    /\ hjob \in [Helpers -> (Callers \X (1..Rounds)) \union {None}]
    /\ hpc \in [Helpers -> HelperSteps]
    /\ cpc \in [Callers -> CallerSteps]
    /\ mine \in [Callers -> Helpers \union {None}]
    /\ round \in [Callers -> 1..Rounds]
    /\ ran \in [Callers -> 0..2]
    /\ queued \subseteq Threads

Init ==
    /\ hstate = [h \in Helpers |-> "idle"]
    /\ hjob = [h \in Helpers |-> None]
    /\ hpc = [h \in Helpers |-> "wait"]
    /\ cpc = [c \in Callers |-> "pick"]
    /\ mine = [c \in Callers |-> None]
    /\ round = [c \in Callers |-> 1]
    /\ ran = [c \in Callers |-> 0]
    /\ queued = {}

\* `wake_all` on a helper's keys: the one it waits on for a half, and the
\* one its caller waits on for the end of it. Every thread queued on the
\* key goes on.
WakeHelper(h) == queued' = queued \ {h}
WakeCaller(h) == queued' = queued \ {c \in Callers : mine[c] = h}

----------------------------------------------------------------------------
(* A caller (`join`).                                                      *)

\* Claim an idle helper, or run both halves here.
Pick(c) ==
    /\ cpc[c] = "pick"
    /\ IF \E h \in Helpers : hstate[h] = "idle"
       THEN \E h \in Helpers :
                /\ hstate[h] = "idle"
                /\ hstate' = [hstate EXCEPT ![h] = "claimed"]
                /\ hjob' = [hjob EXCEPT ![h] = <<c, round[c]>>]
                /\ mine' = [mine EXCEPT ![c] = h]
                /\ cpc' = [cpc EXCEPT ![c] = "post"]
                /\ UNCHANGED ran
       ELSE \* Both halves, one after the other.
            /\ ran' = [ran EXCEPT ![c] = 1]
            /\ cpc' = [cpc EXCEPT ![c] = "release"]
            /\ UNCHANGED <<hstate, hjob, mine>>
    /\ UNCHANGED <<hpc, round, queued>>

\* The half is in place: posted, and the helper woken.
Post(c) ==
    /\ cpc[c] = "post"
    /\ hstate' = [hstate EXCEPT ![mine[c]] = "posted"]
    /\ WakeHelper(mine[c])
    /\ cpc' = [cpc EXCEPT ![c] = "own"]
    /\ UNCHANGED <<hjob, hpc, mine, round, ran>>

\* The caller's own half.
Own(c) ==
    /\ cpc[c] = "own"
    /\ cpc' = [cpc EXCEPT ![c] = "steal"]
    /\ UNCHANGED <<hstate, hjob, hpc, mine, round, ran, queued>>

\* Take the half back if the helper has not started it.
Steal(c) ==
    /\ cpc[c] = "steal"
    /\ IF hstate[mine[c]] = "posted"
       THEN /\ hstate' = [hstate EXCEPT ![mine[c]] = "idle"]
            /\ hjob' = [hjob EXCEPT ![mine[c]] = None]
            /\ ran' = [ran EXCEPT ![c] = @ + 1]
            /\ mine' = [mine EXCEPT ![c] = None]
            /\ cpc' = [cpc EXCEPT ![c] = "release"]
       ELSE /\ cpc' = [cpc EXCEPT ![c] = "wait"]
            /\ UNCHANGED <<hstate, hjob, ran, mine>>
    /\ UNCHANGED <<hpc, round, queued>>

\* `wait_until(done)`: go on if the helper is done, or queue.
Wait(c) ==
    /\ cpc[c] = "wait" /\ c \notin queued
    /\ IF hstate[mine[c]] = "done"
       THEN /\ cpc' = [cpc EXCEPT ![c] = "free"]
            /\ UNCHANGED queued
       ELSE /\ queued' = queued \union {c}
            /\ UNCHANGED cpc
    /\ UNCHANGED <<hstate, hjob, hpc, mine, round, ran>>

\* The helper is idle again, for any caller.
Free(c) ==
    /\ cpc[c] = "free"
    /\ hstate' = [hstate EXCEPT ![mine[c]] = "idle"]
    /\ hjob' = [hjob EXCEPT ![mine[c]] = None]
    /\ mine' = [mine EXCEPT ![c] = None]
    /\ cpc' = [cpc EXCEPT ![c] = "release"]
    /\ UNCHANGED <<hpc, round, ran, queued>>

\* The join returns; the next one starts.
Release(c) ==
    /\ cpc[c] = "release"
    /\ IF round[c] < Rounds
       THEN /\ round' = [round EXCEPT ![c] = @ + 1]
            /\ ran' = [ran EXCEPT ![c] = 0]
            /\ cpc' = [cpc EXCEPT ![c] = "pick"]
       ELSE /\ cpc' = [cpc EXCEPT ![c] = "end"]
            /\ UNCHANGED <<round, ran>>
    /\ UNCHANGED <<hstate, hjob, hpc, mine, queued>>

Caller(c) ==
    \/ Pick(c) \/ Post(c) \/ Own(c) \/ Steal(c) \/ Wait(c) \/ Free(c)
    \/ Release(c)

----------------------------------------------------------------------------
(* A helper.                                                               *)

\* `wait_until(posted)`.
Sleep(h) ==
    /\ hpc[h] = "wait" /\ h \notin queued
    /\ IF hstate[h] = "posted"
       THEN /\ hpc' = [hpc EXCEPT ![h] = "take"]
            /\ UNCHANGED queued
       ELSE /\ queued' = queued \union {h}
            /\ UNCHANGED hpc
    /\ UNCHANGED <<hstate, hjob, cpc, mine, round, ran>>

\* Start, unless the caller took the half back meanwhile.
Take(h) ==
    /\ hpc[h] = "take"
    /\ IF hstate[h] = "posted"
       THEN /\ hstate' = [hstate EXCEPT ![h] = "running"]
            /\ hpc' = [hpc EXCEPT ![h] = "run"]
       ELSE /\ hpc' = [hpc EXCEPT ![h] = "wait"]
            /\ UNCHANGED hstate
    /\ UNCHANGED <<hjob, cpc, mine, round, ran, queued>>

\* The half, through the pointer to the caller's stack.
Run(h) ==
    /\ hpc[h] = "run"
    /\ ran' = [ran EXCEPT ![hjob[h][1]] = @ + 1]
    /\ hpc' = [hpc EXCEPT ![h] = "finish"]
    /\ UNCHANGED <<hstate, hjob, cpc, mine, round, queued>>

\* Done: the caller is woken.
Finish(h) ==
    /\ hpc[h] = "finish"
    /\ hstate' = [hstate EXCEPT ![h] = "done"]
    /\ WakeCaller(h)
    /\ hpc' = [hpc EXCEPT ![h] = "wait"]
    /\ UNCHANGED <<hjob, cpc, mine, round, ran>>

Helper(h) == Sleep(h) \/ Take(h) \/ Run(h) \/ Finish(h)

\* A wake-up for no reason: the thread takes itself off the table.
Spurious(t) ==
    /\ t \in queued
    /\ queued' = queued \ {t}
    /\ UNCHANGED <<hstate, hjob, hpc, cpc, mine, round, ran>>

----------------------------------------------------------------------------

Next ==
    \/ \E c \in Callers : Caller(c)
    \/ \E h \in Helpers : Helper(h)
    \/ \E t \in Threads : Spurious(t)

Spec ==
    /\ Init /\ [][Next]_vars
    /\ \A c \in Callers : WF_vars(Caller(c))
    /\ \A h \in Helpers : WF_vars(Helper(h))

\* Each join runs the other half once: never twice, and once before it
\* returns.
RunsOnce ==
    \A c \in Callers :
        /\ ran[c] <= 1
        /\ cpc[c] \in {"release", "end"} => ran[c] = 1

\* A helper runs a half only while its caller is in that join, waiting
\* or running its own half: the half's stack frame is still there.
NoUseAfterReturn ==
    \A h \in Helpers :
        hpc[h] = "run" =>
            LET c == hjob[h][1] IN
            /\ hjob[h][2] = round[c]
            /\ mine[c] = h
            /\ cpc[c] \in {"own", "steal", "wait"}

\* A helper that is not idle serves the one caller that claimed it.
OneCaller ==
    \A h \in Helpers :
        hstate[h] # "idle" =>
            /\ hjob[h] # None
            /\ mine[hjob[h][1]] = h
            /\ \A c \in Callers : mine[c] = h => c = hjob[h][1]

\* Every join returns.
EveryJoinEnds == \A c \in Callers : <>(cpc[c] = "end")

=============================================================================
