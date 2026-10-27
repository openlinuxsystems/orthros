\* SPDX-FileCopyrightText: 2026 The Dukovo contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

------------------------------ MODULE Commit -------------------------------
(***************************************************************************)
(* A model of how Btrfs commits a transaction to a disk that may lose     *)
(* power at any moment (ADR 0129; kernel/fs/btrfs, the write path).        *)
(*                                                                         *)
(* A filesystem is a tree of blocks: here a root and two leaves, `a` and  *)
(* `b`, each leaf a value. The root holds where each leaf is and its      *)
(* generation, and the superblock where the root is and its generation.   *)
(* A block is never changed where it is once a commit has written it: a   *)
(* transaction copies each block it changes to a free place, and its      *)
(* parent with it, up to the root. The places the copies leave are        *)
(* pinned, not freed, until the superblock that no longer needs them is   *)
(* on the disk. A commit writes the transaction's blocks, flushes the     *)
(* disk's cache, writes the superblock, and waits for it to be durable.   *)
(* While one commit runs, the next transaction goes on changing the tree, *)
(* and may write its blocks early, as memory runs short.                  *)
(*                                                                         *)
(* The disk keeps writes in a cache and makes them durable one by one, in *)
(* any order; a power cut loses those it has not. After a cut, the        *)
(* filesystem is mounted from the superblock on the disk, and every place *)
(* its tree does not use is free.                                         *)
(*                                                                         *)
(* The checks, in every state, as a cut can come in any: the superblock   *)
(* on the disk leads to a whole tree, every block where its parent says,  *)
(* of the generation it says (Linux's \"parent transid\" check), and the  *)
(* tree holds the values of the transaction that superblock committed; a  *)
(* commit that has finished is on the disk, or a later one is. With       *)
(* `FlushFirst` FALSE, TLC finds a superblock durable before the blocks   *)
(* it leads to; with `UnpinLate` FALSE, a place the durable tree still    *)
(* uses free, and without that check, written over by the next            *)
(* transaction; with `CopyWritten` FALSE, a value changed in memory after *)
(* its block was written early, which the commit then does not write      *)
(* (configurations `ob test tla` does not run).                           *)
(***************************************************************************)
EXTENDS Naturals, TLC

CONSTANTS
    Locs,       \* the disk's places for blocks
    Values,     \* what a leaf may hold
    MaxGen,     \* the last generation a transaction may have
    FlushFirst, \* whether a commit flushes its blocks before its superblock
    UnpinLate,  \* whether places a commit freed wait for its superblock
    CopyWritten,\* whether a block written early is copied to be changed
    None        \* nothing (a model value)

Leaves == {"a", "b"}
Blocks == Leaves \cup {"root"}

(* A tree in memory: its transaction's generation `gen`; where its root  *)
(* is and the generation that wrote it; where each leaf is, the          *)
(* generation that wrote it, and its value.                              *)
Trees ==
    [gen : 1..MaxGen + 1, root : Locs, rgen : 1..MaxGen,
     loc : [Leaves -> Locs], lgen : [Leaves -> 1..MaxGen + 1],
     val : [Leaves -> Values]]

VARIABLES
    disk,       \* each place's durable content, or None
    cache,      \* each place's write the disk took but has not made durable
    super,      \* the superblock on the disk: where the root is, and its generation
    superCache, \* a superblock written but not yet durable, or None
    done,       \* the tree of the last commit known to be durable
    committing, \* the commit under way, or None
    run,        \* the running transaction's tree
    written,    \* places of the running transaction's blocks it wrote early
    pinned,     \* places the running transaction's copies left
    free,       \* places free to take
    history     \* each generation's values, as its commit began

vars == <<disk, cache, super, superCache, done, committing, run, written,
          pinned, free, history>>

\* The contents of a tree's blocks.
RootBlock(t) == [kind |-> "root", gen |-> t.rgen, loc |-> t.loc, lgen |-> t.lgen]
LeafBlock(t, n) == [kind |-> n, gen |-> t.lgen[n], val |-> t.val[n]]
Content(t, b) == IF b = "root" THEN RootBlock(t) ELSE LeafBlock(t, b)
Place(t, b) == IF b = "root" THEN t.root ELSE t.loc[b]
Gen(t, b) == IF b = "root" THEN t.rgen ELSE t.lgen[b]
Uses(t) == {t.root} \cup {t.loc[n] : n \in Leaves}

----------------------------------------------------------------------------

Init ==
    \E r, x, y \in Locs :
        /\ r # x /\ r # y /\ x # y
        /\ LET v == CHOOSE v \in Values : TRUE
               t == [gen |-> 1, root |-> r, rgen |-> 1, loc |-> [a |-> x, b |-> y],
                     lgen |-> [n \in Leaves |-> 1], val |-> [n \in Leaves |-> v]]
           IN /\ disk = [l \in Locs |->
                    IF l = r THEN RootBlock(t)
                    ELSE IF l = x THEN LeafBlock(t, "a")
                    ELSE IF l = y THEN LeafBlock(t, "b")
                    ELSE None]
              /\ done = t
              /\ run = [t EXCEPT !.gen = 2]
              /\ history = [g \in 1..MaxGen |-> IF g = 1 THEN t.val ELSE None]
              /\ free = Locs \ {r, x, y}
              /\ super = [root |-> r, gen |-> 1]
    /\ cache = [l \in Locs |-> None]
    /\ superCache = None
    /\ committing = None
    /\ written = {}
    /\ pinned = {}

(* A block of the running transaction that it may still change where it  *)
(* is: copied in this transaction and not written yet.                    *)
Fresh(b) == Gen(run, b) = run.gen /\ (CopyWritten => Place(run, b) \notin written)

----------------------------------------------------------------------------
(* The running transaction *)

\* A leaf takes a new value: where it is, if it is fresh; else it is copied
\* to a free place, and the root too, unless the root is fresh.
Change(n, v) ==
    /\ run.gen <= MaxGen
    /\ IF Fresh(n)
        THEN /\ run' = [run EXCEPT !.val[n] = v]
             /\ UNCHANGED <<free, pinned>>
        ELSE \E x \in free :
            IF Fresh("root")
                THEN /\ run' = [run EXCEPT !.loc[n] = x, !.lgen[n] = run.gen, !.val[n] = v]
                     /\ free' = free \ {x}
                     /\ pinned' = pinned \cup {run.loc[n]}
                ELSE \E y \in free \ {x} :
                     /\ run' = [run EXCEPT !.loc[n] = x, !.lgen[n] = run.gen, !.val[n] = v,
                                           !.root = y, !.rgen = run.gen]
                     /\ free' = free \ {x, y}
                     /\ pinned' = pinned \cup {run.loc[n], run.root}
    /\ UNCHANGED <<disk, cache, super, superCache, done, committing, written, history>>

\* Memory runs short: a block of the running transaction is written before
\* its commit. Changing it again copies it.
WriteEarly(b) ==
    /\ Fresh(b)
    /\ cache' = [cache EXCEPT ![Place(run, b)] = Content(run, b)]
    /\ written' = written \cup {Place(run, b)}
    /\ UNCHANGED <<disk, super, superCache, done, committing, run, pinned, free, history>>

----------------------------------------------------------------------------
(* The commit *)

\* A transaction that changed something commits, once the commit before it
\* is done; the next transaction starts from its tree.
StartCommit ==
    /\ committing = None
    /\ run.gen <= MaxGen
    /\ run.rgen = run.gen
    /\ committing' = [tree |-> run, pinned |-> pinned, phase |-> "write",
                      unwritten |-> {b \in Blocks : Gen(run, b) = run.gen
                                                    /\ Place(run, b) \notin written}]
    /\ history' = [history EXCEPT ![run.gen] = run.val]
    /\ run' = [run EXCEPT !.gen = run.gen + 1]
    /\ written' = {}
    /\ pinned' = {}
    /\ UNCHANGED <<disk, cache, super, superCache, done, free>>

\* The commit writes one of its blocks.
CommitWrite(b) ==
    /\ committing # None
    /\ committing.phase = "write"
    /\ b \in committing.unwritten
    /\ cache' = [cache EXCEPT ![Place(committing.tree, b)] = Content(committing.tree, b)]
    /\ committing' = [committing EXCEPT !.unwritten = @ \ {b}]
    /\ UNCHANGED <<disk, super, superCache, done, run, written, pinned, free, history>>

\* With its blocks written, it flushes the disk's cache: the flush ends once
\* they are durable.
CommitFlush ==
    /\ committing # None
    /\ committing.phase = "write"
    /\ committing.unwritten = {}
    /\ FlushFirst => \A b \in Blocks : cache[Place(committing.tree, b)] = None
    /\ committing' = [committing EXCEPT !.phase = "super"]
    /\ UNCHANGED <<disk, cache, super, superCache, done, run, written, pinned, free, history>>

\* It writes the superblock, with FUA.
CommitSuper ==
    /\ committing # None
    /\ committing.phase = "super"
    /\ superCache' = [root |-> committing.tree.root, gen |-> committing.tree.gen]
    /\ committing' = [committing EXCEPT !.phase = "wait"]
    /\ free' = IF UnpinLate THEN free ELSE free \cup committing.pinned
    /\ UNCHANGED <<disk, cache, super, done, run, written, pinned, history>>

\* The superblock is durable: the commit is done, and the places its copies
\* left are free.
CommitDone ==
    /\ committing # None
    /\ committing.phase = "wait"
    /\ superCache = None
    /\ done' = committing.tree
    /\ free' = IF UnpinLate THEN free \cup committing.pinned ELSE free
    /\ committing' = None
    /\ UNCHANGED <<disk, cache, super, superCache, run, written, pinned, history>>

----------------------------------------------------------------------------
(* The disk *)

\* The disk makes a write durable, in any order.
Persist(l) ==
    /\ cache[l] # None
    /\ disk' = [disk EXCEPT ![l] = cache[l]]
    /\ cache' = [cache EXCEPT ![l] = None]
    /\ UNCHANGED <<super, superCache, done, committing, run, written, pinned, free, history>>

PersistSuper ==
    /\ superCache # None
    /\ super' = superCache
    /\ superCache' = None
    /\ UNCHANGED <<disk, cache, done, committing, run, written, pinned, free, history>>

\* The tree the superblock on the disk leads to.
Mounted ==
    LET r == disk[super.root] IN
    [gen |-> super.gen, root |-> super.root, rgen |-> r.gen, loc |-> r.loc,
     lgen |-> r.lgen, val |-> [n \in Leaves |-> disk[r.loc[n]].val]]

\* The power goes: what the disk had not made durable is lost, and the
\* filesystem is mounted from the disk.
Crash ==
    /\ cache' = [l \in Locs |-> None]
    /\ superCache' = None
    /\ done' = Mounted
    /\ run' = [Mounted EXCEPT !.gen = super.gen + 1]
    /\ committing' = None
    /\ written' = {}
    /\ pinned' = {}
    /\ free' = Locs \ Uses(Mounted)
    /\ UNCHANGED <<disk, super, history>>

----------------------------------------------------------------------------

Next ==
    \/ \E n \in Leaves, v \in Values : Change(n, v)
    \/ \E b \in Blocks : WriteEarly(b) \/ CommitWrite(b)
    \/ StartCommit \/ CommitFlush \/ CommitSuper \/ CommitDone
    \/ \E l \in Locs : Persist(l)
    \/ PersistSuper
    \/ Crash

Spec == Init /\ [][Next]_vars

Symmetry == Permutations(Locs)

----------------------------------------------------------------------------

TypeOK ==
    /\ run \in Trees
    /\ done \in Trees
    /\ super.root \in Locs
    /\ written \subseteq Locs
    /\ pinned \subseteq Locs
    /\ free \subseteq Locs

(* The superblock on the disk leads to a whole tree: each block where its *)
(* parent says, of the generation its parent says.                         *)
Whole ==
    LET r == disk[super.root] IN
    /\ r # None
    /\ r.kind = "root"
    /\ r.gen = super.gen
    /\ \A n \in Leaves :
        /\ disk[r.loc[n]] # None
        /\ disk[r.loc[n]].kind = n
        /\ disk[r.loc[n]].gen = r.lgen[n]

(* And holds the values of the transaction its superblock committed. *)
ReadsBack == Whole => \A n \in Leaves : Mounted.val[n] = history[super.gen][n]

(* A commit that is done is on the disk, or a later one is. *)
Durable == super.gen >= done.gen

(* Nothing is free that a tree in memory, or the one on the disk, uses. *)
FreeIsFree ==
    /\ free \cap Uses(run) = {}
    /\ free \cap Uses(done) = {}
    /\ committing # None => free \cap Uses(committing.tree) = {}
=============================================================================
