<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0265. Colour themes, and what an option does as you type it

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked for high-contrast, light and colour-blind presets, and for
option help as you type, from the completion files every program has
(ADR 0102).

## Decision

- **Themes:** `theme = "default" | "high-contrast" | "light" |
  "colorblind"` at the top of colors.toml. A theme is a list of keys it
  changes from the defaults; it is applied before the file's own keys,
  so they change it one by one, and a person's file may pick another.
  high-contrast is bright and bold with nothing faded; light has no
  yellow or white text; colorblind uses blue and yellow (and magenta)
  where the defaults use green and red. Still only the 16 colours.
- **Option help:** while the last word of the line starts with `-` and
  the command has a completion file, the hint is the rest of the
  option's name and its description, when exactly one option (any of
  its forms) starts with what was typed, or one is it. → takes the rest
  of the name only. `[suggest] options = false` turns it off.

## Consequences

- A theme never needs the user to repeat every key.
- The hint shows what `--help` would, without leaving the line.
