<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0120. Extended state enables what the host's does, with AMX held off

- Status: Accepted
- Date: 2026-10-26

## Context

Radix enabled x87, SSE, AVX and AVX-512 in XCR0, and left MPX,
protection keys and AMX off: nothing used them, and AMX's 8 KiB of tile
data would not fit the save area in a thread's 4 KiB context block.

KVM runs with its host's XCR0. When a guest's differs, it loads the
guest's with XSETBV before every entry to the guest and its own after
every exit. A Linux guest enables each user component it supports that
the guest's CPUID offers, and KVM offers those its host enabled, so a
Linux guest's XCR0 is its host's and the two XSETBVs never run. Radix's
differed on every CI runner: no protection keys on any, and no AMX on
the Intel ones. On bare metal an XSETBV costs little, but CI's KVM runs
inside a cloud virtual machine, and on Intel CPUs XSETBV in a virtual
machine always exits to the hypervisor below: each exit of Radix's
virtual machine took two more of the runner's own.

On a new CI runner type, an Intel Xeon 6973P-C, the `ob bench` row for
a page unmapped while its space runs on the other CPU took 16.5 µs
against Linux's 15.1 µs (1.09 times) after nine runs, in code that was
at 0.95 on AMD runners. Nearly all of the row is two exits: the IPI's
send and its delivery to the other CPU. The XSETBVs were the one
difference found in what the hypervisor does for the two guests at those
exits.

## Decision

- XCR0 enables each of x87, SSE, AVX, AVX-512, protection keys (PKRU)
  and AMX's tile configuration and tile data that CPUID leaf 0xD
  reports, as Linux does. MPX stays off, as Linux has left it since
  version 5.6.
- Protection keys stay unused: CR4.PKE stays clear, so PKRU changes no
  access.
- AMX is on only where extended feature disable (XFD) can hold its tile
  data off, and is always held off: IA32_XFD names tile data on every
  CPU, so each AMX instruction faults with #NM. A program sees an
  invalid instruction (SIGILL), as it did with AMX off and as a Linux
  program does that has not asked for AMX. Held off, the tile data is
  never saved, so an image ends with the tile configuration: 2,816
  bytes on a CPU with AVX-512 and AMX, against 2,688 before.
- AMX's components are never loaded from a signal frame's image, which
  is the program's memory: they are always initial, and an invalid tile
  configuration would make XRSTOR fault in the kernel.

## Alternatives

- **XCR0 as it was.** Every exit of a virtual machine nested on an Intel
  CPU costs two more exits.
- **AMX on and saved in full.** 8 KiB of tile data for each thread does
  not fit the context block, and no program uses AMX yet; Linux saves it
  only for a program that asks for it.
- **Programs may ask for AMX now**, with `arch_prctl`'s
  `ARCH_REQ_XCOMP_PERM` as in Linux. It needs larger save areas made on
  demand; it waits for a program that wants it.

## Consequences

- Under KVM, Radix's XCR0 matches the host's wherever a Linux guest's
  does, and no exit switches it.
- Save areas, and the room a signal frame takes on the stack, grow: on a
  CPU with AVX and protection keys but not AVX-512, QEMU's emulated one
  among them, from 832 bytes to 2,696, since PKRU's place in the
  standard format comes after AVX-512's. The bytes copied into a signal
  frame stay those the components in use need.
- An AMX instruction faults with #NM rather than #UD, and raises the
  same signal.
- A host that enables a component Radix does not know, such as APX on a
  later CPU, makes the two differ again until Radix enables it too.
