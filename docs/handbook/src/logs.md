<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Logs, monitoring and troubleshooting

This chapter describes the system's log and its notifications as of
Phase 13 of the roadmap. The full references are logd(8), log(1),
journalctl(1), logger(1), logtrim(8), log(5), dmesg(1), notifyd(8),
notify-send(1) and notifications(1).

## What the log holds

Everything that says something on Dukovo says it to the kernel log:
the kernel itself, the services, whose output Spark copies there line
by line, and programs that write to `/devices/kmsg`. The kernel keeps
only its newest 512 records, so the logd service keeps every record for
good, as it comes, in `/system/state/log`: one file a day, each record a
line of JSON.

Each record says when it was made, in which boot, how urgent it is
(error, warning, info or debug), and who wrote it: the kernel, a service
by its name, or a program by its name, its process number and the user
it ran as. Who wrote a record is what the kernel says, not what the
record says, so a program cannot pass its lines off as a service's, or
as another user's.

A script writes a line with `logger`: `logger -t backup "All 3 disks are
copied."` and `log backup` shows it.

## Who reads what

The log is for administrators, as on Ubuntu, where only the `adm` group
reads the whole journal and `dmesg` needs root. Everyone reads the
records of their own programs:

| Who | `log` and `journalctl` | `log --audit`, `log --disk-usage`, `logtrim` | `dmesg` |
| --- | --- | --- | --- |
| Anyone | The records of their own programs | No | No |
| An administrator, with `elevate` | Every record | Yes | Yes |

So an administrator types `elevate log -p error` or `elevate dmesg`.
Without `elevate`, `log` adds a hint on its standard error that it shows
only your own programs' records; `-q` leaves it out. Anyone can still
write to the log.

## Reading it

`log` shows this boot's records, oldest first:

```
$ log
Oct 30 21:25:01 dukovo kernel: Radix 0.13.0 on 4 CPUs
Oct 30 21:25:02 dukovo spark: started volumed (/system/commands/volumed) as process 14
Oct 30 21:25:02 dukovo volumed: watching for volumes; 5 disks and partitions now
```

Narrow it down with options, which all must hold together:

| To see | Type |
| --- | --- |
| One service's or program's records | `log volumed`, or `log -s volumed -s consoled` |
| Errors only, or warnings and errors | `log -p error`, `log -p warning` |
| The last few records | `log -n 20` |
| New records as they come | `log -f` (Ctrl+C stops) |
| A time range | `log --since "2026-10-30 08:00" --until -1h`, `log --since today` |
| Records whose text matches | `log -g "disk|volume"` |
| The kernel's records alone | `log -k` |
| The boot before this one | `log -b -1`; `log --list-boots` lists them |
| Every boot | `log --all` |
| The records as JSON, for scripts and `jq` | `log --json` |

`journalctl` takes systemd's options, so what you type on Linux works:
`journalctl -u volumed.service -b`, `journalctl -k`, `journalctl -f`,
`journalctl -p err --since today`. `elevate dmesg` still reads the
kernel's ring directly, and `dmesg -k` or `dmesg -u` shows only the
kernel's records or only the programs'.

## The audit log

Security events are kept twice: in the log, and in the audit log, which
keeps them for a year in `/system/state/log/audit`. Today they are
Spark's answers to requests that change something (a service started,
stopped or restarted, the machine powered off), each with the process
that asked; the power button; the kernel log cleared; records the
kernel's ring lost before logd read them; the log trimmed; accounts
made, passwords changed and wrong passwords; signing in and out; and
each command run with `elevate`, and each refusal. Permissions will be,
as they come.

```
$ elevate log --audit
Oct 30 21:40:12 dukovo spark: asked to restart volumed, by process 57
```

Only the kernel and Spark can put an event in the audit log. A service
writes one by starting a line of its output with `<86>`, the syslog
priority of a security event, and Spark passes it on as that service's;
a program that writes the same to the kernel log only adds a line to
the log. `journalctl _TRANSPORT=audit` reads the audit log too.

## How much space it takes

`log --disk-usage` says how much the log takes, and the audit log's
share. logd keeps 90 days of the log and a year of the audit log, in at
most 4 GiB or a tenth of their file system, whichever is less, and
removes the oldest days first, each removal a security event. Change
the limits in `/system/settings/log` and run `service logd reload`;
0 lifts a limit:

```
keep-days = 30
max-size = 1G
```

`logtrim` trims now (`logtrim -n` says what it would remove), and
`logtrim --size 500M` or `logtrim --days 30` once to a limit of your
own, as `journalctl --vacuum-size=500M` and `--vacuum-time=30d` do. The
audit log keeps to its own setting.

## Notifications

A program or a script can tell whoever is at the screen something with
`notify-send`, as on Linux; the console shows it on its bottom line for
8 seconds, after the name of the program that sent it:

```
$ notify-send -a backup 'Backup done' 'All 3 disks are copied.'
```

```
 notify-send (backup): Backup done - All 3 disks are copied.
```

`-t` sets the time in milliseconds, and `-u critical` keeps it, in
red, until you press a key. The name in front is the one the kernel
gives, so a program cannot pass a notification off as another's; a
name it gives itself with `-a` comes after it. `notifications` lists
the last 64, and `notifications -f` follows them. The notifyd service
keeps them in memory only, never on the disk or in the log, and
refuses more than 10 in 10 seconds from one program.

## When logd is not running

`log` and `journalctl` read through logd. If it has stopped, `service
logd status` says why, `service logd start` starts it again, and
`elevate dmesg` still shows the kernel's newest records. Started again, logd goes on
from the last record it kept, so nothing is kept twice.
