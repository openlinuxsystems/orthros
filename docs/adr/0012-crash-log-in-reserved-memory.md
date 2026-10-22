<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0012. Keep the console output in reserved memory across warm restarts

- Status: Accepted
- Date: 2026-10-20

## Context

The reference laptop (ADR 0009) has no serial port. A kernel that crashes
or hangs there leaves only what was on the screen, which a restart erases.
The roadmap asks for the log to survive a warm restart so the next boot
can print it. The usual alternatives each have a gap: pstore through ACPI
ERST or UEFI variables depends on firmware support and writes flash; a USB
debug cable arrives only in Phase 11; network logging only in Phase 12.

## Decision

The kernel reserves 1 MiB of RAM at an address that depends only on the
firmware's memory map: the first 2 MiB boundary at or above 4 GiB with
enough usable RAM (firmware and boot loaders rarely write there), or else
the first such boundary at or above 64 MiB. Every console message, at every
level, is copied into it. The region holds two slots, each a checksummed
64-byte header (boot number, how the boot ended, bytes written) and a ring
of text; each boot writes the slot the previous boot did not, so a crash
during the next boot does not destroy the evidence. The format is in
`radix-debug(7)`.

At boot the kernel prints how the previous boot ended (panic, requested
restart, or still running: a hang, reset or power loss) and its last lines.
Panics and `kdb`'s `reboot` restart without cutting power and write back
the caches first. `crashlog=off` disables the log.

## Consequences

- On machines whose firmware keeps RAM across a warm reset, the last words
  of a crashed boot are visible on the next boot's screen. QEMU under OVMF
  and SeaBIOS does, in both placements (`ob test` checks it); the Dell
  smoke test checks the reference laptop.
- A cold boot, a firmware memory test or a firmware that scrubs memory
  loses the log; the kernel then says no log was found.
- 1 MiB of RAM is set aside; Phase 2's memory manager must keep honouring
  the reservation.
- The place is fixed per machine, not per kernel build, so a newer kernel
  reads an older kernel's log as long as the format version matches.
- Revisit if pstore-style persistent storage, or the Phase 11 and 12
  debugging channels, make it redundant.
