<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Logs, monitoring and troubleshooting

This chapter describes the system's log as of Phase 13 of the roadmap.
The full references are logd(8), log(1), journalctl(1) and dmesg(1).

## What the log holds

Everything that says something on Dukovo says it to the kernel log:
the kernel itself, the services, whose output Spark copies there line
by line, and programs that write to `/devices/kmsg`. The kernel keeps
only its newest 512 records, so the logd service keeps every record for
good, as it comes, in `/system/state/log`: one file a day, each record a
line of JSON.

Each record says when it was made, in which boot, how urgent it is
(error, warning, info or debug), and who wrote it: the kernel, a service
by its name, or a program by its name and process number. Who wrote a
record is what the kernel says, not what the record says, so a program
cannot pass its lines off as a service's.

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
`journalctl -p err --since today`. `dmesg` still reads the kernel's ring
directly, and `dmesg -k` or `dmesg -u` shows only the kernel's records or
only the programs'.

## How much space it takes

`log --disk-usage` says how much the log takes. Trimming the oldest days
by size and age comes with the next part of Phase 13.

## When logd is not running

`log` and `journalctl` read through logd. If it has stopped, `service
logd status` says why, `service logd start` starts it again, and `dmesg`
still shows the kernel's newest records. Started again, logd goes on
from the last record it kept, so nothing is kept twice.
