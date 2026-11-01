<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0263. A long command tells you it ended, when you are on another console

- Status: Accepted
- Date: 2026-11-01

## Context

A long build on tty2 ends while you are on tty1: you only find out by
going back. Desktop shells send a notification; Dukovo has notifyd
(ADR 0212) and consoled shows notices on the console's bottom line.

## Decision

- A command typed at the prompt that runs for `[notify] after` seconds
  or more (settings.toml(5), 30) sends notifyd one request: title
  `<command> finished` or `failed with status N`, body `after 1m 35s on
  pts/1`, app `nukesh`, and a new field, `terminal`, the shell's
  terminal as `pts/N`. The shell does not wait for an answer.
- notifyd keeps `terminal` (32 characters at most) with the
  notification and sends it on. consoled does not show a notification
  whose terminal is the console on the screen: you saw it end.
- Editors, pagers, `ssh`, `top`, `watch`, `tmux`, other shells and
  REPLs send none: they are worked in, not waited for.

## Consequences

- Any program may name its terminal the same way.
- With notifyd not running, nothing happens and nothing waits.
