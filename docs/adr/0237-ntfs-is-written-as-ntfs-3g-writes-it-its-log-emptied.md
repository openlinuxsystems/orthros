<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0237. NTFS is written as ntfs-3g writes it, its log emptied

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11f, its second part: writing NTFS ([ADR 0236] read it). The
volume must stay one that Windows mounts without a check, and that
`chkdsk` finds nothing wrong with.

- **Windows journals every change to the volume's metadata** in
  `$LogFile`, and at mount replays what a crash left there. The format
  of its records (the client NTFS writes, its redo and undo operations)
  is not published, and is only partly described where the rest of NTFS
  is. ntfs-3g does not journal. Before its first change it empties the
  log: it fills it with `0xFF`, which Windows takes as a clean log, and
  starts afresh at its next mount. A volume whose log holds changes to
  replay, or that Windows left dirty or hibernated with, ntfs-3g mounts
  read-only, since writing would lose or undo Windows' changes.
- Chase chose that way on 2026-10-31 ("Clean log, as ntfs-3g does"),
  over journaling Windows' way, which would need the log's format worked
  out from what Windows writes.
- Without a journal, a crash in the middle of a change can leave the
  volume inconsistent. Windows then checks it at its next mount, since
  the volume stays marked dirty until the change is done.

## Decision

- **A writable mount** of a volume Radix must not change fails with
  `NotSupported`, and Radix's mount makes it read-only, as for a Btrfs
  Radix cannot write ([ADR 0130]):
  - NTFS before 3.0;
  - a volume Windows left dirty, or hibernated (or Fast Startup) with;
  - one whose `$LogFile` is not clean: some restart page, at the log's
    start or one system page in, says a client is in the log, and not
    that the log is clean. A log all `0xFF` is clean too;
  - one whose master file table Radix cannot grow: its `$DATA` in an
    attribute list, or not initialized to its end;
  - one whose `$MFTMirr`, `$LogFile` or `$MFT`'s `$BITMAP` is not a
    plain stream of clusters.

  Mounted read-only, the kernel says why, and what would let Dukovo
  write it: `Files: the NTFS of vda is only read: Windows hibernated
  with it, or shut down with Fast Startup; shut Windows down fully
  (Shift with Shut down) to write it`.
- **Before its first change after a mount**, Radix empties the log as
  ntfs-3g does, straight to the disk, and marks the volume dirty in
  `$Volume`, written through. A sync that leaves nothing to write, with
  no file waiting to be written, marks it clean again, as an unmount
  does. A crash between leaves it dirty, and Windows' next mount checks
  it.
- **Records:**
  - The first records (four, or a cluster's worth if a cluster holds
    more) go to `$MFTMirr` too, each time they change.
  - The master file table grows when it has no free record: by 16
    records, or a sixteenth of itself when that is more, in whole
    clusters. It takes clusters after its last while they are free, else
    one run long enough, from its zone first, so that the runs its
    record 0 holds stay few. New records are formatted free.
  - New files take records from 24 on, after those Windows keeps for
    itself.
  - A record's header counts its `$FILE_NAME`s, as Windows and ntfs-3g
    count its links.
- **Clusters** are taken next-fit from where the last ones were taken,
  outside the MFT zone (the eighth of the volume after the master file
  table, which Windows keeps for it) while there are any, then before
  it, then in it. A file grows from its last cluster on when it can.
  Run lists are packed with lengths signed as offsets are, as Windows
  and ntfs-3g read them.
- **Files:**
  - Data stays in its record while the record has room for it, as
    Windows keeps small files, and moves to clusters when it does not.
    A record that needs room for a name moves its data out first.
  - A new file takes its directory's security descriptor (its
    `$Secure` number), the archive attribute and NTFS 3's
    `$STANDARD_INFORMATION`.
  - Times are kept to NTFS's 100 ns. Each name's copy of the file's
    times, size and attributes, in its directory's index, is updated
    when the file is written back.
  - Making a file read-only (no write permission for anyone) sets its
    read-only attribute, as ntfs-3g maps it.
- **Names:**
  - A new name is a Win32 name, or Win32 and DOS together when it is a
    valid upper-case 8.3 name already, as Windows makes it. Radix makes
    no other short names, as Windows does not on a volume other than
    its system's.
  - A name Windows could not open fails with `BadName`, as ntfs-3g's
    `windows_names` option refuses it: one with `\ / : * ? " < > |` or
    a control character in it, one ending in a space or a dot, and a
    device's (`CON`, `NUL`, `COM1` and the others, with or without an
    extension).
  - Renames, hard links (up to 1024 a file) and removal work as POSIX
    says; a rename may change only a name's case.
- **Directories:** an index changes as a B+ tree, in its root while the
  record holds it, then in blocks of the volume's index size. The
  blocks a change may need are taken before it starts, so that it never
  stops half done for want of space. A listing goes on after the last
  name it gave, so that taking names out while listing (`rm -r`) skips
  none.
- **Not written yet**, failing with `NotSupported` (11f.3 adds them):
  compressed, sparse and encrypted data; files whose attributes spread
  over records (an attribute list); symbolic links, junctions and other
  reparse points; and directories whose bitmap is in clusters. The
  plan's override, with which the owner mounts a hibernated volume to
  write after a warning, comes with them, since it needs a way for
  `volumed` to ask.
- **Lock order**: the namespace, a page cache, a node's state (one at a
  time), the free space, the master file table, the block cache.

## Consequences

- Windows mounts what Radix wrote without a check, and keeps no journal
  of Radix's changes to replay or undo, as with ntfs-3g. A crash while
  Radix writes leaves the volume dirty, and Windows checks it at its
  next mount; Radix mounts it read-only until then.
- Host tests write volumes `mkntfs` made, of every cluster size and of 4
  KiB sectors, and check them with ntfs-3g's `ntfsresize --info` (each
  cluster in use held by one file, and each file's clusters in use),
  `ntfsfix -n` (`$MFTMirr` matches), `ntfsls` and `ntfscat`, and with
  The Sleuth Kit's `fls` and `icat` (no file lost from its directory).
  `chkdsk` on Windows itself comes in 11f.3, in CI.
- A dirty volume needs Windows (or `ntfsfix`) before Radix writes it
  again.

[ADR 0130]: 0130-btrfs-files-change-under-one-transaction-lock-and-data-is-copied-at-write-back.md
[ADR 0236]: 0236-ntfs-is-our-own-implementation-read-only-first.md
