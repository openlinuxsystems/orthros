<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0282. Prompt marks for terminals, and links on file names

- Status: Accepted
- Date: 2026-11-01

## Context

Terminals such as Warp, iTerm2, kitty, WezTerm and Ghostty can jump between commands and select one command's output when the shell marks them (OSC 133), and can open file names a program prints as links (OSC 8).

## Decision

- `[terminal] marks = true`: reedline's OSC 133 marks around the prompt (A) and the line typed (B), and the shell's own around each command's output: C before it runs and `D;status` after.
- `[terminal] links = true`: `ls` is given `--hyperlink=auto`, so file names it prints to a terminal are links. Pipes and files stay plain.
- The console ignores both, as vte does.

## Consequences

- Shell integration works in modern terminals without a plugin.
