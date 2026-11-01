<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0262. A shared command record, a Ctrl+R picker, z, folder keys, abbreviations and a right prompt

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked for Nukesh to be best in class at the prompt: fuzzy Ctrl+R
ranked by folder (as atuin and fzf do), `z` (zoxide), Alt+←/→ folder
history (fish), abbreviations (fish) and a right prompt with duration,
jobs and git branch. bash's history file has none of a command's
status, duration or folder, and each shell sees another's commands only
when it exits.

## Decision

- **The record:** `~/.local/state/nukesh/commands`, one line a command,
  `time TAB status TAB milliseconds TAB folder TAB command`, escaped,
  appended in one write as each command ends (0600, in a 0700 folder).
  Every shell reads what others added since it last looked, with one
  stat, before each prompt. It keeps the newest 20 000, trimmed at
  30 000 by writing a new file in its place. A command after a space,
  and commands with history off, are left out. HISTFILE, `fc`,
  `history` and the up arrow stay bash's, per shell.
- **The picker:** Ctrl+R opens a list menu over the record: each
  command once, its letters matched in order, scored for letters side
  by side and at word starts, then +40 when run in this folder, then
  freshness over 30 days; each with `✗ status`, duration, age and
  folder. The hint after the cursor comes from the record, this
  folder's newest first.
- **z:** a builtin, with `~/.local/state/nukesh/folders` (`rank TAB time
  TAB path`), counted before each prompt when the folder changed;
  scores are rank × 4 within an hour, × 2 within a day, × ½ within a
  week, × ¼ after; ranks age by a tenth when they add up past 10 000.
  Words match in order, the last in the last part of the path. `z -l`
  lists. A folder that exists by the name given goes there as `cd`.
- **Folder keys:** Alt+← and Alt+→ on an empty line go back and forward
  through this shell's folders (100 kept); on a line with text they
  move a word, as before. They and Space are reedline host commands,
  which return to the shell with the line kept.
- **Abbreviations:** `abbr name expansion`, `abbr`, `abbr -r name`. Space
  after a command word that is one (at the line's start, after `|`,
  `;`, `&`, `(`, `!`, `if`/`then`/`do`/…, or `sudo`/`elevate`) grows it in
  the line; Enter grows one left at the end and shows what ran. Space
  is bound only while there are abbreviations.
- **Right prompt:** while PS1 is its default, `took 4.2s` after a
  command of `[prompt] took-after` seconds or more (3), `N jobs`, and
  `git branch` from the nearest `.git/HEAD` (a worktree's `gitdir:`
  followed; a detached head is 7 hex digits), in `[prompt] took`,
  `jobs` and `branch`'s styles.

## Consequences

- Shells see each other's commands at once, with no daemon and no
  database: a file and a stat per prompt.
- The record is a second copy of what was typed; `rm` of the file, or
  a leading space, keeps a command out of it.
- `abbr` and `z` are builtins bash does not have; scripts that define
  functions of those names still win, as functions come first.
