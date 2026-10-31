<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0204. Zen's scalable machine-check banks

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11e, its first part. The roadmap asks for Zen 5's machine-check
banks, the Framework Desktop's, which ADR 0169 left for this phase.
Since Zen, AMD's CPUs have the scalable machine-check architecture
(SMCA): CPUID 0x80000007's EBX bit 3 says so, and each of their 20 to 30
banks belongs to one block of the chip (a core's load-store unit, its
level 2 cache, the level 3 cache, the data fabric, a memory controller,
the security processor...). Each bank keeps the legacy four registers,
through which ADRs 0145, 0168 and 0169 read it, and has registers of its
own from MSR C000_2000h, 16 a bank:

- MCA_CONFIG, whose `McaX` bit the operating system must set to say it
  reads the new registers. Without it the CPU takes the kernel for one
  that cannot handle an uncorrected error, which it then treats as
  fatal. Another bit says whether an address's least significant valid
  bit is in the status (bits 29:24) rather than in the address (bits
  61:56), as on Zen 4 and later.
- MCA_IPID: a hardware ID and a type that name the block, and an
  instance ID that names which one: for a memory controller, its
  channel.
- MCA_SYND: the error's syndrome.
- MCA_DESTAT and MCA_DEADDR: a second place for a deferred error, an
  uncorrected one that nothing has used yet, which the status can lose to
  a later error.

What the kernel did until now was wrong on Zen in three ways: it never
set `McaX`; it counted no memory error, as Zen's memory controllers do not
use the compound error code that names a channel; and it took a memory
controller's address for a physical one, when it is the controller's own
normalized address, so a deferred DRAM error would have retired the
wrong page. It also called deferred errors corrected, as their status
does not say they are uncorrected.

Linux sets `McaX` (`arch/x86/kernel/cpu/mce/amd.c`), names the blocks
from MCA_IPID, reads a memory controller's channel from the sixth
hexadecimal digit of its instance ID (`drivers/edac/amd64_edac.c`), and
translates its addresses through the data fabric before it retires a
page.

## Decision

- **`radix_mca::smca`** works out the new registers, so its tests run on
  the host: their addresses, MCA_CONFIG's value with `McaX` set, the
  block each bank's MCA_IPID names (the core's, the data fabric's, the
  memory controllers', and the system blocks of client and server Zen,
  as AMD's Processor Programming Reference lists them), a memory
  controller's channel, the extended error code (MCA_STATUS's bits
  21:16), and where an address's least significant bit is.
- **Each CPU sets `McaX`** in every bank's MCA_CONFIG as it turns
  reporting on, keeping the firmware's other bits.
- **Each error read** carries its bank's MCA_IPID, its MCA_SYND where the
  status says it holds one, and MCA_CONFIG's place of the address's
  least significant bit. Its text names the block, a memory controller's
  channel and the extended code (a memory controller's codes 0 and 1 by
  name, a DRAM ECC error and poisoned data written), and ends with the
  two registers: `corrected error without an error code in the unified
  memory controller on channel 1, a DRAM ECC error, address 0x12340
  (status 0x8420000000000000, ipid 0x9600150f00, synd 0x5a00000000000001)`.
- **Deferred errors** are logged as such, and counted as uncorrected,
  as Linux's EDAC counts them.
- **Memory errors** are a memory controller's bank's, on the channel its
  instance ID names, for the EDAC counts of ADR 0169.
- **A memory controller's address names no page**: the page is not
  retired until the kernel can translate the address through the data
  fabric. Other blocks' addresses are physical, their least significant
  bit read where MCA_CONFIG says.
- **The poll** clears MCA_DESTAT with a status that held a deferred
  error, as Linux does, and reads it when the status lost one. The poll
  now takes the banks' lock a bank at a time, so that only one bank's
  errors are on the stack (ADR 0058).
- **The log** says the banks are of AMD's scalable kind.

## Consequences

- Zen's uncorrected errors reach the kernel as the architecture means,
  its memory errors are counted by channel, and no deferred DRAM error
  retires a page it does not name.
- QEMU's AMD CPUs have no scalable banks, so the host tests cover the
  decoding, and the Framework Desktop's run checks the rest
  ([ADR 0079](0079-framework-desktop-second-reference-machine.md)).
- Not yet: translating a memory controller's address through the data
  fabric, so its pages can be retired and its errors counted by DIMM;
  the deferred-error interrupt (MCA_CONFIG's `DeferredIntType`), so a
  deferred error is read at once rather than at the next poll; the
  blocks' own extended codes beyond the memory controller's two; and
  the blocks of AMD's GPUs and server chips that client Zen lacks.
