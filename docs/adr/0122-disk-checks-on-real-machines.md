<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0122. Real machines' disks are checked by serial number, and Linux checks what Dukovo wrote

- Status: Accepted
- Date: 2026-10-27

## Context

Phase 10a is done when the Dell Inspiron 15 5570 (an AHCI controller with
an SSD and a hard disk) and the Framework Desktop (two NVMe drives) read
and write raw disks. QEMU's disks cannot show what real ones do: a write
cache that loses what a flush did not reach, a drive that numbers its
namespaces or reports its sizes in its own way, firmware that is slow at
some transfer sizes. The boot tests cover the drivers' logic; a real
machine has to cover the rest.

Three things make this harder than a boot test. The project lead runs
each check by hand, booting a USB stick, with no serial port to read a
log from and no network driver yet (Phase 12) to send one. The disks
may be erased, but only the ones the lead names: the Framework's second
NVMe drive holds data and must never be written. And disk names cannot
say which disk is which, since both Dukovo and Linux name disks in the
order their controllers are found, which can differ between the two
systems and between boots.

The phase also asks for each disk's speed next to Linux's on the same
machine, which CI's virtual machines cannot give.

## Decision

The check is one C program, `disktest` (userspace/posix/disktest.c),
built for Dukovo and for Linux. It writes only to the disk whose serial
number it is given, and does nothing without `--yes-erase`. Dukovo's
`/kernel/block/<disk>/device/serial` gives every SATA and NVMe disk's
serial number, as Linux's sysfs gives an NVMe disk's; on Linux the
program also reads a SATA disk's from SCSI's unit serial number page.
When no disk has the serial number, or more than one does (namespaces of
one controller), it names the disks it saw and stops.

`./ob image --disk-check SERIAL` adds a second entry to the boot menu,
which runs the program on Dukovo, and puts Linux's side on the stick:
the program built for Linux and `crosscheck/run.sh`. On Dukovo the
program:

1. writes 3 bytes off a sector's boundary and reads the sector back,
   which only Dukovo's raw devices take (Linux's need whole sectors);
2. writes a pattern of its own, different at every offset and in every
   run, to 64 places of 1 MiB spread from the disk's first MiB to its
   last, empties the disk's write cache, and reads every place back;
3. measures 4 KiB, 64 KiB and 1 MiB reads and writes one after the other,
   as `ob bench` does, each three times, the fastest counting;
4. saves what it found, with the pattern's seed, in a report area of the
   disk (the MiB from 33 MiB in), since the disk under test is the only
   place it may write that Linux can read.

The lead then restarts into a Linux live system, whose `run.sh` finds
the same disk by its serial number and runs Linux's side: it reads
Dukovo's report, reads every place back with the seed, which shows that
what Dukovo wrote reached the disk itself and survived the power going
off, measures the same transfers on the same disk, and prints the result
as Markdown, which goes into the sub-phase's report.

The `disk-check` boot test runs both sides on QEMU's NVMe disk: Dukovo's
in the virtual machine, Linux's on the host on the disk's file.

## Consequences

- No disk is written by mistake: the serial number is given when the
  image is built, shown in the boot menu, and checked again by Linux's
  side, which asks before it writes.
- Linux reading Dukovo's places after a restart checks flushes and
  shutdown on each machine's own disks, which no virtual disk can.
- Disk speed on real hardware comes from the same program on the same
  disk on both systems, so the comparison does not depend on how either
  system's benchmark tools differ.
- Linux's side is linked against the live system's C library, not built
  static, so the image carries none of the GNU C Library's code; it
  needs a live system with glibc 2.34 or later (2021 and on).
- The disk under test loses what it held, as the lead agreed for the
  Dell's disks and one of the Framework's drives (Phase 10 prep, question
  3); the other Framework drive is written only in 10e, when the lead
  says so.
