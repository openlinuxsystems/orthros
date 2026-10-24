<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0102. Every program in /system/bin has its manual page, indexed, and the shell completes its options from that page

- Status: Accepted
- Date: 2026-10-24

## Context

Phase 9's exit asks that `man` work for every command in `/system/bin`
and that the shell complete every shipped command, from its argument
definitions. Until now the ports' pages were in the image, but not
Dukovo's own (docs/man, among them those of the systools, Spark,
consoled and Nukesh), and `man` found a page only by its file's name:
`man poweroff` found nothing, as poweroff is a name in reboot(8)'s NAME
section, and `apropos` and `whatis` had no index to read.

The programs come from many places: uutils' clap definitions, GNU's
getopt tables, BSD programs, 7-Zip's own parser, Dukovo's Rust tools.
They share no format for their options, and most have no completion
scripts we could ship; bash-completion's are GPL and written for bash.
What every one of them does have, or must have, is a manual page.

## Decision

- `ob build` compiles Dukovo's pages with scdoc into the image beside
  the ports', except those of what runs on the build host (ob(1),
  dukovo.toml(5)), and fails if a page of a port and one of docs/man
  share a path.
- It indexes every page with the build host's makewhatis(8), from mandoc,
  which Dukovo's man is. The index format is checked to be the one the
  image's mandoc 1.14.6 reads. man, apropos and whatis then find a page
  by each name in its NAME section.
- The build fails if a program in `/system/bin` has no page, by its
  file's name or a NAME section's. Pages that only lead to another, as
  `.so` requests, give a program a page of its own name (egrep, fgrep,
  7z).
- From each program's page, as `mandoc -T ascii` lays it out, the build
  reads the options that the page lists: each line of a list that starts
  with an option forms at the left margin, outside sections such as
  SYNOPSIS and EXAMPLES, with the metavariable after it (`FILE`, `dir`,
  `<COLS>`) telling what its argument completes as, and the first
  sentence of the item's text as its description. It writes them, with a
  short table of what some programs' operands are (users for chown,
  services for service, commands for which and nohup), to
  `/system/share/nukesh/completions/NAME`, a tab-separated file whose
  form nukesh(1) documents, so users can write their own.
- Nukesh completes a program that no `complete` names from its file:
  options with their descriptions in the menu, an option's argument,
  `--name=value`, arguments joined to their option, and operands.

## Consequences

- A program cannot be added to `/system/bin` without a page, so the
  completion and the manual cannot drift from each other: fixing a page
  fixes its completion.
- The descriptions are as good as the pages' first sentences, and the
  argument kinds as good as their metavariables; a program whose page
  lists no options (awk's are in its prose) completes only files.
- The build host needs mandoc as well as scdoc for `ob build`, not only
  for `ob docs`, and the benchmark jobs of CI install it.
- Subcommands' own options (tmux's, 7-Zip's) are not completed; a
  program that needs more can be given a bash-style `complete` script
  later.
