<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0267. The shell exits when its terminal hangs up at the prompt, and private sessions keep nothing

- Status: Accepted
- Date: 2026-11-01

## Context

crossterm 0.29, under reedline, loops for ever when `read` on a terminal that hung up returns 0 or EIO. Nukesh catches SIGHUP for its traps, so a shell whose window or ssh link went away spun at full CPU and never saved its history. Chase also asked for a private mode.

## Decision

- **Hang-up at the prompt:** before each prompt the shell readies, in memory, the jobs to tell and the history lines not yet saved. A SIGHUP that comes while it waits for a line sends SIGHUP to the jobs (SIGCONT to stopped ones), appends those lines to the history file with plain `open`/`write`/`close`, and `_exit(129)`s from the handler, as bash does. While a command runs, SIGHUP goes to its trap or the default as before.
- **`nukesh --private`:** nothing is kept: no command record, no folder ranks, no history file, no notices to other consoles, and no folder resume. The prompt shows a `private` badge. Alt+E and every other feature still work.

## Consequences

- No more orphaned shells spinning after a terminal closes; history up to the last prompt is kept.
- A private session leaves nothing behind for the next person at the machine.
