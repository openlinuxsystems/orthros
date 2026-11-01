<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0239. A fragmented NTFS file spreads over records its attribute list names

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11f, its fourth part. A file's record is 1 KiB on most volumes
(4 KiB on disks of 4 KiB sectors), and holds where the file's clusters
are, its run list, beside its names and times. A file written into a
fragmented volume has a run for each piece of free space it took: a few
hundred runs fill its record. [ADR 0238] left such a file failing with
`NoSpace` as it grew, and every file whose record had an attribute
list failing with `NotSupported`.

NTFS lets a file have more records than one. Each extension record
names the first, the base record, and the base record then holds an
`$ATTRIBUTE_LIST`: an entry for each of the file's attributes but the
list itself, wherever it is, by its type, name and first VCN, the
record it is in and its number there, sorted by type, then name, then
VCN. A non-resident attribute may be cut into pieces, each in a record
of its own and covering its own VCNs, the first of them carrying the
attribute's sizes and the others zeros. Windows and ntfs-3g move a
fragmented file's data out so; Windows moves the names of a file of
many hard links out too.

## Decision

- **A file's data spreads when its record has no room** for its runs:
  its unnamed `$DATA` is cut into pieces, each in an extension record of
  its own, whole, and the base record keeps an attribute list naming
  them. The records it had are used again, in order, before new ones
  are taken, and those it no longer needs are freed after its base
  record says so.
- **A compressed or sparse file's pieces end on its compression units'
  edges**, so that no unit is split between records.
- **The data comes back** into the base record, its list and extension
  records gone, when its runs fit there again, as when the file is cut
  short.
- **The list is rebuilt** from the base record and the pieces whenever
  the base record is written: a name added or taken out, a change of
  size or times, so that it names every attribute as it is. It stays in
  the base record (resident); a file whose list would outgrow it fails
  with `NoSpace`, as before, which takes thousands of runs.
- **Before a write takes clusters**, Radix checks that the file's
  records have room to say where they are, so that the write fails
  rather than a later sync.
- **Lists others made are written** the same way when only the file's
  data is in its extension records, as Windows and ntfs-3g leave a
  fragmented file. A file with other attributes there (as Windows moves
  the names of a file of many hard links), a directory whose index has
  a list, and the system's own files with lists are read, not written
  (`NotSupported`).
- **Checks**: host tests write files of thousands of runs (two written
  a cluster of each in turn), plain and compressed, on volumes of
  clusters of 512 bytes and 4 KiB and of sectors of 4 KiB. ntfs-3g,
  The Sleuth Kit and Radix read them back, mounted again; they are
  changed in their pieces, given another name, grown, removed and cut
  short until their list goes, and each step's volume goes to Windows'
  `chkdsk` ([ADR 0238]). The tests read the master file table to check
  the lists, that each compressed piece starts and ends on a unit's
  edge, and that the records a file left are free.

## Consequences

- A fragmented volume takes files as large as its free space, as on
  Windows.
- ntfs-3g cannot make such a file for a test: it takes one free run of
  each 32,768 clusters' worth of `$Bitmap` in each pass over the volume,
  so it cannot fill a small, fragmented volume, and fails with `ENOSPC`.
  Lists Windows and ntfs-3g make are read as before ([ADR 0236]);
  what Radix makes, ntfs-3g, The Sleuth Kit and `chkdsk` check.
- A file of many hard links that Windows moved names out of stays
  read-only, until a later change writes names in extension records.

[ADR 0236]: 0236-ntfs-is-our-own-implementation-read-only-first.md
[ADR 0238]: 0238-ntfs-compressed-sparse-and-links-are-written-and-windows-checks-them.md
