dukovo-privacy(7) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

dukovo-privacy - what Dukovo keeps and what it sends

# DESCRIPTION

Dukovo's privacy promise (ADR 0212):

- *No telemetry.* Dukovo collects nothing about how it is used. No part
  of it sends usage figures, crash reports, identifiers or anything else
  about its user or the machine to anyone.
- *Nothing leaves the machine unless the user starts it*, but for what a
  network needs to work and the update checks, each listed below. A
  program the user runs sends what it is asked to; Dukovo itself sends
  nothing on its own.
- *No account and no ads.* Dukovo needs no online account, and shows no
  ads.

Each service that sends something on its own is listed here, with what
it sends, where and how to turn it off, before it ships. Anything sent
that is not listed is a bug.

# KEPT ON THE MACHINE

*The log and the audit log*
	In _/system/state/log_, by *logd*(8): 90 days of the log and a year
	of the audit log, in at most 4 GiB or a tenth of their file system
	(*log*(5)).

*The crash log*
	In reserved memory, shown at the next boot and kept in the log
	(*radix-debug*(7)). It goes into a report only if the user copies
	it there.

*Notifications*
	In *notifyd*(8)'s memory only, the last 64, until it stops.

# SENT ON ITS OWN

Nothing yet: no service talks to the network by itself. Planned, each to
be described here in full before it ships:

*netd* (Phase 12)
	A DHCP request on a network the user connects to, carrying the
	network card's hardware address; none with a static address.

*timed* (Phase 12)
	A request for the time to a time server.

*Discovery* (Phase 12)
	The machine's name, announced on the local network only.

*netlog* (Phase 12)
	The log, to a machine the user names, only when turned on to debug
	a machine.

*updated* (Phase 13)
	A check for updates at _updates.dukovo.org_, which can be turned
	off.

A name is looked up (DNS) only when a program asks for one.

# REPORTING A SECURITY PROBLEM

Privately, through the repository's GitHub Security tab (*Report a
vulnerability*), as _SECURITY.md_ in the source tree says; never in a
public issue.

# SEE ALSO

*logd*(8), *log*(5), *notifyd*(8), *radix-debug*(7)
