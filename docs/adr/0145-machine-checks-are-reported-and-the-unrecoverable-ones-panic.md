<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0145. Machine checks are reported, and the ones the CPU cannot go on after panic

- Status: Accepted
- Date: 2026-10-28

## Context

Phase 11d asks for machine-check (MCE) handling on Intel and AMD. x86-64
processors report hardware errors, in their caches, TLBs, buses and the
memory they read, through the machine-check architecture: each CPU has
banks (IA32_MCG_CAP says how many; QEMU gives 10), and each bank holds the
last error it saw in IA32_MCi_STATUS, with an address and more detail
when the status says so. An error the hardware corrected only sits in its
bank. One it did not correct raises #MC, vector 18, if CR4.MCE is set; if
it is not, the CPU shuts down. IA32_MCG_STATUS then says whether the
interrupted code can restart (RIPV), and the banks say how bad the error
is. A second #MC before the first is ended (MCG_STATUS.MCIP cleared) shuts
the CPU down too.

Until now Radix left CR4.MCE clear, so an uncorrected error reset the
machine without a word, and corrected errors, the warning that memory or
a cache is going bad, were never read.

Linux turns every bank on (IA32_MCi_CTL to all ones, Intel's family 6
before model 0x1a aside, where bank 0 belongs to the firmware), logs what
the banks hold at boot, polls them every five minutes on CPUs without an
interrupt for corrected errors (CMCI on Intel, thresholds on AMD), and in
#MC grades each error: the processor's state corrupt (PCC) or an
uncorrected error it cannot recover from panics; with software error
recovery (MCG_CAP.SER_P) an error that needs action (AR) in a program ends
that program and retires the page, and one that needs none (SRAO) is
logged. Its decoding lives in `mcelog` and `rasdaemon`.

## Decision

- Each CPU turns reporting on as it starts, as Linux does: IA32_MCG_CTL
  (where there is one) and each bank's control to all ones, then CR4.MCE.
  `mce=off` leaves it off.
- The #MC handler (on its own stack, as before) lets the CPU go on when
  MCG_STATUS.RIPV is set and no bank holds an error it cannot go on after:
  PCC, or uncorrected and signalled without software error recovery, or
  needing action (AR). It then clears MCG_STATUS and leaves the errors in
  their banks, since a handler that can interrupt anything may not log.
  Otherwise the exception is fatal: the panic names the bank and decodes
  its error, as the crash log keeps it for the next boot.
- A kernel thread, `mce`, reads every CPU's banks at the end of boot,
  which finds what an earlier boot left (the banks keep their errors
  across a restart, which is how an error that panicked or reset the
  machine is seen afterwards), and every 300 s after (`mce-poll=seconds`),
  through each CPU's worker. Each error is cleared and logged with its CPU
  and bank: a warning if corrected, an error if not. One lock serializes
  the reads, so a bank that several CPUs share is read by one.
- A new library, `radix-mca`, decodes a bank's status into words, from
  the error codes Intel's SDM and AMD's APM share (simple codes, and
  compound ones naming the cache, TLB, memory channel or bus and the kind
  of access), and grades it. Its tests run on the host.

## Consequences

- The boot test `machine-check` uses QEMU's `mce` monitor command, which
  fills a bank as a CPU would: a corrected cache error must be logged
  within a second, an uncorrected memory error with PCC must panic with
  the decoded error, and the next boot must find it in its bank.
- Errors that need action in a program panic for now, instead of ending
  the program, since the kernel cannot yet retire a page; the memory
  error work of 11d brings that, with error counts by DIMM (EDAC) on
  machines with ECC, CMCI and AMD's thresholds in place of polling, and
  the Zen 5 banks' own registers (SMCA) in 11e. Legacy bank registers
  alias them, so AMD machines are read today.
- A #MC broadcast to every CPU (Intel's default for uncorrected errors)
  runs the handler on each; only CPUs whose banks hold the fatal error,
  or whose code cannot restart, panic, and the panic stops the others.
