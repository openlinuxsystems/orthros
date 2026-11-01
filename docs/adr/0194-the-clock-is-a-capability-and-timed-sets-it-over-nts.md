<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0194. The clock is a capability, and timed sets it over NTS

- Status: Accepted
- Date: 2026-11-01

## Context

The roadmap's 12a asks for `timed`: "Network Time Security (NTS) by
default with plain SNTP as a fallback, writes the RTC; ships the IANA
time zone database, public domain, and Unicode CLDR locale data,
Unicode license; English is the 1.0 language".

Until now the kernel read the real-time clock at boot and nothing could
set the time: mlibc's `clock_settime` failed with EPERM. A clock that
is wrong breaks TLS, whose certificates are checked by date, logs and
anything that compares times across machines. Plain NTP and SNTP are
not authenticated: whoever is on the network path can set a client's
clock, and with it make an expired certificate valid again. NTS (RFC
8915) authenticates NTP: a TLS 1.3 connection to the server's key
establishment port gives the client keys and cookies, and each NTP
request and answer is then sealed with AES-SIV. Cloudflare, Netnod and
the German PTB run public NTS servers; pool.ntp.org does not offer NTS.

mlibc's time zone code read only version 1 data from a time zone file,
which zic's "slim" files, its default since 2020, leave empty, so
`localtime` gave the IANA database's zones their local mean time,
hours off. Linux reads the zone from `/etc/localtime`, a link into
`/usr/share/zoneinfo`, which `timedatectl set-timezone` points. mlibc
reads compiled glibc locales (ADR 0073), which only C.UTF-8 had.

## Decision

- **The clock is a capability, kind 21** (`clock`, radix-clock(2),
  interface 0x30): `state` reads the clock, its slew and the real-time
  clock with *read*; `set`, `slew` and `write_rtc` need *control*. `set`
  takes times from 1980 to 2500, writes the real-time clock too, and the
  kernel logs each step, with the process that made it; `slew` moves
  the clock by up to 128 ms at 500 µs a second, as Linux's adjtime(3),
  so that it never jumps or runs backwards. Spark holds it, and gives
  it with the read and control rights to services whose files ask for
  `admin.time` (service.toml(5)): timed, consoled for root's shells, and
  authd, which hands it to elevate(8)'s commands, as Linux gives
  CAP_SYS_TIME. mlibc's `clock_settime(CLOCK_REALTIME)` uses it, and
  fails with EPERM without it. A bundle's manifest may ask for
  `admin.time`, which only an administrator gives, through elevate.
- **timed(8)** asks the NTS servers of `/system/settings/timed`
  (timed(5)), or time.cloudflare.com, nts.netnod.se and
  ptbtime1.ptb.de, three organizations in three countries, in turn;
  only while none answers does it ask `fallback`'s plain NTP servers,
  pool.ntp.org by default, and it logs that the time is not
  authenticated. Each round takes four samples of the first server that
  answers and keeps the one with the shortest round trip; an offset past
  128 ms steps the clock, one past 0.5 ms slews it. The real-time clock
  is written with each step and every eleven minutes. Rounds start
  every 64 seconds and double to 1024 while the clock stays close, and
  come back in 32 seconds after a failure.
- **A wrong clock does not lock timed out.** At start timed moves a
  clock that is behind the time it last synced, kept in
  `/system/state/timed`, forward to it. If a server's certificate is not
  valid by the clock before timed first syncs, it asks that server's NTP
  for the time once, unauthenticated, and checks the certificate at that
  time, refusing a time before the last sync, so that an attacker
  cannot make an expired certificate valid by setting the clock back;
  the time it then sets is NTS's.
- **NTS's AES-SIV is RustCrypto's** `aes-siv` crate (Apache-2.0 OR
  MIT), as ring has none. NTP and NTS-KE messages are in netproto, the
  library resolved uses too (ADR 0193).
- **timedatectl(1)** shows the clock, the real-time clock, the zone and
  whether timed has synced, with systemd's commands: `timesync-status`
  shows timed's last round, `set-timezone` points
  `/system/settings/localtime`, which `/etc/localtime` is, into the
  database, `set-time` sets the clock while timed is not running.
- **The IANA time zone database** (tzdata 2026f, public domain) is a
  port, compiled by its own zic in the slim format, into
  `/system/resources/zoneinfo`, which `/usr/share/zoneinfo` leads to, with
  its links as symbolic links and its tables (zone1970.tab and the
  others). The default `/etc/localtime` is UTC's.
- **mlibc reads time zones with musl 1.2.5's code** (patch 0051,
  `ports/mlibc/musl/timezone.hpp`, MIT): version 2+ files' 64-bit data
  and the TZ rule at their end for the times after their last
  transition, POSIX TZ rules, and the database's search path, with
  `/system/resources/zoneinfo` first. It reads a file whole, 64 KiB at
  most, and checks the counts, indexes and names in it before it uses
  them. musl's `timezone` came out wrong for zones east of UTC (an
  unsigned negation); that is fixed, and the patch is meant for
  upstream.
- **English's locales, from glibc's locale data, not CLDR's.** The
  locales port compiles glibc 2.42's English sources for each of its 19
  countries, en_US.UTF-8, en_GB.UTF-8, en_IN.UTF-8 and the others, with
  the host's localedef into the files mlibc reads, as C.UTF-8 was;
  identical files become symbolic links, about 6 MB in all. The roadmap
  names Unicode's CLDR: mlibc reads glibc's compiled format and nothing
  converts CLDR to it, while glibc's sources carry the same dates,
  numbers and sorting order for English, and the Free Software
  Foundation claims no copyright in them; their character tables are
  the Unicode Character Database's, under the Unicode License v3. CLDR
  can come with the desktop's own formatting (ICU4X) and the
  translations after 1.0. This departs from the roadmap's wording, and
  the pull request asks for the maintainer's word on it.

## Consequences

- The clock is right within milliseconds a minute after boot on any
  network that reaches one of the three servers, and nobody on the path
  can change it. A network that blocks NTS's port 4460 gets plain NTP,
  and the log says so; an administrator who wants none sets
  `fallback =`.
- Every program has the IANA database's zones and English's formats:
  `TZ=America/New_York date`, `LANG=en_GB.UTF-8`, and strftime's `%x`.
- Not yet: timers armed by `CLOCK_REALTIME` before a step fire at their
  old time, not the new; timed does not train the clock's frequency, so
  a fast or slow clock is slewed again each round; it takes the first
  server that answers instead of choosing among several by their
  agreement (NTP's selection); its plain-NTP fallback is SNTP, not
  authenticated; it serves no time to others; and leap seconds are not
  smeared or announced.
- Tests: netproto's NTP and NTS tests (timestamps across era 1, NTS-KE
  records, sealing and opening, a changed answer and another request's
  refused), timed's (settings, and samples taken from good answers
  only: no kiss of death, no unsynchronized server), the posix suite's
  new `zones` part (localtime, mktime, %Z and %z in seven zones before
  1970 and after 2037, TZ rules, `/etc/localtime`, and
  English's dates, numbers and sorting order, checked against glibc),
  and the `network-services` boot test, whose NTS server is five
  seconds ahead: timed must step the clock by it, the kernel log it, and
  the next round leave the clock as it is.

## Alternatives considered

- **chrony or ntpd-rs:** chrony is GPL C, the full NTP with frequency
  training, more than a client needs before 1.0; ntpd-rs, Rust with NTS,
  brings tokio and a daemon of its own design for the same result.
  Either may replace timed if training the frequency matters.
- **settimeofday for root:** the Linux way, but being root gives no
  power of its own here, only what a process's capabilities hold (ADR
  0214); a capability says exactly who may set the time.
- **Fat time zone files:** they hold every transition to 2037 and need
  no TZ rule, but are three times the size, and still need musl's code
  past 2037.
- **CLDR now:** a converter from CLDR's XML to glibc's locale sources,
  or a locale format of our own for mlibc, before any program needs
  more than English.
