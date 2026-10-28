subvolume(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

subvolume - make, snapshot and remove Btrfs subvolumes

# SYNOPSIS

*subvolume create* _path_ ...

*subvolume snapshot* [*-r*] _source_ _dest_

*subvolume delete* _path_ ...

# DESCRIPTION

subvolume makes and removes Btrfs subvolumes, as Linux's *btrfs
subvolume* does with the same three commands and messages. A
subvolume is a directory whose files are a tree of their own on the
disk; a snapshot of one is a new subvolume that holds what it held, at
once, in blocks the two share until either changes them (ADR 0138).

*create* makes each _path_ a new, empty subvolume.

*snapshot* makes _dest_ a snapshot of the subvolume that holds
_source_; if _dest_ is a directory, the snapshot goes in it, with
_source_'s name. What was written to _source_'s files but not yet
synced is in it. A subvolume inside _source_ is an empty directory in
the snapshot, as on Linux.

*delete* removes each _path_, a subvolume, with everything in it, and
frees what no snapshot shares. *rmdir*(1) and *rm*(1) remove an empty
subvolume too.

# OPTIONS

*-r*
	Make the snapshot read-only: every change in it fails with
	_Read-only file system_ (*EROFS*), though the filesystem is mounted
	to write.

# DIFFERENCES FROM LINUX

- A subvolume with a file or directory in it open, or one that is the
  filesystem's default subvolume, fails to go with _Device or resource
  busy_; Linux lets its open files live on until they close, and
  refuses the default with _Operation not permitted_.
- A subvolume that holds another fails to go with _Directory not
  empty_, as on Linux; delete the inner one first. Linux's *-R* (all
  below too) is not there yet.
- *list*, *show*, *get-default* and *set-default*, and the other
  commands of *btrfs subvolume*, are not there yet: Phase 15's *snap*
  lists and restores snapshots.
- Removing a subvolume frees its blocks in one transaction, at once,
  rather than in the background as Linux's cleaner does.

# EXIT STATUS

0 when every subvolume was made, snapshotted or removed; 1 when one
was not, after the ones before it, and for a mistake in the command
line, as Linux's *btrfs* exits.

# SEE ALSO

*radix-directory*(2), *radix-files*(7), *systools*(1), *df*(1)
