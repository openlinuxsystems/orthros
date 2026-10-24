\* SPDX-FileCopyrightText: 2026 The Dukovo contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

---------------------------- MODULE Capabilities ----------------------------
(***************************************************************************)
(* A model of the Radix capability store's derivation tree (radix-cap,     *)
(* store.rs): capabilities are created, derived, retyped, moved and        *)
(* deleted, and revoked in steps while everything else goes on.            *)
(*                                                                         *)
(* A slot holds a capability: an object, rights and a badge. Deriving puts *)
(* a copy with fewer rights, and perhaps a badge, in an empty slot as the  *)
(* source's child. Retyping makes a new object inside the source's (as     *)
(* memory is retyped into frames and smaller memory) and puts its          *)
(* capability in an empty slot as the source's child. Deleting moves a     *)
(* capability's children up to its parent. A revoke marks its capability,  *)
(* then deletes capabilities below it one leaf at a time (the store's      *)
(* budgeted steps are runs of these with nothing in between), then clears  *)
(* the mark; while the mark is set, nothing can be derived from the        *)
(* capability or from those below it, and the capability cannot move.      *)
(*                                                                         *)
(* The ghost variable `src` records, for each capability, the slots of the *)
(* capabilities it was derived from, directly or not, that still exist.    *)
(* The main invariant, TreeIsDerivation, says the tree's ancestors of a    *)
(* capability are exactly those: so revoking a capability reaches          *)
(* everything ever derived from it, however the tree was cut up by deletes *)
(* and moves in between. Along the derivation history rights never widen,  *)
(* badges stay, and each object is the one above or lies inside it; and    *)
(* every revoke ends.                                                      *)
(*                                                                         *)
(* Spaces, the transfer right, sealing, ranges, kinds and the memory       *)
(* blocks themselves are left out: they are checks on single capabilities, *)
(* which the Kani proofs cover. Any object can be retyped here, a superset *)
(* of what the store allows. docs/design/capabilities.md explains how the  *)
(* model maps to the code.                                                 *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Slots,      \* the slots of every capability space
    Objects,    \* names for kernel objects
    Rights,     \* the rights; must include "copy"
    MaxDepth,   \* the deepest a capability can be derived (MAX_DEPTH)
    None,       \* no parent
    NoCap       \* an empty slot

ASSUME "copy" \in Rights /\ MaxDepth \in Nat \ {0}
ASSUME None \notin Slots \cup Objects /\ NoCap \notin [obj : Objects, rights : SUBSET Rights, badge : {0, 1}]

\* A capability. Badge 0 is no badge; one other badge is enough to check
\* that badges are set once.
Caps == [obj : Objects, rights : SUBSET Rights, badge : {0, 1}]

VARIABLES
    cap,        \* what each slot holds
    parent,     \* the slot each capability was derived from, after deletes
    depth,      \* each capability's stored depth
    revoking,   \* the revoke marks
    origin,     \* the object each object was retyped from, or None
    src         \* ghost: the existing capabilities each was derived from

vars == <<cap, parent, depth, revoking, origin, src>>

Full == {s \in Slots : cap[s] # NoCap}

\* The chain of parents above `s`. A chain is at most MaxDepth long while
\* the invariants hold; the bound keeps this defined when they do not.
RECURSIVE Chain(_, _)
Chain(s, n) == IF n = 0 \/ parent[s] = None THEN {}
               ELSE {parent[s]} \cup Chain(parent[s], n - 1)
Ancestors(s) == Chain(s, MaxDepth)
Descendants(s) == {t \in Full : s \in Ancestors(t)}

\* The store's may_derive: a revoke under way above `s`, or on `s`.
Blocked(s) == \E a \in Ancestors(s) \cup {s} : revoking[a]

\* How far `s` is from its root, which may be less than its depth: a
\* delete moves children up without lowering theirs.
Distance(s) == Cardinality(Ancestors(s))

\* The depths once `t` is derived from `f`: those from `f` up to the root
\* become their distances, as may_derive sets them, and `t` is one deeper.
Settled(f, t) == [x \in Slots |->
                    IF x = t THEN Distance(f) + 1
                    ELSE IF x \in Ancestors(f) \cup {f} THEN Distance(x)
                    ELSE depth[x]]

\* The objects `o` lies inside: what it was retyped from, and so on.
RECURSIVE Origins(_, _)
Origins(o, n) == IF n = 0 \/ origin[o] = None THEN {}
                 ELSE {origin[o]} \cup Origins(origin[o], n - 1)
Outer(o) == Origins(o, Cardinality(Objects))

\* Objects that exist: those a capability names, and those they lie in,
\* which live on while anything retyped from them does.
Live == LET named == {cap[x].obj : x \in Full}
        IN  named \cup UNION {Outer(o) : o \in named}

Init ==
    /\ cap = [s \in Slots |-> NoCap]
    /\ parent = [s \in Slots |-> None]
    /\ depth = [s \in Slots |-> 0]
    /\ revoking = [s \in Slots |-> FALSE]
    /\ origin = [o \in Objects |-> None]
    /\ src = [s \in Slots |-> {}]

\* An original capability for a new object.
Create(s, o) ==
    /\ s \notin Full
    /\ o \notin Live
    /\ cap' = [cap EXCEPT ![s] = [obj |-> o, rights |-> Rights, badge |-> 0]]
    /\ origin' = [origin EXCEPT ![o] = None]
    /\ UNCHANGED <<parent, depth, revoking, src>>

\* Capability::derive and Capabilities::derive: badge 0 asks for none.
Derive(f, t, mask, b) ==
    /\ f \in Full
    /\ t \notin Full
    /\ "copy" \in cap[f].rights
    /\ Distance(f) < MaxDepth
    /\ ~Blocked(f)
    /\ b = 0 \/ cap[f].badge = 0
    /\ cap' = [cap EXCEPT ![t] = [obj |-> cap[f].obj,
                                  rights |-> cap[f].rights \cap mask,
                                  badge |-> IF b = 0 THEN cap[f].badge ELSE b]]
    /\ parent' = [parent EXCEPT ![t] = f]
    /\ depth' = Settled(f, t)
    /\ src' = [src EXCEPT ![t] = src[f] \cup {f}]
    /\ UNCHANGED <<revoking, origin>>

\* Capabilities::retype: a new object inside the one `f` names, whose
\* original capability, with `f`'s rights and no badge, is `f`'s child.
\* Memory capabilities carry no badge.
Retype(f, t, o) ==
    /\ f \in Full
    /\ t \notin Full
    /\ "copy" \in cap[f].rights
    /\ cap[f].badge = 0
    /\ Distance(f) < MaxDepth
    /\ ~Blocked(f)
    /\ o \notin Live
    /\ cap' = [cap EXCEPT ![t] = [obj |-> o, rights |-> cap[f].rights, badge |-> 0]]
    /\ origin' = [origin EXCEPT ![o] = cap[f].obj]
    /\ parent' = [parent EXCEPT ![t] = f]
    /\ depth' = Settled(f, t)
    /\ src' = [src EXCEPT ![t] = src[f] \cup {f}]
    /\ UNCHANGED revoking

\* The store's remove: the children of `s` move up to its parent.
Remove(s) ==
    /\ cap' = [cap EXCEPT ![s] = NoCap]
    /\ parent' = [x \in Slots |-> IF x = s THEN None
                                 ELSE IF parent[x] = s THEN parent[s]
                                 ELSE parent[x]]
    /\ depth' = [depth EXCEPT ![s] = 0]
    /\ revoking' = [revoking EXCEPT ![s] = FALSE]
    /\ src' = [x \in Slots |-> IF x = s THEN {} ELSE src[x] \ {s}]
    /\ UNCHANGED origin

Delete(s) == s \in Full /\ Remove(s)

\* Capabilities::move_cap: the capability keeps its place in the tree.
Move(f, t) ==
    /\ f \in Full
    /\ t \notin Full
    /\ ~revoking[f]
    /\ LET rename(x) == IF x = f THEN t ELSE x
       IN  /\ cap' = [x \in Slots |-> IF x = t THEN cap[f]
                                     ELSE IF x = f THEN NoCap ELSE cap[x]]
           /\ parent' = [x \in Slots |-> IF x = t THEN parent[f]
                                        ELSE IF x = f THEN None
                                        ELSE rename(parent[x])]
           /\ depth' = [x \in Slots |-> IF x = t THEN depth[f]
                                       ELSE IF x = f THEN 0 ELSE depth[x]]
           /\ src' = [x \in Slots |-> IF x = t THEN src[f]
                                     ELSE IF x = f THEN {}
                                     ELSE {rename(y) : y \in src[x]}]
    /\ UNCHANGED <<revoking, origin>>

\* Capabilities::revoke: the first call sets the mark, each call deletes
\* leaves below it, and the last clears the mark.
RevokeStart(s) ==
    /\ s \in Full
    /\ ~revoking[s]
    /\ revoking' = [revoking EXCEPT ![s] = TRUE]
    /\ UNCHANGED <<cap, parent, depth, origin, src>>

RevokeStep(s) ==
    /\ revoking[s]
    /\ \E l \in Descendants(s) : Descendants(l) = {} /\ Remove(l)

RevokeDone(s) ==
    /\ revoking[s]
    /\ Descendants(s) = {}
    /\ revoking' = [revoking EXCEPT ![s] = FALSE]
    /\ UNCHANGED <<cap, parent, depth, origin, src>>

Revoke(s) == RevokeStep(s) \/ RevokeDone(s)

Next ==
    \E s \in Slots :
        \/ \E o \in Objects : Create(s, o)
        \/ \E t \in Slots, mask \in SUBSET Rights, b \in {0, 1} : Derive(s, t, mask, b)
        \/ \E t \in Slots, o \in Objects : Retype(s, t, o)
        \/ Delete(s)
        \/ \E t \in Slots : Move(s, t)
        \/ RevokeStart(s)
        \/ Revoke(s)

\* A revoke that has started keeps getting turns: the kernel's revoke
\* loops until it is done.
Spec == Init /\ [][Next]_vars /\ \A s \in Slots : WF_vars(Revoke(s))

-----------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
    /\ cap \in [Slots -> Caps \cup {NoCap}]
    /\ parent \in [Slots -> Slots \cup {None}]
    /\ depth \in [Slots -> 0..MaxDepth]
    /\ revoking \in [Slots -> BOOLEAN]
    /\ origin \in [Objects -> Objects \cup {None}]
    /\ src \in [Slots -> SUBSET Slots]

\* An empty slot is in no tree and in no history.
EmptySlotsAreBlank ==
    \A s \in Slots \ Full :
        /\ parent[s] = None /\ depth[s] = 0 /\ ~revoking[s] /\ src[s] = {}
        /\ \A x \in Slots : parent[x] # s /\ s \notin src[x]

\* Parents are full and shallower, so the tree has no cycles and no chain
\* is longer than MaxDepth.
TreeWellFormed ==
    \A s \in Full : parent[s] # None => parent[s] \in Full /\ depth[parent[s]] < depth[s]

\* The tree above a capability is exactly what it was derived from.
TreeIsDerivation == \A s \in Full : Ancestors(s) = src[s]

\* Along the derivation history: no right gained, the same object or one
\* inside it, a badge kept once set, and `copy` held by everything copied
\* from.
RightsNeverWiden == \A t \in Full : \A s \in src[t] : cap[t].rights \subseteq cap[s].rights
SameOrInnerObject ==
    \A t \in Full : \A s \in src[t] : cap[t].obj = cap[s].obj \/ cap[s].obj \in Outer(cap[t].obj)
BadgesFixed == \A t \in Full : \A s \in src[t] : cap[s].badge = 0 \/ cap[t].badge = cap[s].badge
CopiedOnlyWithCopy == \A t \in Full : \A s \in src[t] : "copy" \in cap[s].rights

\* What the store's audit checks on each edge of the tree: a capability
\* names its parent's object or one inside it, through any number of
\* retypes, since deletes move capabilities up past the memory they were
\* retyped from.
ParentCovers ==
    \A t \in Full : parent[t] # None =>
        \/ cap[t].obj = cap[parent[t]].obj
        \/ cap[parent[t]].obj \in Outer(cap[t].obj)

\* No object lies inside itself.
OriginsAcyclic == \A o \in Live : o \notin Outer(o)

-----------------------------------------------------------------------------
(* Properties of steps, and liveness *)

\* A capability never changes in its slot.
CapsImmutable == [][\A s \in Full \cap Full' : cap'[s] = cap[s]]_vars

\* Nothing new is derived from a capability under revoke, or from one
\* below it. (A step that empties a slot and fills one is a move.)
NoDerivationUnderRevoke ==
    [][\A t \in Full' \ Full :
          Full \ Full' = {} => \A s \in src'[t] : ~revoking[s]]_vars

\* When a revoke ends with its capability still there, nothing derived
\* from that capability is left.
RevokeIsComplete ==
    [][\A s \in Slots :
          (revoking[s] /\ ~revoking'[s] /\ s \in Full') =>
              \A t \in Full' : s \notin src'[t]]_vars

\* Every revoke ends.
RevokeTerminates == \A s \in Slots : revoking[s] ~> ~revoking[s]

\* Slots and objects are interchangeable, which TLC uses to check the
\* safety properties on a larger model (Capabilities.cfg). Symmetry is not
\* sound for liveness, so CapabilitiesLiveness.cfg goes without it.
Symmetry == Permutations(Slots) \cup Permutations(Objects)

=============================================================================
