\* SPDX-FileCopyrightText: 2026 The Dukovo contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

------------------------------- MODULE Futex -------------------------------
(***************************************************************************)
(* A model of futex waits and wakes on one word                            *)
(* (kernel/radix/src/user/memory/futex.rs: `wait` and `wake`), with the    *)
(* scheduler's `block` and `wake` beneath them (sched/mod.rs).             *)
(*                                                                         *)
(* A waiter locks the word's bucket, compares the word with what it        *)
(* expects (0), and if they match clears its flag and joins the list, in   *)
(* arrival order, before it lets the lock go. Then, until its flag is set  *)
(* or it is interrupted (a signal, a stop or its deadline), it sleeps. An  *)
(* interrupted waiter that finds its flag clear locks the bucket again and *)
(* takes itself off the list; if a waker took it off first, that counts as *)
(* its wake-up.                                                            *)
(*                                                                         *)
(* A waker writes the word (1), then locks the bucket and takes up to      *)
(* `Batch` waiters off the front of the list, setting each one's flag, and *)
(* lets the lock go before it wakes their threads; it goes back for more   *)
(* until it has woken `Count` or the list is empty.                        *)
(*                                                                         *)
(* Each critical section of the bucket's lock is one step here.            *)
(* The scheduler's `wake` of a thread that is not asleep notes the wake as *)
(* pending, and the thread's next `block` then returns at once; both run   *)
(* under the thread's own lock, so each is one step here. A signal sets    *)
(* the thread's interrupted flag and then wakes it, which is one step too: *)
(* a waiter that reads the flag in between gets the pending wake.          *)
(*                                                                         *)
(* NoLostWakeup says a waiter never sleeps with its flag set unless its    *)
(* waker has yet to wake it; WakesCount says a wait ends as woken exactly  *)
(* when a waker took it off the list, so no wake is spent on a waiter that *)
(* reports a signal; EveryWaitEnds says every wait ends, which holds       *)
(* because the wakers wake as many as there are waiters. NoLostWakeup      *)
(* fails if a woken waiter sleeps again without reading its flag, or if    *)
(* the scheduler forgets a wake that comes before the sleep.               *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Waiters,    \* threads that wait on the word
    Wakers,     \* threads that change the word and wake
    Count,      \* how many each waker wakes at most
    Batch,      \* how many a waker takes off the list at a time
    Signalled   \* waiters that may be interrupted

VARIABLES
    word,       \* the futex word
    list,       \* the bucket's waiters, in arrival order
    flag,       \* each waiter's futex_woken
    taken,      \* whether a waker took the waiter off the list
    intr,       \* the waiter was interrupted
    asleep,     \* the scheduler has the waiter blocked
    pending,    \* the scheduler holds a wake for its next block
    result,     \* how each wait ended
    wpc,        \* where each waiter is
    kpc,        \* where each waker is
    batch,      \* the waiters each waker has taken and not yet woken
    woken,      \* how many each waker has taken
    more        \* whether each waker goes back for more after its batch

vars == <<word, list, flag, taken, intr, asleep, pending, result, wpc, kpc, batch,
          woken, more>>

WaiterSteps == {"lock", "loop", "block", "asleep", "leave", "done"}
WakerSteps == {"store", "lock", "wake", "done"}
Results == {"none", "changed", "woken", "interrupted"}

InList(w) == \E i \in 1..Len(list) : list[i] = w

TypeOK ==
    /\ word \in {0, 1}
    /\ list \in Seq(Waiters)
    /\ flag \in [Waiters -> BOOLEAN]
    /\ taken \in [Waiters -> BOOLEAN]
    /\ intr \in [Waiters -> BOOLEAN]
    /\ asleep \in [Waiters -> BOOLEAN]
    /\ pending \in [Waiters -> BOOLEAN]
    /\ result \in [Waiters -> Results]
    /\ wpc \in [Waiters -> WaiterSteps]
    /\ kpc \in [Wakers -> WakerSteps]
    /\ batch \in [Wakers -> SUBSET Waiters]
    /\ woken \in [Wakers -> 0..Count]
    /\ more \in [Wakers -> BOOLEAN]

Init ==
    /\ word = 0
    /\ list = <<>>
    /\ flag = [w \in Waiters |-> FALSE]
    /\ taken = [w \in Waiters |-> FALSE]
    /\ intr = [w \in Waiters |-> FALSE]
    /\ asleep = [w \in Waiters |-> FALSE]
    /\ pending = [w \in Waiters |-> FALSE]
    /\ result = [w \in Waiters |-> "none"]
    /\ wpc = [w \in Waiters |-> "lock"]
    /\ kpc = [k \in Wakers |-> "store"]
    /\ batch = [k \in Wakers |-> {}]
    /\ woken = [k \in Wakers |-> 0]
    /\ more = [k \in Wakers |-> FALSE]

\* The scheduler's wake: a blocked thread runs, any other holds the wake
\* for its next block.
SchedWake(w) ==
    IF asleep[w]
    THEN /\ asleep' = [asleep EXCEPT ![w] = FALSE]
         /\ UNCHANGED pending
    ELSE /\ pending' = [pending EXCEPT ![w] = TRUE]
         /\ UNCHANGED asleep

----------------------------------------------------------------------------
(* A waiter (`wait`).                                                      *)

\* The critical section: compare, then queue with the flag cleared.
Queue(w) ==
    /\ wpc[w] = "lock"
    /\ IF word # 0
       THEN /\ result' = [result EXCEPT ![w] = "changed"]
            /\ wpc' = [wpc EXCEPT ![w] = "done"]
            /\ UNCHANGED <<list, flag>>
       ELSE /\ flag' = [flag EXCEPT ![w] = FALSE]
            /\ list' = Append(list, w)
            /\ wpc' = [wpc EXCEPT ![w] = "loop"]
            /\ UNCHANGED result
    /\ UNCHANGED <<word, taken, intr, asleep, pending, kpc, batch, woken, more>>

\* `while !done() { block }`, and what follows it.
Look(w) ==
    /\ wpc[w] = "loop"
    /\ IF flag[w]
       THEN /\ result' = [result EXCEPT ![w] = "woken"]
            /\ wpc' = [wpc EXCEPT ![w] = "done"]
       ELSE /\ wpc' = [wpc EXCEPT ![w] = IF intr[w] THEN "leave" ELSE "block"]
            /\ UNCHANGED result
    /\ UNCHANGED <<word, list, flag, taken, intr, asleep, pending, kpc, batch, woken, more>>

Block(w) ==
    /\ wpc[w] = "block"
    /\ IF pending[w]
       THEN /\ pending' = [pending EXCEPT ![w] = FALSE]
            /\ wpc' = [wpc EXCEPT ![w] = "loop"]
            /\ UNCHANGED asleep
       ELSE /\ asleep' = [asleep EXCEPT ![w] = TRUE]
            /\ wpc' = [wpc EXCEPT ![w] = "asleep"]
            /\ UNCHANGED pending
    /\ UNCHANGED <<word, list, flag, taken, intr, result, kpc, batch, woken, more>>

Wake(w) ==
    /\ wpc[w] = "asleep" /\ ~asleep[w]
    /\ wpc' = [wpc EXCEPT ![w] = "loop"]
    /\ UNCHANGED <<word, list, flag, taken, intr, asleep, pending, result, kpc, batch,
                   woken, more>>

\* Interrupted with the flag clear: off the list, unless a waker took it
\* off already, which is then the wait's wake-up.
Leave(w) ==
    /\ wpc[w] = "leave"
    /\ IF InList(w)
       THEN /\ list' = SelectSeq(list, LAMBDA x : x # w)
            /\ result' = [result EXCEPT ![w] = "interrupted"]
       ELSE /\ result' = [result EXCEPT ![w] = "woken"]
            /\ UNCHANGED list
    /\ wpc' = [wpc EXCEPT ![w] = "done"]
    /\ UNCHANGED <<word, flag, taken, intr, asleep, pending, kpc, batch, woken, more>>

Waiter(w) == Queue(w) \/ Look(w) \/ Block(w) \/ Wake(w) \/ Leave(w)

----------------------------------------------------------------------------
(* A waker (`wake`).                                                       *)

Store(k) ==
    /\ kpc[k] = "store"
    /\ word' = 1
    /\ kpc' = [kpc EXCEPT ![k] = "lock"]
    /\ UNCHANGED <<list, flag, taken, intr, asleep, pending, result, wpc, batch, woken,
                   more>>

Min3(a, b, c) == IF a <= b /\ a <= c THEN a ELSE IF b <= c THEN b ELSE c

\* The critical section: take up to `Batch` waiters off the front, flags
\* set; then wake them with the lock let go.
Take(k) ==
    /\ kpc[k] = "lock"
    /\ LET n == Min3(Len(list), Batch, Count - woken[k])
           out == {list[i] : i \in 1..n}
       IN /\ list' = SubSeq(list, n + 1, Len(list))
          /\ flag' = [w \in Waiters |-> IF w \in out THEN TRUE ELSE flag[w]]
          /\ taken' = [w \in Waiters |-> IF w \in out THEN TRUE ELSE taken[w]]
          /\ batch' = [batch EXCEPT ![k] = out]
          /\ woken' = [woken EXCEPT ![k] = woken[k] + n]
          /\ more' = [more EXCEPT ![k] = n = Batch /\ woken[k] + n < Count]
          /\ kpc' = [kpc EXCEPT ![k] = IF n = 0 THEN "done" ELSE "wake"]
    /\ UNCHANGED <<word, intr, asleep, pending, result, wpc>>

WakeOne(k) ==
    /\ kpc[k] = "wake"
    /\ \E w \in batch[k] :
          /\ SchedWake(w)
          /\ batch' = [batch EXCEPT ![k] = batch[k] \ {w}]
          /\ kpc' = [kpc EXCEPT ![k] =
                        IF batch[k] = {w} THEN IF more[k] THEN "lock" ELSE "done" ELSE "wake"]
    /\ UNCHANGED <<word, list, flag, taken, intr, result, wpc, woken, more>>

Waker(k) == Store(k) \/ Take(k) \/ WakeOne(k)

----------------------------------------------------------------------------
(* A signal, a stop or a deadline: the flag, then the scheduler's wake.     *)

Interrupt(w) ==
    /\ w \in Signalled /\ ~intr[w] /\ wpc[w] # "done"
    /\ intr' = [intr EXCEPT ![w] = TRUE]
    /\ SchedWake(w)
    /\ UNCHANGED <<word, list, flag, taken, result, wpc, kpc, batch, woken, more>>

----------------------------------------------------------------------------

Done == (\A w \in Waiters : wpc[w] = "done") /\ (\A k \in Wakers : kpc[k] = "done")

Next ==
    \/ \E w \in Waiters : Waiter(w) \/ Interrupt(w)
    \/ \E k \in Wakers : Waker(k)
    \/ Done /\ UNCHANGED vars

Spec ==
    /\ Init /\ [][Next]_vars
    /\ \A w \in Waiters : WF_vars(Waiter(w))
    /\ \A k \in Wakers : WF_vars(Waker(k))

\* Asleep with the flag set, a waiter is in its waker's batch, still to be
\* woken.
NoLostWakeup ==
    \A w \in Waiters :
        (asleep[w] /\ flag[w]) => \E k \in Wakers : w \in batch[k]

\* A wait ends as woken exactly when a waker took it off the list.
WakesCount ==
    \A w \in Waiters :
        /\ result[w] = "woken" => taken[w]
        /\ result[w] \in {"changed", "interrupted"} => ~taken[w]

\* A waiter on the list has its flag clear, and one taken off it is not
\* on it again.
ListIsCurrent ==
    \A w \in Waiters : InList(w) => (~flag[w] /\ ~taken[w])

\* Every wait ends.
EveryWaitEnds == \A w \in Waiters : <>(wpc[w] = "done")

=============================================================================
