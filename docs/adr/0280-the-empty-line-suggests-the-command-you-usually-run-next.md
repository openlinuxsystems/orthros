<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0280. The empty line suggests the command you usually run next

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked for Warp-style next-command suggestions, without AI from the record, and ranked by Nebix when it runs.

## Decision

- On an empty line, the hint is the command that most often came right after the last one: from the command record (ADR 0262), counting pairs within 15 minutes, twice for this folder, shown only if it came next at least twice and worked the last time. → takes it; nothing runs until Enter. `[suggest] next = false` turns it off.
- When Nebix is running it is asked once per prompt (`{"next": last, "status", "folder", "guess"}`, 300 ms at most) and its one-line answer wins; it is never more than one hint.
- After a failure the empty line is for fixing it instead (ADR 0285).

## Consequences

- Habits show without anything leaving the computer.
