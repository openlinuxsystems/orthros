\* SPDX-FileCopyrightText: 2026 The OrthrOS contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

------------------------------ MODULE Channel ------------------------------
(***************************************************************************)
(* A model of how the two ends of a shared-memory channel wake each other *)
(* (userspace/libradix/src/channel.rs: `send`, `recv`, `sleep_while` and  *)
(* `wake_other`).                                                          *)
(*                                                                         *)
(* Each end sends on one queue and receives from the other. An end that    *)
(* finds its queue full (to send) or the other empty (to receive) sets the *)
(* queue's writer or reader `sleeping` word, looks again, and only then    *)
(* waits on its notification; it clears the word once it is done waiting. *)
(* After moving an index (a message written or read), an end signals the  *)
(* other's notification only if the other's word on that queue is set:   *)
(* the reader's after a write, the writer's after a read. A notification   *)
(* keeps a signal until a wait takes it. The code makes the four accesses *)
(* this rests on (the word's store and the second look; the index's store *)
(* and the word's load) sequentially consistent, so the model's            *)
(* interleaving of single accesses covers every execution.                 *)
(*                                                                         *)
(* Each end runs a script of sends and receives, chosen at the start from  *)
(* `Scripts`, so one run covers ping-pong, bursts that fill the queue, and *)
(* both ends sending first. The checks: no deadlock (TLC's own check: an   *)
(* end never waits for good while the other is waiting too or done), and   *)
(* both ends always finish. Leaving out the second look breaks both.      *)
(***************************************************************************)
EXTENDS Naturals, Sequences

CONSTANTS
    Cap     \* messages a queue holds

Ends == {0, 1}
Other(e) == 1 - e

\* Scripts each end may run, as a pair; every pair can finish when nothing
\* is lost (a script that waits for a message the other end never sends
\* is not a fault of the channel).
Scripts == {
    <<<<"send", "recv", "send", "recv">>, <<"recv", "send", "recv", "send">>>>,
    <<<<"send", "send", "send", "recv", "recv", "recv">>, <<"recv", "recv", "recv", "send", "send", "send">>>>,
    <<<<"send", "recv">>, <<"send", "recv">>>>,
    <<<<"send", "send", "recv", "recv">>, <<"recv", "send", "recv", "send">>>>
}

VARIABLES
    script,     \* each end's script
    next,       \* the index in its script of the operation it is on
    pc,         \* where it is in that operation
    queue,      \* messages in the queue each end sends on
    sleeping,   \* the operation each end has said it may sleep in, or "none"
    signal      \* each end's notification holds a signal

vars == <<script, next, pc, queue, sleeping, signal>>

Steps == {"top", "look", "say", "again", "wait", "unsay", "move", "wake", "notify", "done"}

TypeOK ==
    /\ \A e \in Ends : next[e] \in 1..(Len(script[e]) + 1)
    /\ pc \in [Ends -> Steps]
    /\ queue \in [Ends -> 0..Cap]
    /\ sleeping \in [Ends -> {"none", "send", "recv"}]
    /\ signal \in [Ends -> BOOLEAN]

Init ==
    /\ \E pair \in Scripts : script = [e \in Ends |-> pair[e + 1]]
    /\ next = [e \in Ends |-> 1]
    /\ pc = [e \in Ends |-> "top"]
    /\ queue = [e \in Ends |-> 0]
    /\ sleeping = [e \in Ends |-> "none"]
    /\ signal = [e \in Ends |-> FALSE]

Op(e) == script[e][next[e]]

\* Whether end e cannot go on with its operation: no room to send, or
\* nothing to receive.
Blocked(e) ==
    IF Op(e) = "send" THEN queue[e] = Cap ELSE queue[Other(e)] = 0

Go(e, step) == pc' = [pc EXCEPT ![e] = step]

Top(e) ==
    /\ pc[e] = "top"
    /\ Go(e, IF next[e] > Len(script[e]) THEN "done" ELSE "look")
    /\ UNCHANGED <<script, next, queue, sleeping, signal>>

\* sleep_while: look, and if blocked say so, look again, and wait.
Look(e) ==
    /\ pc[e] = "look"
    /\ Go(e, IF Blocked(e) THEN "say" ELSE "move")
    /\ UNCHANGED <<script, next, queue, sleeping, signal>>

Say(e) ==
    /\ pc[e] = "say"
    /\ sleeping' = [sleeping EXCEPT ![e] = Op(e)]
    /\ Go(e, "again")
    /\ UNCHANGED <<script, next, queue, signal>>

Again(e) ==
    /\ pc[e] = "again"
    /\ Go(e, IF Blocked(e) THEN "wait" ELSE "unsay")
    /\ UNCHANGED <<script, next, queue, sleeping, signal>>

\* The wait returns once a signal is there, and takes it.
Wait(e) ==
    /\ pc[e] = "wait"
    /\ signal[e]
    /\ signal' = [signal EXCEPT ![e] = FALSE]
    /\ Go(e, "unsay")
    /\ UNCHANGED <<script, next, queue, sleeping>>

Unsay(e) ==
    /\ pc[e] = "unsay"
    /\ sleeping' = [sleeping EXCEPT ![e] = "none"]
    /\ Go(e, "look")
    /\ UNCHANGED <<script, next, queue, signal>>

\* Write a message and move the tail, or read one and move the head.
Move(e) ==
    /\ pc[e] = "move"
    /\ queue' = IF Op(e) = "send"
                THEN [queue EXCEPT ![e] = @ + 1]
                ELSE [queue EXCEPT ![Other(e)] = @ - 1]
    /\ Go(e, "wake")
    /\ UNCHANGED <<script, next, sleeping, signal>>

\* wake_other: signal the other end only if it says it may sleep waiting
\* for what this end has just done: a message after a send, room after a
\* receive.
Wake(e) ==
    /\ pc[e] = "wake"
    /\ IF sleeping[Other(e)] = (IF Op(e) = "send" THEN "recv" ELSE "send")
       THEN /\ Go(e, "notify")
            /\ UNCHANGED next
       ELSE /\ Go(e, "top")
            /\ next' = [next EXCEPT ![e] = @ + 1]
    /\ UNCHANGED <<script, queue, sleeping, signal>>

Notify(e) ==
    /\ pc[e] = "notify"
    /\ signal' = [signal EXCEPT ![Other(e)] = TRUE]
    /\ Go(e, "top")
    /\ next' = [next EXCEPT ![e] = @ + 1]
    /\ UNCHANGED <<script, queue, sleeping>>

End(e) ==
    Top(e) \/ Look(e) \/ Say(e) \/ Again(e) \/ Wait(e) \/ Unsay(e) \/ Move(e) \/ Wake(e)
    \/ Notify(e)

Finished == \A e \in Ends : pc[e] = "done"

Next ==
    \/ \E e \in Ends : End(e)
    \/ Finished /\ UNCHANGED vars

Spec == Init /\ [][Next]_vars /\ \A e \in Ends : WF_vars(End(e))

\* Both ends get through their scripts.
BothFinish == <>Finished

=============================================================================
