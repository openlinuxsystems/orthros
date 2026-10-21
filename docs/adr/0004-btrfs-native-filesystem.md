<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0004. Btrfs is the native file system

- Status: Accepted
- Date: 2026-10-20

## Context

Section 7 of the specification asks for a copy-on-write native file system
with snapshots, checksums and subvolumes, and lists Btrfs, OpenZFS,
bcachefs or a new design as acceptable. Atomic updates and snapshots
(roadmap Phase 15) depend on that choice.

## Decision

Btrfs is the native file system for the system and user data. Radix gets
its own Btrfs implementation in Rust (roadmap Phase 10), compatible with the
on-disk format Linux uses.

## Consequences

- Linux's `btrfs check` and `btrfs-progs` can verify images Radix writes,
  and disks can move between Linux and OrthrOS, which the file system
  torture tests rely on.
- Subvolumes and snapshots give the update and rollback design a ready
  mechanism.
- Writing a compatible Btrfs implementation is a large piece of work and is
  sized XL in the roadmap.
