<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0245. ISO 9660 and UDF are read-only compartments, tried after the disks' filesystems

- Status: Accepted
- Date: 2026-10-31

## Context

Installation media and much software still come as disc images: ISO
9660 for CDs and most Linux and BSD images, UDF for DVDs, Blu-ray discs
and Windows' installation media, often with an ISO 9660 "bridge" beside
the UDF so that older systems read the disc too. Users write those
images to USB sticks, keep them on disks, and still have discs. A
Dukovo machine should open them as Linux, Windows and macOS do.

ISO 9660 (ECMA-119) names files in upper case with eight and three
letters and a version (`README.TXT;1`), at most eight directories deep.
Two extensions lift that: Joliet (Microsoft, 1995), a second directory
tree with UCS-2 names, and Rock Ridge (IEEE P1282, on the System Use
Sharing Protocol, IEEE P1281), which adds POSIX names, modes, owners,
times, symbolic links and device numbers to each record of the primary
tree, and moves directories deeper than eight levels to one of their
own with links back. A file larger than 4 GiB is several extents, each
with a record of the file's name.

UDF (ECMA-167 with the OSTA UDF specification, revisions 1.02 to 2.60)
is a different filesystem: file entries with allocation descriptors,
directories of file identifier descriptors, and a logical volume whose
partition maps say how a partition's blocks are found: in a row
(physical), in a row but for packets moved after they wore out
(sparable, for rewritable discs), through a virtual allocation table
written last on the disc (virtual, for CD-R and other write-once
media), or through a metadata file (UDF 2.50 and later, Blu-ray).

Linux's `isofs` and `udf` are GPL-2.0, and the BSDs' are tied to their
kernels. The specifications are free to read.

## Decision

Two new compartments, `radix-iso9660` and `radix-udf` in `kernel/fs`,
written from ECMA-119 4th edition, the Joliet specification, SUSP and
RRIP 1.12, ECMA-167 3rd edition and UDF 2.60, without `unsafe`, each
built and tested on the host as the other filesystems are.

- **Read-only.** Nothing on a disc can be written, and writing UDF to
  a disk is left for when a user needs it. A mount to write fails with
  `ReadOnly`, so Spark and volumed mount the volume read-only, as they
  do a disk that refuses writes.
- **ISO 9660 reads one tree.** Rock Ridge's, if the root's own record
  says the volume uses it; else Joliet's (the supplementary descriptor
  of the highest level); else the primary tree, its names in lower case
  without `;1` and found in either case, as Linux's `map=normal` shows
  them. Without Rock Ridge every file and directory is `r-xr-xr-x`,
  owned by user 0, as on Linux, since any file of a disc may be a
  program. A file of several extents is one file; interleaved files are
  refused with `NotSupported`; associated files are not listed; a
  relocated directory is listed only where its link stands. The label
  is the Joliet volume identifier if there is one, else the primary.
- **UDF reads every kind of partition** mkudffs makes: physical,
  sparable (with its sparing table), virtual (the allocation table found
  in the last 16 blocks of the device) and metadata (its file, else its
  mirror); file entries and extended file entries, short, long and
  extended allocation descriptors and data held in the entry, with
  their continuation extents; indirect entries of strategy 4096; and
  logical blocks of 512 to 4096 bytes. Deleted and hidden entries are
  not listed, as Linux hides them. The label is the logical volume's
  identifier. A partition map of another kind fails with
  `NotSupported`.
- **Tried last, UDF first.** Spark and volumed try FAT, exFAT, NTFS,
  Btrfs, ext4, then UDF, then ISO 9660: a disc image written over a disk
  may leave the disk's old superblocks (and the other way round), and a
  DVD's ISO 9660 is only a bridge to its UDF, which is what Windows and
  macOS read too. `mount -t iso9660` still reads a bridge disc's ISO
  9660. blkid names both, with libblkid's labels and UUIDs.
- **As the other filesystems.** Metadata goes through the device's
  block cache, file data through each file's page cache, read in runs
  of whole pages straight from the extents on the device. Each file has
  one node while it is in use.

The host tests read images made by xorriso (ISO 9660 with and without
Rock Ridge and Joliet), mkudffs (each kind of partition and revision)
filled by udfclient, and genisoimage (a UDF bridge), all host tools
only. The `discs` boot test mounts an ISO 9660 disc and a UDF one at
boot and lists every file.

## Consequences

- USB sticks and disks holding disc images, and disc images attached
  to virtual machines, open in `/volumes`. Optical drives themselves
  wait for an ATAPI and SCSI MMC driver: a drive's packet commands and
  its 2048-byte sectors.
- Only the first session of a multisession disc image is read, since an
  image has no table of contents to name the last one; El Torito boot
  images are not shown as files.
- A UDF disk cannot be written yet; it mounts read-only.
- The free space of both is shown as none, as a disc has.
