\* SPDX-FileCopyrightText: 2026 The OrthrOS contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

------------------------------ MODULE Endpoint ------------------------------
(***************************************************************************)
(* A model of how Radix endpoints hand messages, calls and replies between *)
(* threads (kernel/radix/src/user/ipc.rs).                                 *)
(*                                                                         *)
(* Callers send messages on one endpoint; each message is a plain send or *)
(* a call, which waits for a reply. Servers receive in a loop and either   *)
(* reply to the call they hold or receive again without replying, which    *)
(* lets the caller go with `gone`. Each step of a thread is one critical   *)
(* section of the code: `hand_over` and `take` run under the endpoint's    *)
(* lock, and change the other thread's IPC state, which that thread reads *)
(* under its own lock when it wakes. A thread asked to stop cancels its    *)
(* wait under the same locks (`cancel`) and exits, and an exiting server  *)
(* lets its caller go (`abandon_caller`). The endpoint may close at any   *)
(* moment: under its lock it empties both queues, and then, outside the    *)
(* lock, fails each thread it took out (`Endpoint::close`). Waking and     *)
(* blocking are left out: the Scheduler model shows that a thread waiting  *)
(* for its state to change is woken once it has.                           *)
(*                                                                         *)
(* The invariants say that a thread is queued exactly when its state says *)
(* so (QueuesMatchStates), that every message reaches one server at most  *)
(* (AtMostOnce), that a send which returned success reached a server and  *)
(* a reply came from the server that took the call (Delivered), that a    *)
(* held call belongs to a caller still waiting for that very call         *)
(* (HeldCallsWait), and that one call is held by one server at most       *)
(* (OneHolder). The liveness property AllFinish says that every caller    *)
(* and server finishes, given fair scheduling. docs/design/ipc.md maps    *)
(* the model to the code.                                                  *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Callers,    \* threads that send and call
    Servers,    \* threads that receive and reply
    Rounds,     \* how many messages each caller sends
    Stoppable,  \* the threads that may be asked to stop
    EarlyClose, \* whether the endpoint may close while callers still run
    None        \* no message

ASSUME Rounds \in Nat \ {0}
ASSUME Stoppable \subseteq Callers \cup Servers
ASSUME EarlyClose \in BOOLEAN

Threads == Callers \cup Servers

\* A message names its sender and its round, and says whether it is a call.
Msgs == [from : Callers, round : 1..Rounds, call : BOOLEAN]

\* A thread's IPC state (`IpcState`), with the message it is about.
Kinds == {"idle", "sending", "receiving", "awaiting", "taken", "delivered", "failed"}
St(kind, m) == [kind |-> kind, msg |-> m]
Idle == St("idle", None)

Outcomes == {"none", "taken", "reply", "gone", "interrupted"}
Done == {"done", "exited"}
CallerPcs == {"start", "send_wait", "reply_wait"} \cup Done
ServerPcs == {"recv", "take", "wait", "serve"} \cup Done

VARIABLES
    senders,    \* the endpoint's queue of senders' messages
    receivers,  \* its queue of receivers
    closed,     \* whether its last capability is gone
    failing,    \* threads a close took out of a queue and has yet to fail
    st,         \* each thread's IPC state
    pc,         \* where each thread is
    round,      \* each caller's current message
    holding,    \* the call each server holds for its reply (`reply`)
    stopped,    \* threads asked to stop
    received,   \* how many times each message was handed to a server
    replied,    \* the calls whose caller got its reply
    outcome     \* what each of a caller's sends returned

vars == <<senders, receivers, closed, failing, st, pc, round, holding, stopped,
          received, replied, outcome>>

Range(seq) == {seq[i] : i \in 1..Len(seq)}
Remove(seq, x) == SelectSeq(seq, LAMBDA y : y # x)

\* The message caller `c` is sending now.
Current(c, call) == [from |-> c, round |-> round[c], call |-> call]

TypeOK ==
    /\ senders \in Seq(Msgs)
    /\ receivers \in Seq(Servers)
    /\ closed \in BOOLEAN
    /\ failing \subseteq Threads
    /\ st \in [Threads -> [kind : Kinds, msg : Msgs \cup {None}]]
    /\ pc \in [Threads -> CallerPcs \cup ServerPcs]
    /\ \A c \in Callers : pc[c] \in CallerPcs
    /\ \A s \in Servers : pc[s] \in ServerPcs
    /\ round \in [Callers -> 1..Rounds]
    /\ holding \in [Servers -> Msgs \cup {None}]
    /\ stopped \subseteq Stoppable
    /\ received \in [Msgs -> 0..2]
    /\ replied \subseteq Msgs
    /\ outcome \in [Callers -> [1..Rounds -> Outcomes]]

Init ==
    /\ senders = <<>>
    /\ receivers = <<>>
    /\ closed = FALSE
    /\ failing = {}
    /\ st = [t \in Threads |-> Idle]
    /\ pc = [t \in Threads |-> IF t \in Callers THEN "start" ELSE "recv"]
    /\ round = [c \in Callers |-> 1]
    /\ holding = [s \in Servers |-> None]
    /\ stopped = {}
    /\ received = [m \in Msgs |-> 0]
    /\ replied = {}
    /\ outcome = [c \in Callers |-> [r \in 1..Rounds |-> "none"]]

\* Caller `c`'s current send returns `result`; it goes on to the next.
Finish(c, result) ==
    /\ outcome' = [outcome EXCEPT ![c][round[c]] = result]
    /\ IF round[c] = Rounds
       THEN pc' = [pc EXCEPT ![c] = "done"] /\ UNCHANGED round
       ELSE pc' = [pc EXCEPT ![c] = "start"] /\ round' = [round EXCEPT ![c] = @ + 1]

(***************************************************************************)
(* Callers: `send` and `cap_call`.                                         *)
(***************************************************************************)

\* `hand_over`, under the endpoint's lock: to the first queued receiver,
\* or into the queue of senders.
HandOver(c) ==
    /\ pc[c] = "start"
    /\ \E call \in BOOLEAN :
        LET m == Current(c, call) IN
        \/ /\ closed
           /\ Finish(c, "gone")
           /\ UNCHANGED <<senders, receivers, closed, failing, st, holding, stopped,
                          received, replied>>
        \/ /\ ~closed
           /\ receivers # <<>>
           /\ receivers' = Tail(receivers)
           /\ received' = [received EXCEPT ![m] = @ + 1]
           /\ st' = [st EXCEPT ![Head(receivers)] = St("delivered", m),
                               ![c] = IF call THEN St("awaiting", m) ELSE @]
           /\ IF call
              THEN pc' = [pc EXCEPT ![c] = "reply_wait"] /\ UNCHANGED <<round, outcome>>
              ELSE Finish(c, "taken")
           /\ UNCHANGED <<senders, closed, failing, holding, stopped, replied>>
        \/ /\ ~closed
           /\ receivers = <<>>
           /\ senders' = Append(senders, m)
           /\ st' = [st EXCEPT ![c] = St("sending", m)]
           /\ pc' = [pc EXCEPT ![c] = "send_wait"]
           /\ UNCHANGED <<receivers, closed, failing, round, holding, stopped, received,
                          replied, outcome>>

\* A queued sender finds its state settled: taken, its call taken (now the
\* reply), already answered, or failed.
SendWait(c) ==
    /\ pc[c] = "send_wait"
    /\ st[c].kind \in {"taken", "awaiting", "delivered", "failed"}
    /\ IF st[c].kind = "awaiting"
       THEN pc' = [pc EXCEPT ![c] = "reply_wait"] /\ UNCHANGED <<st, round, outcome>>
       ELSE /\ st' = [st EXCEPT ![c] = Idle]
            /\ Finish(c, CASE st[c].kind = "taken" -> "taken"
                           [] st[c].kind = "delivered" -> "reply"
                           [] OTHER -> "gone")
    /\ UNCHANGED <<senders, receivers, closed, failing, holding, stopped, received, replied>>

\* A caller finds its reply, or `gone`.
ReplyWait(c) ==
    /\ pc[c] = "reply_wait"
    /\ st[c].kind \in {"delivered", "failed"}
    /\ st' = [st EXCEPT ![c] = Idle]
    /\ Finish(c, IF st[c].kind = "delivered" THEN "reply" ELSE "gone")
    /\ UNCHANGED <<senders, receivers, closed, failing, holding, stopped, received, replied>>

\* `cancel` of a stopped sender, under the endpoint's lock: only while it
\* is still queued.
CancelSend(c) ==
    /\ pc[c] = "send_wait"
    /\ c \in stopped
    /\ st[c].kind = "sending"
    /\ st[c].msg \in Range(senders)
    /\ senders' = Remove(senders, st[c].msg)
    /\ st' = [st EXCEPT ![c] = Idle]
    /\ outcome' = [outcome EXCEPT ![c][round[c]] = "interrupted"]
    /\ pc' = [pc EXCEPT ![c] = "exited"]
    /\ UNCHANGED <<receivers, closed, failing, round, holding, stopped, received, replied>>

\* `cancel` of a stopped caller, under its own lock: only while it still
\* waits for the reply, which the replier then skips.
CancelReply(c) ==
    /\ pc[c] = "reply_wait"
    /\ c \in stopped
    /\ st[c].kind = "awaiting"
    /\ st' = [st EXCEPT ![c] = Idle]
    /\ outcome' = [outcome EXCEPT ![c][round[c]] = "interrupted"]
    /\ pc' = [pc EXCEPT ![c] = "exited"]
    /\ UNCHANGED <<senders, receivers, closed, failing, round, holding, stopped, received,
                   replied>>

\* A stopped caller between system calls exits.
CallerExit(c) ==
    /\ pc[c] = "start"
    /\ c \in stopped
    /\ pc' = [pc EXCEPT ![c] = "exited"]
    /\ UNCHANGED <<senders, receivers, closed, failing, st, round, holding, stopped,
                   received, replied, outcome>>

(***************************************************************************)
(* Servers: `recv`, `reply_recv` and `reply`.                              *)
(***************************************************************************)

\* `abandon_caller`: a server that receives again, or exits, without
\* replying lets its caller go, if the caller still waits.
LetGo(s) ==
    IF holding[s] # None /\ st[holding[s].from].kind = "awaiting"
    THEN st' = [st EXCEPT ![holding[s].from] = St("failed", holding[s])]
    ELSE UNCHANGED st

\* `recv` begins: let the held caller go.
Abandon(s) ==
    /\ pc[s] = "recv"
    /\ LetGo(s)
    /\ holding' = [holding EXCEPT ![s] = None]
    /\ pc' = [pc EXCEPT ![s] = "take"]
    /\ UNCHANGED <<senders, receivers, closed, failing, round, stopped, received, replied,
                   outcome>>

\* `take`, under the endpoint's lock: the first queued message, or into
\* the queue of receivers. A closed endpoint ends the server.
Take(s) ==
    /\ pc[s] = "take"
    /\ \/ /\ closed
          /\ pc' = [pc EXCEPT ![s] = "done"]
          /\ UNCHANGED <<senders, receivers, st, holding, received>>
       \/ /\ ~closed
          /\ senders # <<>>
          /\ LET m == Head(senders) IN
             /\ senders' = Tail(senders)
             /\ received' = [received EXCEPT ![m] = @ + 1]
             /\ st' = [st EXCEPT ![m.from] = IF m.call THEN St("awaiting", m)
                                                       ELSE St("taken", m)]
             /\ holding' = IF m.call THEN [holding EXCEPT ![s] = m] ELSE holding
             /\ pc' = [pc EXCEPT ![s] = IF m.call THEN "serve" ELSE "recv"]
          /\ UNCHANGED receivers
       \/ /\ ~closed
          /\ senders = <<>>
          /\ receivers' = Append(receivers, s)
          /\ st' = [st EXCEPT ![s] = St("receiving", None)]
          /\ pc' = [pc EXCEPT ![s] = "wait"]
          /\ UNCHANGED <<senders, holding, received>>
    /\ UNCHANGED <<closed, failing, round, stopped, replied, outcome>>

\* A queued receiver finds a message, or `gone`.
RecvWait(s) ==
    /\ pc[s] = "wait"
    /\ st[s].kind \in {"delivered", "failed"}
    /\ st' = [st EXCEPT ![s] = Idle]
    /\ IF st[s].kind = "failed"
       THEN pc' = [pc EXCEPT ![s] = "done"] /\ UNCHANGED holding
       ELSE IF st[s].msg.call
            THEN /\ holding' = [holding EXCEPT ![s] = st[s].msg]
                 /\ pc' = [pc EXCEPT ![s] = "serve"]
            ELSE pc' = [pc EXCEPT ![s] = "recv"] /\ UNCHANGED holding
    /\ UNCHANGED <<senders, receivers, closed, failing, round, stopped, received, replied,
                   outcome>>

\* `cancel` of a stopped receiver, while it is still queued.
CancelRecv(s) ==
    /\ pc[s] = "wait"
    /\ s \in stopped
    /\ st[s].kind = "receiving"
    /\ s \in Range(receivers)
    /\ receivers' = Remove(receivers, s)
    /\ st' = [st EXCEPT ![s] = Idle]
    /\ pc' = [pc EXCEPT ![s] = "exited"]
    /\ UNCHANGED <<senders, closed, failing, round, holding, stopped, received, replied,
                   outcome>>

\* `reply` (also the first half of `reply_recv`): `deliver_reply` checks,
\* under the caller's lock, that it still waits for a reply.
Reply(s) ==
    /\ pc[s] = "serve"
    /\ LET m == holding[s] IN
       IF st[m.from].kind = "awaiting"
       THEN /\ st' = [st EXCEPT ![m.from] = St("delivered", m)]
            /\ replied' = replied \cup {m}
       ELSE UNCHANGED <<st, replied>>
    /\ holding' = [holding EXCEPT ![s] = None]
    /\ pc' = [pc EXCEPT ![s] = "recv"]
    /\ UNCHANGED <<senders, receivers, closed, failing, round, stopped, received, outcome>>

\* Or the server receives again without replying.
NoReply(s) ==
    /\ pc[s] = "serve"
    /\ pc' = [pc EXCEPT ![s] = "recv"]
    /\ UNCHANGED <<senders, receivers, closed, failing, st, round, holding, stopped,
                   received, replied, outcome>>

\* A stopped server that is not queued exits, letting its caller go
\* (`exiting`).
ServerExit(s) ==
    /\ pc[s] \in {"recv", "serve"}
    /\ s \in stopped
    /\ LetGo(s)
    /\ holding' = [holding EXCEPT ![s] = None]
    /\ pc' = [pc EXCEPT ![s] = "exited"]
    /\ UNCHANGED <<senders, receivers, closed, failing, round, stopped, received, replied,
                   outcome>>

(***************************************************************************)
(* The endpoint and the rest of the system.                               *)
(***************************************************************************)

\* `Endpoint::close`, under the endpoint's lock.
Close ==
    /\ ~closed
    /\ EarlyClose \/ \A c \in Callers : pc[c] \in Done
    /\ closed' = TRUE
    /\ failing' = {m.from : m \in Range(senders)} \cup Range(receivers)
    /\ senders' = <<>>
    /\ receivers' = <<>>
    /\ UNCHANGED <<st, pc, round, holding, stopped, received, replied, outcome>>

\* Then, outside the lock, each thread it took out gets `gone`.
Fail(t) ==
    /\ t \in failing
    /\ st' = [st EXCEPT ![t] = St("failed", st[t].msg)]
    /\ failing' = failing \ {t}
    /\ UNCHANGED <<senders, receivers, closed, pc, round, holding, stopped, received,
                   replied, outcome>>

\* A thread is asked to stop (the out-of-memory policy, or its program's
\* end).
Stop(t) ==
    /\ t \in Stoppable \ stopped
    /\ pc[t] \notin Done
    /\ stopped' = stopped \cup {t}
    /\ UNCHANGED <<senders, receivers, closed, failing, st, pc, round, holding, received,
                   replied, outcome>>

\* Everything has finished.
Finished ==
    /\ \A t \in Threads : pc[t] \in Done
    /\ failing = {}
    /\ UNCHANGED vars

CallerStep(c) ==
    HandOver(c) \/ SendWait(c) \/ ReplyWait(c) \/ CancelSend(c) \/ CancelReply(c)
        \/ CallerExit(c)

ServerStep(s) ==
    Abandon(s) \/ Take(s) \/ RecvWait(s) \/ CancelRecv(s) \/ Reply(s) \/ NoReply(s)
        \/ ServerExit(s)

Next ==
    \/ \E c \in Callers : CallerStep(c)
    \/ \E s \in Servers : ServerStep(s)
    \/ Close
    \/ \E t \in Threads : Fail(t) \/ Stop(t)
    \/ Finished

\* Every thread that can take a step takes it eventually, and the endpoint
\* closes once the callers are done. A server may skip replies forever,
\* which only lets its callers go. Stopping is not fair: it may never come.
Fairness ==
    /\ \A c \in Callers : WF_vars(CallerStep(c))
    /\ \A s \in Servers : WF_vars(ServerStep(s))
    /\ WF_vars(Close)
    /\ \A t \in Threads : WF_vars(Fail(t))

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* Properties.                                                             *)
(***************************************************************************)

\* A thread is queued exactly while its state says so, once at most; a
\* close takes it out before failing it.
QueuesMatchStates ==
    /\ \A i, j \in 1..Len(senders) : i # j => senders[i].from # senders[j].from
    /\ \A i, j \in 1..Len(receivers) : i # j => receivers[i] # receivers[j]
    /\ \A m \in Range(senders) : st[m.from] = St("sending", m) /\ pc[m.from] = "send_wait"
    /\ \A s \in Range(receivers) : st[s].kind = "receiving" /\ pc[s] = "wait"
    /\ \A c \in Callers : st[c].kind = "sending" => st[c].msg \in Range(senders) \/ c \in failing
    /\ \A s \in Servers : st[s].kind = "receiving" => s \in Range(receivers) \/ s \in failing

\* No message reaches two servers, or one twice.
AtMostOnce == \A m \in Msgs : received[m] <= 1

\* A send that returned success reached a server; a call that got its
\* reply reached a server, which replied to it.
Delivered ==
    \A c \in Callers, r \in 1..Rounds :
        LET send == [from |-> c, round |-> r, call |-> FALSE]
            call == [from |-> c, round |-> r, call |-> TRUE] IN
        /\ outcome[c][r] = "taken" => received[send] = 1
        /\ outcome[c][r] = "reply" => received[call] = 1 /\ call \in replied

\* A server holds a call only while its caller waits for that very call,
\* or has exited: `deliver_reply` and `abandon_caller`, which check only
\* that the caller is waiting, never reach a later call of the same caller.
HeldCallsWait ==
    \A s \in Servers : holding[s] # None =>
        LET m == holding[s] IN
        \/ /\ st[m.from] = St("awaiting", m)
           /\ pc[m.from] \in {"send_wait", "reply_wait"}
           /\ round[m.from] = m.round
        \/ pc[m.from] = "exited"

\* The state of a waiting caller is about its current message.
StateIsCurrent ==
    \A c \in Callers : st[c].kind # "idle" =>
        st[c].msg.round = round[c] /\ pc[c] \in {"send_wait", "reply_wait"}

\* One call is held by one server at most.
OneHolder ==
    \A s1, s2 \in Servers : s1 # s2 /\ holding[s1] # None => holding[s1] # holding[s2]

\* Every caller and server finishes: no wait lasts forever.
AllFinish == <>(\A t \in Threads : pc[t] \in Done)

=============================================================================
