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

*netd*
	A DHCP request on each network card, from boot, asking for an
	address, router and DNS servers, and carrying the card's hardware
	(MAC) address as its client identifier, and no host name; then a
	renewal at half the lease's time (*netd*(8)). None for a card with
	addresses of its own, or that *network*(5) has netd leave alone.

*resolved*
	Only the names programs ask for, to the DNS servers that
	*resolved*(5) names or else DHCP gave, over DNS over TLS when they
	take it, so that the network on the way does not read them
	(*resolved*(8)); _localhost_, _.local_ and _invalid_ names never
	leave the machine. It sends nothing on its own.

*timed*
	A request for the time, over Network Time Security, to
	_time.cloudflare.com_, _nts.netnod.se_ or _ptbtime1.ptb.de_, the
	first that answers, every 64 seconds to 17 minutes: a TLS
	connection, then NTP packets with random numbers and the cookies
	the server gave, which say nothing of the machine; and to
	_pool.ntp.org_ over plain NTP only while none of them answers
	(*timed*(8)). Another server is named, or none, in *timed*(5);
	*enabled = false* in its service file, *service edit timed*
	(*service*(8)), turns it off.

*netlog*
	The log, to a computer the user names, only when *netlog*(5) or
	the *netlog=* flag (*dukovo-boot*(7)) names one, to debug a
	machine; by default it sends nothing (*netlog*(8)).

Planned, each to be described here in full before it ships:

*Discovery* (Phase 12)
	The machine's name, announced on the local network only.

*updated* (Phase 13)
	A check for updates at _updates.dukovo.org_, which can be turned
	off.

# REPORTING A SECURITY PROBLEM

Privately, through the repository's GitHub Security tab (*Report a
vulnerability*), as _SECURITY.md_ in the source tree says; never in a
public issue.

# SEE ALSO

*logd*(8), *log*(5), *netd*(8), *netlog*(8), *notifyd*(8),
*radix-debug*(7), *resolved*(8), *timed*(8)
