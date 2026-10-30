<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0169. Corrected errors interrupt where the CPU can, and memory errors are counted

- Status: Accepted
- Date: 2026-10-30

## Context

ADR 0145 reads every CPU's machine-check banks at boot and every 300
seconds. A bank holds one error, so between two reads a corrected error
can wait five minutes to be logged, and the next one in the same bank
overwrites it (the status's overflow bit says only that one was lost).
Corrected errors are the warning that memory or a cache is going bad, and
how many there are is what tells a failing DIMM from a stray bit flip.
Phase 11d asks for those counts, and for the interrupts CPUs have for
corrected errors in place of waiting for the poll.

- **Intel's CMCI** (IA32_MCG_CAP bit 10): each bank's IA32_MCi_CTL2 turns
  it on (bit 30) with a threshold of corrected errors (bits 14:0); once
  the bank reaches it, the local APIC's LVT CMCI entry (0x2F0) interrupts.
  The status then counts the corrected errors since it was cleared (bits
  52:38). A bank several CPUs share (the memory controller's, the last
  level cache's) has one IA32_MCi_CTL2, which the first CPU to set the bit
  claims. Linux (`mce/intel.c`) claims banks that way, sets the threshold
  to 1, and stops interrupts during a storm of more than 15 a second.
- **AMD's thresholds**: a bank's MCA_MISC0 (MCi_MISC, which Zen's legacy
  registers alias) can hold a 12-bit counter of the errors the bank
  corrects (CntP, bit 62). It counts from where software sets it, and when
  it reaches 0xFFF it is full (bit 48), stops, and raises the extended LVT
  entry of the local APIC the firmware named in bits 55:52, if it can
  interrupt (bit 60, or bank 4). Linux (`mce/amd.c`) starts each counter
  so that it fills after 4095 errors, its default limit.
- **EDAC**: Linux's drivers count memory errors by memory controller and
  DIMM under `/sys/devices/system/edac/mc`, which `ras-mc-ctl` and
  `edac-util` read. Placing an error on a DIMM needs each chipset's map
  from addresses to DIMMs, read from its own registers. A memory
  controller's error code (0000 0000 1MMM CCCC) names its channel, though.
  Neither reference machine has ECC memory (the Dell's Kaby Lake laptop,
  the Framework's soldered LPDDR5X), and QEMU emulates no memory
  controller, so there is nothing to check a DIMM map against.

QEMU emulates neither CMCI nor AMD's counters.

## Decision

- **CMCI.** Each CPU, once its local APIC is set up, claims the banks it
  can, with a threshold of 1 as in Linux, and points LVT CMCI at vector
  0xF9, Linux's. A bank another CPU or the firmware claimed already, and
  one without CMCI, is only polled. Every CPU's banks are still polled.
- **AMD's thresholds.** Each CPU starts the counter of each bank that has
  one the firmware left unlocked, at Linux's limit: it fills after 4095
  errors. Those that can interrupt use the extended LVT entry the
  firmware named for the first of them, pointed at the same vector; one
  that names another only counts. Zen's extended counters (MISC1 to MISC4)
  are left alone.
- **The interrupt** only wakes the `mce` thread, which reads every CPU's
  banks at once, as its poll does. The poll gives each error a count: the
  status's on Intel, the counter's on AMD (which it starts again), else 1.
  The log line of an error that stands for more says how many.
- **Storms.** More than 15 interrupts in a second make the thread mask
  them on every CPU and read the banks every second; once 30 seconds of
  reads find no corrected error, it unmasks them. The rule
  (`radix_mca::corrected::Storm`) is pure, and its tests run on the host.
- **Memory errors** are counted by the bank and the channel their code
  names, in Linux's EDAC layout: `/kernel/devices/system/edac/mc/mc0`,
  one memory controller as Linux's firmware-first driver (`ghes_edac`)
  shows one, with `ce_count`, `ue_count`, the `_noinfo_` counts of errors
  without a channel, and a `dimm<n>` for each bank and channel that had
  an error (`dimm_label` and `dimm_location` name them, `dimm_ce_count`
  and `dimm_ue_count` count), in the order they first did. Counts last
  from boot.
- The claiming of banks and starting of counters, the counts, the storms
  and the reading of memory errors' channels live in `radix-mca`
  (`corrected`), through a trait for the registers, so their tests run
  against a simulated machine on the host.

## Consequences

- Corrected errors on Intel are logged when they happen, and counted
  even when several land between two reads. On AMD they are counted
  exactly up to 4095 between reads, and logged at the poll, as in Linux.
- The host tests simulate a machine whose CPUs share some banks, with a
  bank without CMCI and one the firmware claimed, and AMD banks with and
  without counters, locked ones, and ones naming other LVT entries. The
  boot test `machine-check` checks the counts and the EDAC files with
  corrected memory errors from both CPUs, polled; the interrupts get their
  first real run on the reference machines (the Dell has CMCI; the
  Framework's Zen 5 has the counters).
- Not yet: counts by DIMM, which need a chipset's address map and a
  machine with ECC to check it on; Zen's banks of their own (SMCA), which
  name the memory controllers' banks, their deferred error interrupt and
  extended counters (Phase 11e); taking a page out of use after many
  corrected errors, as Linux's collector (CEC) does; resetting the counts
  (`reset_counters`).
- The interrupts take vector 0xF9, above the ones devices are given
  (0x50 to 0xEF).
