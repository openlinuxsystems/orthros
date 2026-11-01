<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0266. `ob bench --shell` times Nukesh next to bash

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked for a bench row for startup and prompt latency against
bash. The kernel bench compares C programs in two VMs; bash does not
run in the Linux VM's initramfs, and what is measured here is the
shell, not the kernel.

## Decision

- `ob bench --shell` builds Nukesh for the host and runs it and bash
  (`--norc --noprofile`) interactively under script(1), each with an
  empty home, TERM=xterm-256color and PS1='$ ', answering the cursor
  questions a terminal answers. It reports the fastest of 20 starts to
  the first prompt, and of 100 round trips of `true` and Enter to the
  next prompt, with their ratio, to `out/bench/shell.md` and `--summary`.
- It is a report, not part of `--gate`: host timings of a few hundred
  microseconds are noisier than the gate allows.

## Consequences

- The line editor's cost per prompt is visible and can be worked on.
