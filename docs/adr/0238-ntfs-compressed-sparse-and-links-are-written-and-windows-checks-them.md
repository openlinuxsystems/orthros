<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0238. NTFS's compressed and sparse data and links are written, and Windows checks them

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11f, its third part. [ADR 0237] wrote NTFS as ntfs-3g does, and
left compressed and sparse data, symbolic links and the plan's
hibernation override failing with `NotSupported`. Its checks were
ntfs-3g's and The Sleuth Kit's, not Windows' own: the plan asks for
Windows' `chkdsk` in CI, for power cuts, and for damaged volumes to be
fuzzed.

- **Compressed data** is LZNT1 ([MS-XCA]) in compression units of 16
  clusters. Windows compresses only on volumes of clusters of 4 KiB or
  less, and keeps each unit compressed only when that saves a cluster.
  A unit that compresses is followed by a hole in the run list, so a
  compressed file is sparse too.
- **Sparse files** keep holes for what was never written, which read
  as zeros.
- **Symbolic links** on NTFS are reparse points ([MS-FSCC]), each listed
  in the view index `$R` of `$Extend\$Reparse`, keyed by its tag and
  its file's reference. Windows' own (`IO_REPARSE_TAG_SYMLINK`) take a
  path of Windows names; WSL's (`IO_REPARSE_TAG_LX_SYMLINK`) take any
  bytes, and Windows does not follow them.
- **Hibernation**: Windows hibernated (or Fast Startup) restores its
  memory, the volume's caches with it, on its next start. Writing the
  volume in between loses what Windows changes. ntfs-3g has an option,
  `remove_hiberfile`, that writes it anyway and deletes `hiberfil.sys`.

## Decision

- **Compressed files** are written a compression unit at a time:
  - each unit compressed when that takes fewer clusters, else as it
    is, and left a hole when it is all zeros, as Windows does. A
    compressed unit keeps room for the zero header that ends its data:
    The Sleuth Kit reads a unit only to one, or to the unit's end;
  - a change rewrites the units it touches, reading only those it does
    not cover whole, and takes the unit's new clusters before giving
    back its old ones;
  - only on volumes of clusters of 4 KiB or less, as Windows: a file
    made in a directory marked compressed is compressed there, and not
    on larger clusters. A compressed file on clusters Windows does not
    compress on is not written (`NotSupported`).
- **Sparse files** are given clusters a compression unit at a time as
  data goes into a hole, zeros around it, as Windows fills them. A file
  may grow to 2^32 − 1 clusters, as on Windows, larger than its volume.
  When resident data moves to clusters, the file's compressed and sparse
  flags go with it.
- **Symbolic links**:
  - A target that is a relative path of names Windows takes is made as
    a Windows link, relative, with the path in Windows' form as both its
    substitute and its print name, so that Windows follows it. Any other
    (absolute, with `:` or a trailing `/`, or names Windows refuses) is
    made as WSL's, its target as it is in UTF-8.
  - A link to a directory is made as a directory with an empty index,
    as Windows makes one, so that Windows lists it as one.
  - Each link is added to `$Extend\$Reparse` when made, and taken out
    when its last name goes. Its tag is in its names' index entries
    too, as Windows keeps it.
  - `$R`'s entries sort as unsigned 32-bit words, the tag first; the
    index grows into blocks and back as directories do.
- **The hibernation override**: `remount -w --discard-hibernation`
  (*remount*(8), a new `discard_hibernation` mount flag) writes a
  hibernated volume after a warning that what Windows left open is
  lost. Radix then zeroes the first page of `hiberfil.sys`, so that
  Windows boots afresh rather than resume, and keeps the volume marked
  dirty, so that Windows checks it at its next start. The flag passes
  over the dirty mark and the log Windows left only on a volume it
  hibernated with: on any other, a dirty volume or one with a log to
  replay stays read-only, and a clean one mounts as it would without
  the flag. Another filesystem, or a bind mount, refuses the flag.
- **The dirty mark** goes to `$MFT`'s copy of `$Volume` before
  `$MFTMirr`'s, and is cleared in `$MFTMirr`'s first: a power cut
  between the two writes leaves the copy Windows reads dirty.
- **Windows checks what Radix writes**, in CI:
  - The NTFS host tests keep each volume they check, when
    `NTFS_IMAGES` asks: as a fixed VHD of one MBR partition, with a list
    of each path and the SHA-256 of each file.
  - A Windows job mounts each with `Mount-DiskImage`, runs `chkdsk` on
    it, which must find nothing wrong, and reads every file back, which
    must match; nothing else may be on it, but on a volume others
    filled, whose list names only what a test changed.
  - Volumes of 4 KiB sectors are left out, as a VHD has none.
  - What it found first: `chkdsk` takes a long name in the Win32
    namespace without a short name beside it, as [ADR 0237] made them,
    for an error ("minor file name errors", and each index entry of it
    "incorrect"). A new name that is not a valid 8.3 name now goes in
    the POSIX namespace, as ntfs-3g puts every name; Windows lists and
    opens it as any other.
- **Power cuts:**
  - A host test cuts a sync at a dozen points and at each of its last
    six requests: the disk takes the first requests and fails the rest.
    Each cut leaves the volume dirty, or, after its last write, whole
    and clean. Files synced before it keep their records and bytes.
    Windows' `chkdsk /f` must mend each, and find the synced files at
    their paths.
  - The `ntfs-power-cut` boot test cuts QEMU's power while the storage
    test writes: what it synced must be there, and the volume dirty, or
    clean and sound to ntfs-3g when the cut came just after a sync,
    which clears the mark. Radix then mounts it read-only, as it mounts
    a dirty volume until Windows checks it; the test marks a clean one
    dirty first.
- **Damaged volumes**: a property test flips bytes in the records and
  index blocks of the ntfs-3g volume, past what their update sequences
  catch, mounts it read-only and writable, reads everything on it and
  changes everything: Radix must never panic.
- **Not written yet**, failing with `NotSupported`: encrypted (EFS)
  data, a file whose attributes spread over records (an attribute
  list), and a directory whose index bitmap is in clusters. Named
  streams are read, and kept as they are, but not written, as Radix's
  VFS has no extended attributes to write them through yet.

## Consequences

- Windows reads what Radix writes, compressed, sparse or linked, and
  `chkdsk` finds it sound. A WSL link is only followed under Linux or
  WSL; Windows lists it.
- A hibernated Windows volume can be written when Windows will not
  start again, at the cost of what it left open.
- CI gains a Windows job, after the unit job: it cannot use `./ob`, so
  it runs `tools/windows/chkdsk.ps1`.
- A file whose run list outgrows its record (a large file on a very
  fragmented volume) cannot grow until attribute lists are written.

[ADR 0237]: 0237-ntfs-is-written-as-ntfs-3g-writes-it-its-log-emptied.md
[MS-FSCC]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/
[MS-XCA]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-xca/
