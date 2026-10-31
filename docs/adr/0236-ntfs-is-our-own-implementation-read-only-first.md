<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0236. NTFS is our own implementation, read-only first

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11f, its first part. NTFS is what Windows formats its disks with,
and many USB disks come with it; a Dukovo machine that replaces a
Windows one will be asked to read the old disk, and a stick from a
Windows machine should just work (Chase, 2026-10-28). The phase reads
and writes NTFS, unlocks BitLocker and has Windows' chkdsk check what
Dukovo writes; this part reads.

- Linux's ntfs3 and ntfs-3g are GPL, and are not read (the rule for
  every GPL source in this project). Microsoft publishes no
  specification of NTFS. It publishes [MS-FSCC], which lays out
  reparse points (Windows' links and junctions) and the attributes
  files carry, and [MS-XCA], which specifies LZNT1, the compression
  NTFS uses. The rest is described in the Linux-NTFS project's
  documentation of the format (Richard Russon and others), which
  ntfs-3g's developers wrote from what Windows writes, and in later
  write-ups of it.
- The plan named the `ntfs` crate (Colin Finck, MIT OR Apache-2.0) for
  reading. It reads through `std::io::Read + Seek` (or a `no_std` copy
  of them), parses with `binrw`, allocates with `Vec` and `String`
  wherever it needs to, and writes nothing. The kernel allocates only
  fallibly (every allocation can fail with `NoMemory`), reads through
  its block cache and page cache, and keeps a filesystem's state in one
  crate a compartment without `unsafe` code can hold; writing, which
  the next parts add, would change most of the reading code anyway.
- An NTFS volume is a master file table (`$MFT`) of 1 KiB records (4
  KiB on disks of 4 KiB sectors), each a file's attributes: its times,
  its names (one a directory it is in, and a short DOS name beside a
  long one), its data, and for a directory a B+ tree of its names,
  ordered by the volume's up-case table. Data larger than a record is in
  runs of clusters; a file of many attributes spreads over records an
  attribute list names. Every record and index block has its last two
  bytes of each 512 replaced by an update sequence number, to tell a
  torn write.

## Decision

- **A new crate, `radix-ntfs`** (`kernel/fs/ntfs`), a compartment with
  no `unsafe` code, built from those descriptions, with no code of
  Linux's, ntfs-3g's or the `ntfs` crate's. It reads the boot sector
  (sectors of 512 bytes to 4 KiB, clusters up to 2 MiB, records and
  index blocks up to 64 KiB), the update sequences, attributes resident
  and not, run lists with holes, attribute lists, the up-case table,
  directory indexes (their root, blocks and bitmap), `$Volume`'s label
  and version (1.x to 3.x) and `$Bitmap` for the free space. Every
  number from the disk is checked before use: a volume that is not NTFS
  is `UnknownFilesystem`, and one whose numbers do not add up, or a
  record whose update sequence does not match, `Corrupt`.
- **Metadata goes through a block cache of the device's sectors, file
  data through each file's page cache**, read from the clusters a run
  list gives, as exFAT and ext4 read theirs. A compressed file is read
  a compression unit (16 clusters) at a time and decompressed with
  LZNT1, its other pages handed to the cache; a unit that is all holes
  is zeros and one stored whole is read as it is. Data past what is
  initialized reads as zeros.
- **Read-only for now.** A writable mount fails with `NotSupported`,
  which the `mount` call gives as `invalid_argument`, as Linux does for
  what a filesystem cannot write; Spark and `volumed` then mount it
  read-only, as they do a Btrfs Radix cannot write. Windows' hibernated
  or Fast Startup volumes (`hiberfil.sys` starting `hibr`), and volumes
  marked dirty, are found at mount for when writes come.
- **Names**: lookups compare names through the up-case table, as
  Windows does, and prefer a name of the exact case where a volume
  holds two that differ only in case, as ntfs-3g can write. Listings
  leave out DOS short names, which a lookup still finds, and the
  system's files (`$MFT`, `$Bitmap` and the rest, records 0 to 15), as
  Linux's ntfs3 and ntfs-3g do.
- **Links**: a symbolic link of Windows' (relative or absolute), a
  junction, WSL's links and ntfs-3g's own (Interix, a system file
  starting `IntxLNK\x01`) are symbolic links. An absolute target
  (`\??\C:\...` or a volume GUID) is read as a path on the link's own
  volume, `../` up to its root and then the rest, as ntfs-3g reads
  them: the disk may not be mounted where Windows had it. A junction
  is a link, not a directory, so that nothing walks into a loop through
  one.
- **Permissions**: NTFS keeps Windows' security descriptors, not
  POSIX's owners and modes. Every file and directory shows 0755 and
  root's, a link 0777, and a file marked read-only loses its write bits,
  as exFAT and FAT show theirs (ADR 0213). Mapping security descriptors
  to users waits for users (Phase 13b) and writes.
- **What cannot be read fails rather than reads wrong**: an encrypted
  file (EFS) fails with `denied`, and a file whose data is elsewhere
  (offline, OneDrive's recall-on-access, deduplicated) with
  `invalid_argument`, as `not_supported` is given; such a file's size
  and times are still shown. Alternate data streams are not shown yet.
- **The tests** are host tests against ntfs-3g's tools: `mkntfs`
  formats volumes of sectors and clusters of each size, `ntfscp` writes
  files of every size and kind of name into them, `ntfscat` reads back
  the bitmap Radix counts free space from; and `ob fixture ntfs-3g`
  fills a volume through ntfs-3g's FUSE driver, which takes root, with
  what only a mounted filesystem writes (links, junctions, hard links
  enough for an attribute list, compressed and sparse files, a file in
  pieces, a short name, read-only and offline files), kept in the
  repository as its sectors. Property tests feed the parsers random
  bytes, and Kani proves the boot sector's numbers keep every cluster on
  the device and the update sequence inside its record. The `ntfs` boot
  test has Spark mount a volume of each kind, on an NVMe namespace of 4
  KiB sectors and a virtio disk, and the storage program read every
  file back.

## Consequences

- Windows disks and sticks are read by the desktop and the shell
  without a tool of their own; writing, the journal and BitLocker
  follow in Phase 11f's next parts, on this crate.
- ntfs-3g's tools join the host's tools (`ob doctor` names them;
  Fedora packages them as ntfsprogs), as host tools only, GPL like
  e2fsprogs; none of their code is in the repository.
- Nothing in the volume can be changed, so neither a mount nor a crash
  can damage a Windows disk, which is what a reader alone can promise.
- Volumes from Windows NT 3.x and 4.0 (NTFS 1.x and 2.x) are read, but
  were tested only through `mkntfs`'s 3.1; disks of sectors larger than
  4 KiB, and of 512-byte sectors on a device of 4 KiB ones, are
  refused.
