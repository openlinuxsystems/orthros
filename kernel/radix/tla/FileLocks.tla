\* SPDX-FileCopyrightText: 2026 The Dukovo contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

----------------------------- MODULE FileLocks ------------------------------
(***************************************************************************)
(* A model of how a file lock waits for the locks in its way               *)
(* (kernel/radix/src/fs/locks.rs: `lock`, `Table::set`, `Table::release`   *)
(* and `changed`), with the wait table's `wait_until` and `wake_all`       *)
(* (sched/wait.rs) and the scheduler's `block` and `wake` beneath them.    *)
(*                                                                         *)
(* Each process takes an exclusive lock on one file, holds it, and lets it *)
(* go, `Rounds` times; which file each locks is chosen at the start, so    *)
(* one run checks every choice. A process reads `CHANGES`, then, with the  *)
(* table locked, takes the lock if nothing is in its way. If something is, *)
(* it waits on its file's key until `CHANGES` moves: its wait locks the    *)
(* wait table and either finds `CHANGES` moved and tries again, or queues  *)
(* itself and sleeps, and a wake before the sleep makes the sleep return   *)
(* at once. After a wake it takes itself off the table and checks again.   *)
(* Letting a lock go, with the table locked, moves `CHANGES` and wakes the *)
(* file's key, and only then lets the table go. `CHANGES` is shared by     *)
(* every file, so a release of one file's lock makes the waiters of        *)
(* another try again when they next check, though it wakes none of them.   *)
(*                                                                         *)
(* Each critical section of the wait table's lock, and of the lock table's *)
(* outside a release, is one step; a release holds the lock table across   *)
(* two. `CHANGES` moves by a read-modify-write and is read with acquire,   *)
(* so the model's interleaving covers every execution. Shared locks,       *)
(* ranges and the check for deadlocks between processes are                *)
(* `radix_vfs::locks`'s, which the host tests check; signals end a wait    *)
(* sooner, by the same condition. They are left out.                       *)
(*                                                                         *)
(* NoLostWakeup says a process never sleeps while its file's lock is free, *)
(* unless a release of it is still on its way to wake it; EveryLockIsTaken *)
(* says every process takes its lock every time. NoLostWakeup fails if a   *)
(* process reads `CHANGES` after it looks at the table, or if a release    *)
(* moves `CHANGES` before the lock goes, outside the table's lock, or      *)
(* wakes the file's key before it moves `CHANGES`.                         *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Procs,      \* processes taking locks
    Files,      \* the files they lock
    Rounds      \* how many times each takes its lock

VARIABLES
    file,       \* the file each process locks, chosen at the start
    holder,     \* each file's lock holder, or "none"
    owner,      \* the process holding the lock table across a release, or "none"
    changes,    \* CHANGES
    queued,     \* the processes queued in the wait table, each on its file's key
    asleep,     \* the scheduler has the process blocked
    pending,    \* the scheduler holds a wake for its next block
    seen,       \* the CHANGES each process read last
    pc,         \* where each process is
    rounds      \* how many times each has taken its lock

vars == <<file, holder, owner, changes, queued, asleep, pending, seen, pc, rounds>>

Steps == {"seen", "try", "wait", "block", "asleep", "woken", "hold", "wake", "done"}
None == "none"

TypeOK ==
    /\ file \in [Procs -> Files]
    /\ holder \in [Files -> Procs \cup {None}]
    /\ owner \in Procs \cup {None}
    /\ changes \in Nat
    /\ queued \subseteq Procs
    /\ asleep \in [Procs -> BOOLEAN]
    /\ pending \in [Procs -> BOOLEAN]
    /\ seen \in [Procs -> Nat]
    /\ pc \in [Procs -> Steps]
    /\ rounds \in [Procs -> 0..Rounds]

Init ==
    /\ file \in [Procs -> Files]
    /\ holder = [f \in Files |-> None]
    /\ owner = None
    /\ changes = 0
    /\ queued = {}
    /\ asleep = [p \in Procs |-> FALSE]
    /\ pending = [p \in Procs |-> FALSE]
    /\ seen = [p \in Procs |-> 0]
    /\ pc = [p \in Procs |-> "seen"]
    /\ rounds = [p \in Procs |-> 0]

----------------------------------------------------------------------------
(* A process (`lock`).                                                     *)

Seen(p) ==
    /\ pc[p] = "seen"
    /\ seen' = [seen EXCEPT ![p] = changes]
    /\ pc' = [pc EXCEPT ![p] = "try"]
    /\ UNCHANGED <<file, holder, owner, changes, queued, asleep, pending, rounds>>

\* With the lock table locked: the lock, if nothing is in its way.
Try(p) ==
    /\ pc[p] = "try" /\ owner = None
    /\ IF holder[file[p]] = None
       THEN /\ holder' = [holder EXCEPT ![file[p]] = p]
            /\ pc' = [pc EXCEPT ![p] = "hold"]
       ELSE /\ pc' = [pc EXCEPT ![p] = "wait"]
            /\ UNCHANGED holder
    /\ UNCHANGED <<file, owner, changes, queued, asleep, pending, seen, rounds>>

\* `wait_until`'s critical section: the condition, then the queue.
Wait(p) ==
    /\ pc[p] = "wait"
    /\ IF changes # seen[p]
       THEN /\ pc' = [pc EXCEPT ![p] = "seen"]
            /\ UNCHANGED queued
       ELSE /\ queued' = queued \cup {p}
            /\ pc' = [pc EXCEPT ![p] = "block"]
    /\ UNCHANGED <<file, holder, owner, changes, asleep, pending, seen, rounds>>

Block(p) ==
    /\ pc[p] = "block"
    /\ IF pending[p]
       THEN /\ pending' = [pending EXCEPT ![p] = FALSE]
            /\ pc' = [pc EXCEPT ![p] = "woken"]
            /\ UNCHANGED asleep
       ELSE /\ asleep' = [asleep EXCEPT ![p] = TRUE]
            /\ pc' = [pc EXCEPT ![p] = "asleep"]
            /\ UNCHANGED pending
    /\ UNCHANGED <<file, holder, owner, changes, queued, seen, rounds>>

Wake(p) ==
    /\ pc[p] = "asleep" /\ ~asleep[p]
    /\ pc' = [pc EXCEPT ![p] = "woken"]
    /\ UNCHANGED <<file, holder, owner, changes, queued, asleep, pending, seen, rounds>>

\* Off the wait table (if `wake_all` has not taken it off), then the
\* condition again.
Woken(p) ==
    /\ pc[p] = "woken"
    /\ queued' = queued \ {p}
    /\ pc' = [pc EXCEPT ![p] = "wait"]
    /\ UNCHANGED <<file, holder, owner, changes, asleep, pending, seen, rounds>>

\* `Table::set` letting go, with the lock table locked: the lock goes and
\* `CHANGES` moves.
Release(p) ==
    /\ pc[p] = "hold" /\ owner = None
    /\ holder' = [holder EXCEPT ![file[p]] = None]
    /\ owner' = p
    /\ changes' = changes + 1
    /\ pc' = [pc EXCEPT ![p] = "wake"]
    /\ UNCHANGED <<file, queued, asleep, pending, seen, rounds>>

\* `wake_all` on the file's key, the lock table still locked; then it
\* goes.
WakeAll(p) ==
    /\ pc[p] = "wake"
    /\ LET out == {q \in queued : file[q] = file[p]}
       IN /\ asleep' = [q \in Procs |-> IF q \in out THEN FALSE ELSE asleep[q]]
          /\ pending' = [q \in Procs |->
                             IF q \in out /\ ~asleep[q] THEN TRUE ELSE pending[q]]
          /\ queued' = queued \ out
    /\ owner' = None
    /\ rounds' = [rounds EXCEPT ![p] = rounds[p] + 1]
    /\ pc' = [pc EXCEPT ![p] = IF rounds[p] + 1 = Rounds THEN "done" ELSE "seen"]
    /\ UNCHANGED <<file, holder, changes, seen>>

Proc(p) ==
    Seen(p) \/ Try(p) \/ Wait(p) \/ Block(p) \/ Wake(p) \/ Woken(p) \/ Release(p)
    \/ WakeAll(p)

----------------------------------------------------------------------------

Done == \A p \in Procs : pc[p] = "done"

Next ==
    \/ \E p \in Procs : Proc(p)
    \/ Done /\ UNCHANGED vars

Spec ==
    /\ Init /\ [][Next]_vars
    /\ \A p \in Procs : WF_vars(Proc(p))

\* Asleep, a process is queued, and its file's lock is held, or a release
\* of it is still to wake it.
NoLostWakeup ==
    \A p \in Procs :
        asleep[p] =>
            /\ p \in queued
            /\ \/ holder[file[p]] # None
               \/ \E q \in Procs : pc[q] = "wake" /\ file[q] = file[p]

\* The holder of a file's lock is the process that took it.
HolderHolds ==
    \A f \in Files : holder[f] # None => pc[holder[f]] = "hold" /\ file[holder[f]] = f

\* Every process takes its lock every time.
EveryLockIsTaken == \A p \in Procs : <>(pc[p] = "done")

=============================================================================
