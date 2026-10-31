<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0212. Notifications on the console, and the privacy promise

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 13a.3. The plan's last piece of logging is `notifyd`, "a small
service speaking a capability-based IPC protocol" that the console
subscribes to in 1.0, the web console later and Lumen from 1.1, with
console delivery now; and a privacy promise, stated in the handbook and
the installer: Dukovo has no telemetry, nothing leaves the machine
unless the user starts it, update checks are the only automatic network
calls and can be turned off, and crash reports are kept locally and
shared only by hand. The security design (section 11) lists
`notifications` as a permission that is always allowed and can be turned
off per app, through notifyd's interface. The prep doc settled with
Chase how the console shows one: on the bottom line, for 8 seconds
(question 5).

A notification is shown to whoever is at the screen, so who sent it
must be what the kernel says, not what the sender claims, as with the
log (ADR 0210). It may hold private text, a message's first line, so it
is not something to keep.

## Decision

- **notifyd** is a service of systools, started by Spark before
  consoled. It answers on `/system/live/notify/socket`, a line of JSON a
  request, as logd does: `send` with a title, a body, an urgency (low,
  normal or critical) and how many seconds to show it; and `list`, the
  newest it keeps, with `follow` to go on with each new one as it
  comes. Linux programs' `notify-send` options work: urgency, expiry
  time, app name, and icons, categories and hints, which the console
  takes and ignores.
- **The sender is the kernel's word.** notifyd puts on each
  notification the number of the process that connected
  (`socket_state`) and its name from `/processes`. A name the program
  gives for itself is shown after its own, as `notify-send (backup)`,
  never alone.
- **What a notification may hold.** Its title at most 120 characters,
  its body 1000 and its time 60 seconds; a tab is a space, a control
  character a `?`, and the title keeps no newline. A critical one has no
  time: it stays until a key is pressed, as the freedesktop.org
  specification lets critical notifications wait for the user.
- **Floods.** A process may send 10 notifications in any 10 seconds;
  notifyd refuses more, and logs the refusal once a flood. A request
  must come within 5 seconds of connecting, and a follower that does
  not read what it is sent in 5 seconds is cut off, so no program can
  hold notifyd up; it answers on one thread.
- **Nothing is kept.** notifyd keeps the last 64 notifications in
  memory, for `notifications`, and writes nothing of them to the disk or
  to the log; they are gone when it stops.
- **The console.** consoled follows notifyd and shows the newest
  notification on the screen's bottom line, over the shown console's
  last line, in white on blue (red for critical, grey for low), for its
  time, 8 seconds by default; a newer one takes the line at once.
  Control characters never reach the screen as themselves. A key that
  takes a critical notification off still goes to the console. While
  notifyd is not running, consoled tries again every 5 seconds.
- **The privacy promise** goes into the handbook's privacy chapter and
  `dukovo-privacy(7)` now, for the installer to show when it comes: no
  telemetry; nothing leaves the machine unless the user starts it, but
  for what a network needs to work and the update checks; logs, crash
  logs and notifications stay on the machine. It says what Dukovo sends
  on its own today (nothing: the network stack is still being built)
  and what each later service will send, and that each will be listed
  there before it ships.

## Consequences

- A program, a script or a service can tell whoever is at the console
  something without writing to their terminal, and cannot pass itself
  off as another program in doing so.
- A program can still put misleading text in a notification under its
  own name; the trusted path for passwords is Ctrl+Alt+Delete (13b), not
  anything a notification shows.
- Every program can send and list notifications until users come; with
  13b, notifyd will tag each with its sender's user and show a user's
  notifications only on their own consoles, and `notifications` will
  list one's own. Per-app off switches come with 13d's grants, and
  do-not-disturb with the Notification center (1.3.1).
- A process that forks to get around the flood limit gets 10 per child;
  a per-user limit can come with users.
- The plan's promise says update checks are the only automatic network
  calls. DHCP, the time from timed (12a) and discovery (12c) are
  automatic too, so the promise says "but for what a network needs to
  work", and the privacy chapter lists each.
