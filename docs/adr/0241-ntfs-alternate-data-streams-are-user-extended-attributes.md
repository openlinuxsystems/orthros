<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0241. NTFS's alternate data streams are `user.` extended attributes

- Status: Accepted
- Date: 2026-11-01

## Context

Phase 11f. An NTFS file has more data than its own: named `$DATA`
attributes, its alternate data streams, which Windows opens as
`file:stream`. Windows keeps where a download came from there
(`Zone.Identifier`, which makes it warn before running one), and apps
keep their own data, from thumbnails to macOS's resource forks copied
by SMB. [ADR 0236] read NTFS, and [ADR 0237] and [ADR 0238] wrote it,
keeping a file's streams as they were but showing none: Radix's VFS had
no way to name them. Phase 13b.7 gave it Linux's extended attributes
([ADR 0256]): `user.`, `trusted.` and `security.` names, and POSIX
access lists, which Btrfs, ext4 and tmpfs keep. Chase chose on
2026-10-31 to make the streams writable in Phase 11.

ntfs-3g shows streams the same way, with its `streams_interface=xattr`
option (its default): the attribute `user.NAME` is the stream `NAME`,
read with `getfattr` and written with `setfattr`. A disk moved between
Linux and Dukovo then shows its streams alike in both.

## Decision

- **`user.NAME` is the stream `NAME`**, of any file or directory,
  as ntfs-3g names it. Listing a file's attributes lists its streams,
  in the order of its record. A stream's name is UTF-16 on the disk: a
  name that is not UTF-8, or holds `:`, `/`, `\` or NUL, which Windows
  takes in no stream's name, fails with `BadName` (EINVAL). A stream
  whose name is not valid UTF-16, or too long for an attribute's name
  (255 bytes, Linux's limit), is not listed.
- **Reads** find the stream in every record of the file, as its
  attribute list says; a value is at most 64 KiB, Linux's largest, so a
  larger stream fails with `OutOfRange` (ERANGE), and a compressed or
  encrypted one with `NotSupported`.
- **Writes** put a stream in its file's base record when it has room
  there, as Windows keeps a small one, and in clusters of its own when
  it has not, taken and written before the record says so; the old
  value's clusters are freed after. The room the file's own data still
  takes in its record (memory has it ahead of the record) is kept for
  it, so that storing it later cannot fail.
- **A full record makes room as Windows does**: when a stream, or a
  name, has no room, the file's own data leaves the record for clusters
  first, then its other streams do, the largest first. A file whose
  data spread over extension records ([ADR 0239]) keeps its streams in
  its base record, and its attribute list names them.
- **Names are sorted as NTFS sorts them**: a record's attributes of one
  type, and its attribute list's entries, are ordered by their names'
  upper cases (the volume's `$UpCase`), then unit by unit, as Windows
  and ntfs-3g look them up. Until now Radix's names of one type were
  only ever `$I30` or `$R`, and it sorted unit by unit, which differs
  for a name of lower-case letters: ntfs-3g then stopped looking before
  reaching it.
- **A removed file's streams' clusters are freed** with its record; they
  were kept until now.
- **Not on NTFS**: `trusted.`, `security.` and POSIX access lists fail
  with `NotSupported` (EOPNOTSUPP), as on ntfs-3g without its `acl`
  option; Windows keeps a file's permissions in its security descriptor
  instead. A file whose other attributes spread over its records (as
  Windows moves the names of a file of many hard links) is read, but its
  streams are not written (`NotSupported`), as its names are not.
- **Checks**: host tests set, replace and remove streams in records and
  in clusters, on files and directories, on every test volume and on
  ntfs-3g's, whose stream reads back; ntfs-3g's `ntfscat -n` and The
  Sleuth Kit's `icat` read them back, `ntfsresize --info` finds no
  cluster lost, Radix reads them back mounted again, and Windows'
  `chkdsk` checks each volume and reads each stream as Windows
  names it, `file:stream`. The random-change property test sets and
  removes streams among its other changes, and the damaged-volume fuzz
  test reads and changes them.

## Consequences

- A download's zone, an app's stream and a stream a Mac left go with
  their files between Windows, Linux and Dukovo; a backup tool that
  copies extended attributes copies them.
- Tools that list attributes see a Windows volume's streams, as with
  ntfs-3g; `Zone.Identifier` is the one most files carry.
- Dukovo has no `getfattr` and `setfattr` yet; programs use
  getxattr(2) and setxattr(2), as ntfs-3g's users do through those
  tools.

[ADR 0236]: 0236-ntfs-is-our-own-implementation-read-only-first.md
[ADR 0237]: 0237-ntfs-is-written-as-ntfs-3g-writes-it-its-log-emptied.md
[ADR 0238]: 0238-ntfs-compressed-sparse-and-links-are-written-and-windows-checks-them.md
[ADR 0239]: 0239-a-fragmented-ntfs-file-spreads-over-records-its-attribute-list-names.md
[ADR 0256]: 0256-access-lists-and-extended-attributes-as-linux-keeps-them.md
