<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0258. Nukesh colours the line and the prompt, from colors.toml

- Status: Accepted
- Date: 2026-10-31

## Context

The plan's Phase 14c paragraph "Nukesh default colors" asks for a shell
that helps as you type, and Chase asked for it ahead of Phase 14, with a
config file so that every default can be changed. The approved mock
(design/nukesh/nukesh-colors-mock) shows commands, options, strings,
variables and operators in colour, an unknown command red with an
underline before Enter, a badge with the exit status after a failure,
and an admin badge while elevated. Until now the editor used a
highlighter that coloured nothing, and the prompt was `$ ` or `# `.
fish and zsh's syntax highlighting do this; bash does not.

## Decision

- **colors.toml(5).** `/system/settings/nukesh/colors.toml` (shipped
  commented, saying every default), then `~/.config/nukesh/colors.toml`,
  whose keys win one by one. Sections `[prompt]`, `[line]`,
  `[suggest]`, `[ls]`, `[grep]`, `[diff]` and `[ip]`; each style is a
  few words (`"bold blue"`, `"bright-white on red"`). A mistake never
  stops the shell: it is reported when the shell starts and the key
  keeps its default. `nukesh --colors` previews every key in its colour;
  `--check` only checks, for scripts and for editing the file.
- **Only the 16 standard colours**, so that colours follow the
  terminal's theme, and never colour alone: an unknown command is also
  underlined, the badges are words (`✗ 127`, `admin`).
- **Off** with `NO_COLOR` (no-color.org), on a dumb terminal or no
  terminal (the plain reader), and with `enabled = false` or a
  section's `enabled = false`.
- **Highlighting** splits the line as the shell reads it, closely enough
  to colour it, without the parser: it never fails on half-typed text. A
  name where a command starts is an alias, function, builtin, reserved
  word, a path that exists, or a program on `PATH`; the answers are
  cached until `PATH` changes or a command runs.
- **The prompt.** While `PS1` is its default (`$ ` or `# `), the line
  editor draws `name@host folder ›`, then `✗ status` after a failure
  and `admin` and `#` while the shell is root's. `PS1` itself is
  unchanged, so scripts that test it and the plain reader see what they
  did; setting `PS1` turns the drawn prompt off. `›` and `✗` are in
  the Spleen fonts the consoles use; `❯`, the mock's, is not.

## Consequences

- Host unit tests cover the styles, the files' merging and mistakes, the
  shipped file matching the defaults, and the highlighter's pieces.
- Boot tests that read `# ` on the screen still find it: a root shell's
  prompt ends in `admin # `.
- **Not yet:** a wavy underline (SGR 4:3), which the consoles' terminal
  does not draw; true colour or 256 colours, on purpose.

## Alternatives considered

- **Prompt escapes for the badges in `PS1`.** No other shell has them,
  and a new default `PS1` would show in every script that prints it.
- **Running the parser to colour.** It stops at the first error, and a
  line being typed is full of them.
