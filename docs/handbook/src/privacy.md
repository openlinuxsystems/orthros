<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Privacy and security reporting

This chapter is Dukovo's privacy promise, which the installer will show
too, and how to report a security problem. The reference is
dukovo-privacy(7).

## The promise

- **No telemetry.** Dukovo collects nothing about how you use it. No
  part of it sends usage figures, crash reports, identifiers or anything
  else about you or your machine to us or to anyone else.
- **Nothing leaves the machine unless you start it,** but for what a
  network needs to work and the update checks, each listed below with
  how to turn it off. A program you run, a page you open or a file you
  share sends what you ask it to; Dukovo itself sends nothing on its
  own.
- **No account and no ads.** Dukovo needs no online account to install
  or use, and shows no ads.

Every service that sends something on its own is listed here, with
what it sends, where and how to turn it off, before it ships. If you
find Dukovo sending anything that is not listed, it is a bug: report it
as below.

## What stays on the machine

| What | Where | Kept |
| --- | --- | --- |
| The system's log and its audit log | `/system/state/log`, kept by logd | 90 days, and a year for the audit log, in at most 4 GiB or a tenth of their file system ("Logs, monitoring and troubleshooting") |
| The crash log | Reserved memory, shown at the next boot and kept in the log | Until the log trims it |
| Notifications | notifyd's memory only | The last 64, until notifyd stops |

A crash report goes nowhere by itself: to report a crash, copy what
"Debugging the kernel" asks for into the report yourself.

## What the machine sends on its own

Today, nothing: Dukovo's network stack is still being built, and no
service talks to the network by itself. These are the ones planned,
each to be listed here in full before it ships:

| Service | What it will send, and where | Roadmap |
| --- | --- | --- |
| netd | On a network you connect to, a DHCP request for an address, which carries the network card's hardware address; a static address sends none | Phase 12 |
| timed | A request for the time to a time server | Phase 12 |
| Discovery | The machine's name, announced on the local network only, so that `ssh name.local` works | Phase 12 |
| netlog | The log, to a machine you name, only when you turn it on to debug a machine | Phase 12 |
| updated | A check for updates at `updates.dukovo.org`, which can be turned off | Phase 13 |

Looking up a name (DNS) happens only when a program you run asks for
one.

## Reporting a security problem

Report a vulnerability privately, never in a public issue: open the
repository's Security tab on GitHub and choose **Report a
vulnerability**, as `SECURITY.md` in the source tree says. The
maintainers acknowledge a report within 3 working days, agree on a
disclosure date with you within 14, and release a fix and an advisory
within 90 days of the report.
