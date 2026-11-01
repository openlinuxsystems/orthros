<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0260. ls, grep, diff and ip in colour, from colors.toml

- Status: Accepted
- Date: 2026-10-31

## Context

The mock shows `ls -l` with folders blue, programs green, archives red
and links cyan, and grep's matches in bold red. Debian and Ubuntu give
these by aliases in their default bashrc. uutils' ls and GNU grep and
diff take `--color=auto` and their colours from `LS_COLORS`,
`GREP_COLORS` and `--palette`; our ip(8) had no colour.

## Decision

- An interactive Nukesh, before its startup files, defines the aliases
  `ls --color=auto`, `grep --color=auto`, `diff --color=auto
  --palette=...` and `ip -color=auto`, and exports `LS_COLORS`,
  `GREP_COLORS` and `IP_COLORS`, all made from colors.toml's `[ls]`,
  `[grep]`, `[diff]` and `[ip]` (ADR 0258). An alias or variable that
  is already there is kept, and the startup files can change or remove
  them. A section's `enabled = false`, `enabled = false` at the top or
  `NO_COLOR` sets nothing.
- `--color=auto` colours only a terminal, so pipes and files stay plain.
- ip(8) takes iproute2's `-c`/`-color[=always|auto|never]`; `auto` is a
  terminal without `NO_COLOR`. It colours interface names, `UP` and
  `DOWN`, IPv4 addresses and MAC addresses, iproute2's colours by
  default and `IP_COLORS`' (`name=36:up=32:...`) when set.

## Consequences

- Scripts and `sh -c` see the tools as they were: aliases are for the
  interactive shell only.
- A unit test of ip covers the colours and `IP_COLORS`.

## Alternatives considered

- **Colours on by default in each tool.** Patching three ports and
  turning colour on where scripts parse output is the wrong default.
