iostat(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

iostat - what the CPUs and the disks have done

# SYNOPSIS

*iostat* [*-cdkmtxyz*] [_disk_...] [_interval_ [_count_]]

# DESCRIPTION

iostat reports, as sysstat's iostat does, the share of time the CPUs
spent on programs, in the kernel and idle (from _/processes/stat_,
*radix-proc*(5)), and each disk's requests and bytes per second (from
_/kernel/block/_*disk*_/stat_, *radix-sysfs*(5)): since boot, then, with
an _interval_ in seconds, over each interval, _count_ times or until
stopped. A _disk_ named, as _vda_ or _/devices/vda_, is shown alone.

```
$ iostat -d
Device             tps    kB_read/s    kB_wrtn/s    kB_dscd/s    kB_read    kB_wrtn    kB_dscd
vda              12.40       291.31        40.12         0.00      52344       7210          0
```

Radix counts no nice, I/O wait or stolen time, which show *0.00*, and
merges no requests, so the merge columns of *-x* are *0.00*. A call
that moves many buffers at once counts as one request.

# OPTIONS

*-c*
	The CPUs only.

*-d*
	The disks only.

*-x*
	The extended columns: per kind of request (*r*, *w*, *d* for
	discards, *f* for flushes), the requests and kB per second, those
	merged, the average wait in milliseconds and the average size in
	kB; then the average requests in flight (*aqu-sz*) and the share of
	the time with one in flight (*%util*).

*-k*, *-m*
	kB, the default, or MB.

*-t*
	The time of each report.

*-y*
	With an _interval_, leave out the first report, since boot.

*-z*
	Leave out the disks that did nothing in the interval.

# SEE ALSO

*vmstat*(1), *pidstat*(1), *fstrim*(8), *radix-sysfs*(5), *systools*(1)
