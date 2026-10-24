<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0100. consoled draws the virtual consoles as a program, a terminal for TERM=linux built on the vte crate

- Status: Accepted
- Date: 2026-10-23

## Context

ADR 0097 gives programs the keys and the screen. Something must turn
them into the six virtual consoles Phase 9 asks for, each a terminal
with a login shell on it. Programs read the terminal's type from TERM,
and the terminfo database Phase 9 ships (ncurses, ADR 0086) has entries
for every common terminal; the one administrators expect on a text
console is `linux`.

Parsing a terminal's escape sequences correctly is subtle, and the vte
crate, the parser of the Alacritty terminal, does it as DEC's VT500
state machine (Paul Williams' description) does. It is under Apache-2.0
OR MIT and has no dependencies beyond core with its default features
off.

## Decision

- consoled(8), in `userspace/programs/consoled`, is a Rust program on
  the standard library, started by Spark as the `consoled` service. It
  takes `/dev/fb0`, reads `/dev/kbd`, and keeps six consoles, tty1 to
  tty6 (up to twelve with `--consoles`).
- Each console is a terminal emulator for TERM=linux: vte parses the
  bytes, and consoled implements what the `linux` terminfo entry lists,
  with the Linux console's colours, a 1000-line scrollback, wide
  characters as two cells and the 256 and true colours as the nearest of
  xterm's 256. It draws with the kernel's Spleen fonts (`radix-fbcon`,
  ADR 0015) at the size the kernel's console chose, and only the cells
  that changed.
- Its keys are the Linux console's: a US key map, the function and
  editing keys' sequences, Alt as an ESC prefix, Num Lock on from the
  start. Alt+F1 to Alt+F6 switch, Shift+Page Up and Down scroll back,
  Scroll Lock holds output, and Ctrl+Alt+Delete runs reboot(8).
- Each session is root's login shell from `/system/etc/passwd` on a
  pseudo-terminal, started the first time its console is shown, after
  the text of `/system/etc/issue`, and started again when it ends, with
  a pause after five quick ends in a row.
- consoled is written for Dukovo, after the behaviour of Linux's console
  (its console_codes(4) and the `linux` terminfo entry); no code is
  taken from Linux.

## Consequences

- Programs that work on a Linux text console, such as nano, nvi, tmux,
  less and top, work on Dukovo's with TERM=linux.
- Without a screen, consoled exits with 0 and Spark leaves it stopped;
  the serial console stays the way in.
- The kernel's messages no longer show on the screen once consoled runs;
  dmesg(1) shows them, and a panic still draws over everything.
- Other key maps (Phase 11), a login prompt (Phase 13), mouse selection
  and Unicode fonts beyond Spleen's are left for later.
