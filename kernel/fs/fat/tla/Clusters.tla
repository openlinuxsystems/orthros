\* SPDX-FileCopyrightText: 2026 The Dukovo contributors
\* SPDX-License-Identifier: Apache-2.0 OR MIT

------------------------------ MODULE Clusters ------------------------------
(***************************************************************************)
(* A model of how a FAT volume's clusters change hands between files and  *)
(* directories while their data waits in memory to be written back        *)
(* (kernel/fs/fat: node.rs's `FileBacking` and `Drop`, fs.rs's            *)
(* `free_chain`, `cut` and `zero_cluster`; kernel/fs/vfs/src/cache.rs;    *)
(* kernel/fs/block/src/cache.rs).                                          *)
(*                                                                         *)
(* Two caches sit in front of the disk. A file's data is in its own page  *)
(* cache, and is written back to whatever clusters its chain holds at the *)
(* moment of the write-back. A directory's entries, like the tables, are  *)
(* in the volume's block cache, kept by disk position: its dirty sectors  *)
(* are written back where they are, whoever owns them by then. So a       *)
(* cluster that leaves a directory must leave the block cache too         *)
(* (`discard`), or entries written before the free could land on top of   *)
(* the data of the file that gets the cluster next. A file's cluster      *)
(* needs nothing of the kind: its dirty pages past a cut are dropped      *)
(* first, and a removed file's pages go with it, unwritten.               *)
(*                                                                         *)
(* Each step below is atomic: in the code, a file's steps hold its        *)
(* cache's lock, a directory's hold the volume's `ns` lock, and the block *)
(* cache's hold its own. The checks: nothing is ever written to a cluster *)
(* by anyone but its owner of the moment (a free cluster has none), and  *)
(* every owner reads back what it last wrote, from memory or, once memory *)
(* has dropped it, from the disk. With `Discard` FALSE (a configuration   *)
(* `ob test tla` does not run) TLC finds a removed directory's entries    *)
(* written back over a cluster that is free or already a file's.          *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Files,      \* the files
    Clusters,   \* the volume's clusters
    MaxVersion, \* how many times each owner writes
    Discard     \* whether freeing a directory's cluster discards it

Dir == "dir"
Free == "free"
None == "none"
Owners == Files \cup {Dir}

VARIABLES
    owner,      \* who each cluster belongs to, or Free
    chain,      \* each file's cluster, or None
    fver,       \* the last version each file wrote (0: none yet)
    fcached,    \* whether the file's page is in its page cache
    fdirty,     \* whether that page has changes the disk lacks
    bver,       \* the last version written to each directory cluster
    bvalid,     \* whether the block cache holds the cluster
    bdirty,     \* whether it has changes the disk lacks
    disk,       \* what each cluster holds on the disk: [who, version]
    foreign     \* whether anything was ever written to a cluster it did not own

vars == <<owner, chain, fver, fcached, fdirty, bver, bvalid, bdirty, disk, foreign>>

Versions == 0..MaxVersion

TypeOK ==
    /\ owner \in [Clusters -> Owners \cup {Free}]
    /\ chain \in [Files -> Clusters \cup {None}]
    /\ fver \in [Files -> Versions]
    /\ fcached \in [Files -> BOOLEAN]
    /\ fdirty \in [Files -> BOOLEAN]
    /\ bver \in [Clusters -> Versions]
    /\ bvalid \in [Clusters -> BOOLEAN]
    /\ bdirty \in [Clusters -> BOOLEAN]
    /\ disk \in [Clusters -> [who : Owners \cup {None}, version : Versions]]
    /\ foreign \in BOOLEAN

Init ==
    /\ owner = [c \in Clusters |-> Free]
    /\ chain = [f \in Files |-> None]
    /\ fver = [f \in Files |-> 0]
    /\ fcached = [f \in Files |-> FALSE]
    /\ fdirty = [f \in Files |-> FALSE]
    /\ bver = [c \in Clusters |-> 0]
    /\ bvalid = [c \in Clusters |-> FALSE]
    /\ bdirty = [c \in Clusters |-> FALSE]
    /\ disk = [c \in Clusters |-> [who |-> None, version |-> 0]]
    /\ foreign = FALSE

\* Writing `who`'s `version` to cluster `c` on the disk.
DiskWrite(c, who, version) ==
    /\ disk' = [disk EXCEPT ![c] = [who |-> who, version |-> version]]
    /\ foreign' = (foreign \/ owner[c] # who)

----------------------------------------------------------------------------
(* Files *)

\* `reserve`: a file with no cluster takes a free one.
FileGrow(f, c) ==
    /\ chain[f] = None
    /\ owner[c] = Free
    /\ owner' = [owner EXCEPT ![c] = f]
    /\ chain' = [chain EXCEPT ![f] = c]
    /\ fver' = [fver EXCEPT ![f] = 0]
    /\ fcached' = [fcached EXCEPT ![f] = FALSE]
    /\ fdirty' = [fdirty EXCEPT ![f] = FALSE]
    /\ UNCHANGED <<bver, bvalid, bdirty, disk, foreign>>

\* `FileCache::write`: the page changes in memory.
FileWrite(f) ==
    /\ chain[f] # None
    /\ fver[f] < MaxVersion
    /\ fver' = [fver EXCEPT ![f] = @ + 1]
    /\ fcached' = [fcached EXCEPT ![f] = TRUE]
    /\ fdirty' = [fdirty EXCEPT ![f] = TRUE]
    /\ UNCHANGED <<owner, chain, bver, bvalid, bdirty, disk, foreign>>

\* `FileCache::sync`: the page goes to the cluster the chain holds now.
FileWriteBack(f) ==
    /\ fdirty[f]
    /\ chain[f] # None
    /\ DiskWrite(chain[f], f, fver[f])
    /\ fdirty' = [fdirty EXCEPT ![f] = FALSE]
    /\ UNCHANGED <<owner, chain, fver, fcached, bver, bvalid, bdirty>>

\* `FileCache::shrink`: a clean page is dropped.
FileReclaim(f) ==
    /\ fcached[f]
    /\ ~fdirty[f]
    /\ fcached' = [fcached EXCEPT ![f] = FALSE]
    /\ UNCHANGED <<owner, chain, fver, fdirty, bver, bvalid, bdirty, disk, foreign>>

\* `set_size(0)`, or the last reference to a removed file going: its page
\* is dropped, dirty or not, and then its cluster is freed.
FileFree(f) ==
    /\ chain[f] # None
    /\ owner' = [owner EXCEPT ![chain[f]] = Free]
    /\ chain' = [chain EXCEPT ![f] = None]
    /\ fcached' = [fcached EXCEPT ![f] = FALSE]
    /\ fdirty' = [fdirty EXCEPT ![f] = FALSE]
    /\ UNCHANGED <<fver, bver, bvalid, bdirty, disk, foreign>>

----------------------------------------------------------------------------
(* Directories *)

\* `new_dir_cluster`: a free cluster is zeroed in the block cache.
DirGrow(c) ==
    /\ owner[c] = Free
    /\ owner' = [owner EXCEPT ![c] = Dir]
    /\ bver' = [bver EXCEPT ![c] = 0]
    /\ bvalid' = [bvalid EXCEPT ![c] = TRUE]
    /\ bdirty' = [bdirty EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<chain, fver, fcached, fdirty, disk, foreign>>

\* An entry changes in the block cache.
DirWrite(c) ==
    /\ owner[c] = Dir
    /\ bver[c] < MaxVersion
    /\ bver' = [bver EXCEPT ![c] = @ + 1]
    /\ bvalid' = [bvalid EXCEPT ![c] = TRUE]
    /\ bdirty' = [bdirty EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<owner, chain, fver, fcached, fdirty, disk, foreign>>

\* `free_chain(.., discard)`: a removed directory's cluster is freed.
DirFree(c) ==
    /\ owner[c] = Dir
    /\ owner' = [owner EXCEPT ![c] = Free]
    /\ IF Discard
        THEN /\ bvalid' = [bvalid EXCEPT ![c] = FALSE]
             /\ bdirty' = [bdirty EXCEPT ![c] = FALSE]
        ELSE UNCHANGED <<bvalid, bdirty>>
    /\ UNCHANGED <<chain, fver, fcached, fdirty, bver, disk, foreign>>

----------------------------------------------------------------------------
(* The block cache, which knows positions, not owners *)

\* `BlockCache::sync`: a dirty cluster goes to the disk where it is.
BlockWriteBack(c) ==
    /\ bdirty[c]
    /\ DiskWrite(c, Dir, bver[c])
    /\ bdirty' = [bdirty EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<owner, chain, fver, fcached, fdirty, bver, bvalid>>

\* `BlockCache::shrink`: a clean cluster is dropped.
BlockReclaim(c) ==
    /\ bvalid[c]
    /\ ~bdirty[c]
    /\ bvalid' = [bvalid EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<owner, chain, fver, fcached, fdirty, bver, bdirty, disk, foreign>>

----------------------------------------------------------------------------

Next ==
    \/ \E f \in Files, c \in Clusters : FileGrow(f, c)
    \/ \E f \in Files : FileWrite(f) \/ FileWriteBack(f) \/ FileReclaim(f) \/ FileFree(f)
    \/ \E c \in Clusters :
        DirGrow(c) \/ DirWrite(c) \/ DirFree(c) \/ BlockWriteBack(c) \/ BlockReclaim(c)

Spec == Init /\ [][Next]_vars

(* Nothing is ever written to a cluster but by its owner of the moment. *)
NoForeignWrites == ~foreign

(* Every file reads back what it last wrote: its page in memory, or once *)
(* that is dropped (or was never there), its cluster on the disk.        *)
FilesReadBack ==
    \A f \in Files :
        (chain[f] # None /\ fver[f] > 0 /\ ~fcached[f])
            => disk[chain[f]] = [who |-> f, version |-> fver[f]]

(* Every directory cluster reads back what was last written to it, the   *)
(* same way, through the block cache.                                    *)
DirsReadBack ==
    \A c \in Clusters :
        (owner[c] = Dir /\ ~bvalid[c]) => disk[c] = [who |-> Dir, version |-> bver[c]]

=============================================================================
