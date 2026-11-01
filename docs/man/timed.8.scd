timed(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

timed - keep the clock right from the network

# SYNOPSIS

*timed*

# DESCRIPTION

timed sets the system's clock by time servers on the network (ADR
0194), as *chronyd* or systemd's *systemd-timesyncd* does on Linux.
Spark runs it as the *timed* service, after *resolved*(8), with
*admin.time*, which gives it the clock capability (*spark*(8),
*service.toml*(5)).

It asks the servers that _/system/settings/timed_ names (*timed*(5)),
or else Cloudflare's _time.cloudflare.com_, Netnod's _nts.netnod.se_
and PTB's _ptbtime1.ptb.de_, three organizations in three countries,
over Network Time Security (NTS, RFC 8915). A TLS 1.3 connection to the
server's key establishment port, 4460, with its certificate checked
against the machine's authorities (*dukovo-certificates*(7)), gives
timed two keys and eight cookies. Each NTP request then carries a
cookie and a random identifier, and is sealed with the keys (AES-SIV),
and each answer must carry the same identifier, be sealed with the
other key, and bring a new cookie, so that nobody on the way can
change the time timed is told, or follow the machine from one network
to the next by its cookies.

Only while no NTS server answers does timed ask the plain NTP servers
of *fallback*, _pool.ntp.org_ unless the settings say otherwise, over
SNTP (RFC 4330), which is not authenticated, and it says so in the
log. *fallback =* with nothing after it asks none.

## Setting the clock

Each round asks the first server that answers four times and keeps the
answer with the shortest round trip, as NTP's clock filter does. An
answer from a server further than 1.5 seconds from its reference clock
(its root distance) is not taken. An offset past 128 ms steps the
clock, setting it at once; a smaller one, past half a millisecond,
slews it, running it a little faster or slower until it is right, so
that it never jumps or runs backwards for small corrections; one under
half a millisecond is left as it is. The kernel logs each step, *Time: timed set the
clock to* _time_ *UTC (+5000 ms), and the real-time clock*. The
real-time clock is written with each step and every eleven minutes, as
Linux's eleven-minute mode does.

Rounds start every 64 seconds, and grow to every 17 minutes while the
clock stays close; after a step they start again at 64 seconds, and
after a round that failed they come again in 32 seconds.

## A clock far from right

A clock far from right fails the checks of certificates: a real-time
clock whose battery is flat may say 2000. At its start timed sets a
clock that is behind the time it last synced, which it keeps in
_/system/state/timed_, forward to that. If a server's certificate is
still not valid by the clock before timed first syncs, it asks the same
server for the time over plain NTP once, and checks the certificate by
that time instead, unless that time is before it last synced; if the
certificate passes, it goes on over NTS, and the time it sets is
authenticated.

## What it shows

*timedatectl timesync-status* (*timedatectl*(1)) shows
_/system/live/timed/status_, which timed writes after each round:

```
Synchronized: yes, at 2026-11-01 12:00:00 UTC, from time.cloudflare.com over NTS
Offset: +0.412 ms, delay 11.208 ms, stratum 3: the clock is +0.412 ms off; left as it is
Next round: in 128 s
```

What it does goes to its standard output and error, which Spark copies
into the system's log after *timed:*.

# SIGNALS

*SIGHUP*
	Read the settings again and start a round: *service timed reload*
	(*service*(8)) sends it.

*SIGTERM*
	End, leaving the clock as it is.

# FILES

_/system/settings/timed_
	The settings (*timed*(5)).

_/system/state/timed/last_
	When it last synced, in seconds since 1970.

_/system/live/timed/status_
	What *timedatectl timesync-status* shows.

_/system/live/timed/pid_
	Its process number, while it runs.

_/system/settings/spark/services/timed.toml_
	Its service (*service.toml*(5)).

# EXIT STATUS

0 when it was ended; 1 for an argument, as it takes none.

Not yet: timers set by the real time (*CLOCK_REALTIME*) before a step
fire by the time they were set for, not the new one; the clock's
frequency is not trained, so it is slewed again each round instead;
the first server that answers is taken, not chosen among several by
their agreement (NTP's selection); and timed serves no time to others.

# SEE ALSO

*timed*(5), *timedatectl*(1), *resolved*(8), *dukovo-certificates*(7),
*dukovo-privacy*(7), *service*(8), *spark*(8), *systools*(1)
