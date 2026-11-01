<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0264. Hidden files can be shown everywhere, from settings.toml

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked whether Nukesh can always show hidden files. Each place
hides them its own way: `ls` without `-a`, globs without `dotglob`,
and Tab, which also leaves out what a folder's `.hidden` lists (ADR
0113). Three settings for one wish is not friendly, and colors.toml is
for colours.

## Decision

- A second file, settings.toml(5): `/system/settings/nukesh/settings.toml`,
  then `~/.config/nukesh/settings.toml`, key by key, read by interactive
  shells before their startup files; mistakes are reported and skipped.
- `[files] show-hidden = true` makes `ls` `ls -A` (added to the colour
  alias), turns on `dotglob`, and has Tab complete names `.hidden`
  lists. The default is false. The startup files may undo any part.
- `[notify]` (ADR 0263) lives here too.

## Consequences

- One switch, off by default, that a person can turn on for themselves.
- Programs that list folders themselves are unchanged.
