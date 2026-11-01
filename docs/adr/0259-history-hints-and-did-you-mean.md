<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0259. History hints, and "Did you mean" after "command not found"

- Status: Accepted
- Date: 2026-10-31

## Context

The mock's last line shows faded text after the cursor, a command from
the history that starts with what was typed, as fish shows it, and its
third shows "Did you mean service?" after a mistyped command. Ubuntu's
command-not-found does the second by looking up packages; we have no
packages to look up, but we know every command the shell can run.

## Decision

- **Hints:** reedline's history hinter, in `[line] hint`'s style; → or
  End takes the hint. It shows only with colours, since without them it
  would look typed. `[suggest] history = false` turns it off.
- **Did you mean:** after "command not found" at the prompt (an
  interactive shell, not inside a function or script), the closest name
  among aliases, functions, builtins and programs on `PATH`, by optimal
  string alignment distance (letters added, removed, changed, or two
  neighbours swapped), at most 1 for names of four letters or fewer and
  2 otherwise; ties go to the same first letter, then the alphabet. The
  message stays one line: `sevice: command not found. Did you mean
  service?`. Nothing is run in its place. `[suggest] did-you-mean =
  false` turns it off. A `command_not_found_handle` function, as in
  bash, still comes first.

## Consequences

- Scripts' messages are unchanged: the shell corpus and the cases in
  nukesh/tests compare them with bash's.
- Unit tests cover the distance and the choice.

## Alternatives considered

- **Running the suggestion after a yes**, as zsh's `correct`. A typo
  that runs the wrong command is worse than one that runs nothing.
