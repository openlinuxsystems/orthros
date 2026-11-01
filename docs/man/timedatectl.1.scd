timedatectl(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

timedatectl - the clock, the time zone, and how timed keeps the time

# SYNOPSIS

*timedatectl* [*status*]

*timedatectl timesync-status*

*timedatectl list-timezones*

*timedatectl set-timezone* _zone_

*timedatectl set-time* _time_

# DESCRIPTION

timedatectl shows the clock and the time zone and sets them, with the
commands of systemd's *timedatectl* (ADR 0194).

*status*, or no command, shows the local and universal time, the
real-time clock's, the time zone, whether *timed*(8) has synced the
clock, and whether it runs:

```
$ timedatectl
               Local time: Sun 2026-11-01 13:00:00 CET
           Universal time: Sun 2026-11-01 12:00:00 UTC
                 RTC time: Sun 2026-11-01 12:00:00
                Time zone: Europe/Berlin (CET, +0100)
System clock synchronized: yes
              NTP service: active
          RTC in local TZ: no
```

The real-time clock's time is shown only to a process with the clock
capability, as root's shells at the consoles and *elevate*(8) have
(*radix-capabilities*(7)); the real-time clock always keeps UTC.

*timesync-status*, or *show-timesync*, shows what timed did in its last
round: when and from which server it got the time, how far off the
clock was, the round trip, the server's stratum, what it did to the
clock, and when the next round is.

*list-timezones* lists the time zones of the IANA time zone database,
those of its _zone1970.tab_ and *UTC*, one a line.

*set-timezone* _zone_ makes _zone_, such as *Europe/Berlin* or
*America/New_York*, the machine's time zone: it points
_/system/settings/localtime_, which _/etc/localtime_ is, at the zone's
file in _/system/resources/zoneinfo_, which needs a write to
_/system/settings_, as root's. Programs started after it keep time in
that zone; those already running keep the zone they read first. The
*TZ* variable of a program's environment, a zone's name or a POSIX
rule such as *CET-1CEST,M3.5.0,M10.5.0/3*, overrides it for that
program.

*set-time* _time_ sets the clock to _time_, local time, as
*2026-11-01 12:00:00*, *2026-11-01 12:00*, *2026-11-01* (midnight) or
*12:00* (today), and writes the real-time clock. It needs the clock
capability, and refuses while timed runs, which keeps the clock from
the network: *service timed stop* first (*service*(8)). The clock takes
no time before 1980 or from 2500.

# EXIT STATUS

0 when it did what was asked; 1 when it could not, as for a zone that
is not in the database, a time it cannot read, a process without the
clock capability, or a wrong command.

# FILES

_/system/settings/localtime_
	The time zone, a link into the database; _/etc/localtime_ leads
	there.

_/system/resources/zoneinfo_
	The IANA time zone database, release 2026f, compiled by the tz
	project's *zic* (ADR 0194).

_/system/live/timed/status_
	What *timesync-status* shows, which timed writes.

_/system/live/timed/pid_
	timed's process number, while it runs.

# SEE ALSO

*timed*(8), *timed*(5), *coreutils*(1), *elevate*(8),
*radix-capabilities*(7), *systools*(1)
