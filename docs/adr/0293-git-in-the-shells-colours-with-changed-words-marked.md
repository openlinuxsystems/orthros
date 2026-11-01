<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0293. git in the shell's colours, with changed words marked

- Status: Accepted
- Date: 2026-11-01

## Context

The git support plan's last step has readable diffs: the colours of
colors.toml(5) in git, as `ls`, `grep`, `diff` and `ip` have them
(ADR 0260), and the words that changed within a line marked, which
delta and git's own contrib/diff-highlight do. diff-highlight is a Perl
script, and Dukovo has no Perl; delta is a large program of its own.

git takes colours only from its configuration. Settings in the
environment (`GIT_CONFIG_COUNT`) would win over a user's
`~/.gitconfig`, which the user's own choices must not lose to.

## Decision

**Each interactive Nukesh writes `~/.local/state/nukesh/gitconfig`**
from colors.toml when it changed, and **the system's gitconfig
includes it**, last. An included file is read in the scope of the file
that includes it, so the user's `~/.gitconfig` and a repository's own
settings still win; without the file, git ignores the include. It
gives:

- git's diffs colors.toml's `[diff]` colours, with two keys more for
  lines git sees moved (`moved-added`, `moved-removed`), which the
  system's `diff.colorMoved` shows;
- `git status` and `git branch` the prompt's git colours, so staged,
  changed and untracked look the same in both;
- `git diff`, `show` and `log` the pager `nukesh --diff-pager`, and
  `git add -p` the filter `nukesh --diff-highlight`.

With `[diff] enabled = false` the file is empty, and git keeps its own
colours and pager.

**`nukesh --diff-highlight` marks the words that changed**, in
`added-word` and `removed-word` (reverse video of the line's colour by
default, so the 16-colour themes keep working). It reads git's
coloured diff line by line and writes a line for each line it reads,
which `git add -p` needs. In a change, a run of removed lines followed
by as many added ones is paired line by line, as diff-highlight pairs
them; each pair is split into words (letters and digits, spaces, and
each other character alone), and the words outside their longest
common subsequence are marked, except spaces, so that a re-indented
line is not one long mark. A pair with more than 60% of its text
changed is left whole, since marking all of it says nothing. Lines
git did not colour as plain removed or added (moved lines), combined
diffs of merges, diffs without colour and lines over 1000 bytes pass
through as they came. `--diff-pager` writes into less(1), which git
starts with `LESS=FRX`, and goes straight to the terminal if less is
missing.

## Consequences

- The `utilities` part of the posix boot test checks that an
  interactive shell's file gives git its colours and pager, and that a
  word a line gained is marked.
- Every diff git pages goes through one more program, which streams;
  only a run of removed and added lines is held at a time.
- Unpaired runs (three lines become two) are not marked, as with
  diff-highlight; delta pairs them by similarity, which may come later.
