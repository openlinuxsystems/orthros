netlog(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

netlog - send the log to another computer as it is written

# SYNOPSIS

*netlog*

# DESCRIPTION

netlog sends the system's log over UDP to another computer as it is
written (ADR 0196), as Linux's *netconsole* does, for a machine whose
screen shows nothing, or that hangs before its log is saved. Spark runs
it as the *netlog* service, after *resolved*(8) (*spark*(8)). It sends
nothing until a target is named.

The target is *netlog=*_host_[*:*_port_] on the kernel command line,
for one boot (*dukovo-boot*(7)), or else *target* in
_/system/settings/netlog_ (*netlog*(5)); the port is 6666, netconsole's,
unless it is given. A _host_ that is a name is looked up through
resolved.

The log is the kernel's: its own records, the lines programs write to
it (*logger*(1)), and the lines of the services' output, which Spark
copies there (*logd*(8)). netlog reads _/devices/kmsg_ from the boot's
first record the kernel still holds, and sends each record as one
datagram: a line of text with its *syslog*(3) priority in front, as a
syslog server takes it, and its time since boot, as *dmesg*(1) shows
it, cut to 1400 bytes:

```
<6>[    2.104511] Net: net0 is up, at 10.0.2.15/24
```

A record that cannot be sent yet, before the network is up, waits, and
is sent once it can be, so that the boot's first records reach the
target too; records the kernel dropped before they were read are lost.
Records go at most a thousand a second, so that the hundreds the kernel
holds when netlog starts do not overrun the listener's queue at once.
A target whose port is closed loses the records sent to it.

On the other computer, *nc -klu 6666* shows what comes, and so does a
syslog server listening on that port. The datagrams are not encrypted
or authenticated: name a target on a network that may read the log.

# SIGNALS

*SIGHUP*
	Read the settings and the kernel command line again: *service
	netlog reload* (*service*(8)) sends it.

*SIGTERM*
	End.

# FILES

_/system/settings/netlog_
	The settings (*netlog*(5)).

_/devices/kmsg_
	The kernel log, which netlog reads.

_/system/settings/spark/services/netlog.toml_
	Its service, which holds the kernel log to read and the network to
	send through, beyond what every service holds (*service.toml*(5)).

# EXIT STATUS

0 when it was ended; 1 for an argument, as it takes none, or when the
kernel log cannot be read.

Not yet: records sent from the kernel itself, before Spark starts
netlog or after the system has stopped, as netconsole sends them from
the network driver; and IPv6 targets (Phase 12d of the roadmap).

# SEE ALSO

*netlog*(5), *dukovo-boot*(7), *dmesg*(1), *logd*(8), *logger*(1),
*dukovo-privacy*(7), *service*(8), *spark*(8), *systools*(1)
