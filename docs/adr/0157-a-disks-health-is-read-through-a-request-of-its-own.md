<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0157. A disk's health is read through a request of its own

- Status: Accepted
- Date: 2026-10-29

## Context

Phase 10e asks for disk health: SMART for SATA disks and the health log
for NVMe drives, read by `diskhealth` and reported. A server with a RAID
(ADR 0153) should hear that a disk is wearing out before it fails, so
that it can be replaced (ADR 0155) in time.

On Linux, smartctl(8) and smartd(8) send the disk raw commands: ATA
commands through SG_IO's ATA PASS-THROUGH, and NVMe admin commands
through NVME_IOCTL_ADMIN_CMD. Any command goes, so the kernel cannot
tell a read of the health log from a firmware download, a security
erase or a format; Linux asks for CAP_SYS_RAWIO or CAP_SYS_ADMIN, and
the tools run as root. logd and the web console, where the roadmap has
the reports go, come in Phase 13.

## Decision

- radix-block(2) gains `GET_HEALTH`, a request of Dukovo's own. The
  kernel sends only the commands that read health, and writes the
  answer as one record: a kind (SATA or NVMe), flags, then 512 bytes
  of SMART READ DATA or of the NVMe SMART / Health Information log
  (log 02h, for every namespace), then 512 bytes of SMART READ
  ATTRIBUTE THRESHOLDS or zeros. It needs only the read right; a
  partition answers for its disk; a device that keeps no record, such
  as a virtio disk, fails with not_supported.
- The AHCI driver reads the two SMART pages as commands that go out
  alone, through the disk's queue like a flush, so they never meet the
  disk's queued reads and writes. A disk whose IDENTIFY DEVICE data
  says SMART is off is reported so, and not asked.
- The NVMe driver reads the log on the admin queue, polled under the
  controller's lock, as it does its other admin commands.
- The kernel does not judge the record. diskhealth(8) does, as
  smartctl does: FAILING when an attribute that warns of failure is at
  or below its threshold, or the NVMe critical warning says the drive
  is failing; WARNING for an old-age attribute at its threshold,
  reallocated or pending sectors, SMART off, heat, worn-out endurance
  or media errors.
- Spark runs `diskhealth --watch` as a default service. It looks at
  every disk at boot and every half hour, as smartd does by default,
  and writes a line, which Spark puts in the kernel log, at the first
  look and when a disk's verdict or reasons change. Once logd comes,
  the lines go there with the rest of the kernel log.

## Alternatives

- **Pass raw commands through, as Linux does.** smartctl's full
  coverage, self-tests and error logs would come with it, and so would
  every command that can destroy a disk; a capability that allows
  reading health would allow erasing it. A later request can add
  self-tests by name if they are wanted.
- **Judge the disk in the kernel.** The kernel would carry tables of
  attribute meanings that vary by maker, and a policy that belongs to
  the tool; the record is small, and the tool reads it whole.
- **Check health only on request.** A disk that wears out quietly would
  only be found when someone looks; smartd's half hour costs two short
  commands on each SATA disk and one on each NVMe controller.

## Consequences

- A SATA disk's two SMART reads wait for its queue to empty and hold it
  for their time, as a flush does, every half hour.
- A drive's record is read as it is; what an attribute's raw value
  counts is its maker's choice, so diskhealth judges by the normalized
  values and thresholds, and only counts sectors from the raw values of
  the attributes most disks share (5, 197, 198).
- USB disks (Phase 11b) keep no record until SCSI-to-ATA translation is
  worth adding; GET_HEALTH fails with not_supported on them.
