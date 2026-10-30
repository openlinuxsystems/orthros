log(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

log - how much of the system's log logd keeps

# SYNOPSIS

_/system/settings/log_

# DESCRIPTION

This file says how long and how much of the system's log *logd*(8)
keeps, and how long it keeps the security events of the audit log (ADR
0211). Each line is *setting = value*; a line starting with *#* is a
comment, and blank lines are skipped. A setting the file does not give
keeps its default, and 0 lifts a limit. *service logd reload* reads the
file again; logd says in the log what it could not read.

*keep-days =* _days_
	How many days the log keeps records: 90 unless set.

*max-size =* _size_
	The most room the log and the audit log take together: bytes, or
	with *K*, *M*, *G* or *T* after them (KiB and so on), or *KB*,
	*MB*, *GB*, *TB*. 4G unless set.

*max-percent =* _percent_
	The most room they take as a share of their file system, 0 to 100:
	10 unless set. The smaller of the two limits counts.

*audit-keep-days =* _days_
	How many days the audit log keeps security events: 365 unless set.

When the log and the audit log take more room than the limits allow,
logd removes the oldest day files of the log first, then of the audit
log, but never today's or one it is writing.

# EXAMPLE

Keep the log for good, in at most 20 GiB:

```
keep-days = 0
max-size = 20G
max-percent = 0
```

# SEE ALSO

*logd*(8), *logtrim*(8), *log*(1)
