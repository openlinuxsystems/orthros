<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0285. Fixing the last command, and offering bundles for missing ones

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked for a one-key fix of the last command, as thefuck does, and for missing commands to name the package that has them.

## Decision

- After a failure, the empty line's hint is the command fixed, with the Alt+E offer after it; → takes it, Enter runs it. The rules: a mistyped command name (ADR 0259's likely name), `cd` to a folder a case or a typo away, an admin-only command (mount, fdisk, mkfs, ...) again after `elevate`, and a missing command that a bundle has: `bundle install ID`.
- The bundles' commands are read from `/system/state/bundled/catalogue-commands` (`command TAB bundle TAB what it is`), which the store writes when it fetches its catalogue; until it does, nothing is offered. "command not found" then names the bundle. `[commands] offer-install = false` turns it off.
- Pipelines and lists are not fixed: which part failed is not known.

## Consequences

- Nothing ever runs in place of what was typed.
