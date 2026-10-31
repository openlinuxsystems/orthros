notifications(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

notifications - list the notifications

# SYNOPSIS

*notifications* [*-f*] [*-n* _count_] [*--json*]

# DESCRIPTION

notifications lists the notifications *notifyd*(8) keeps, the last 64,
oldest first: when each came, in UTC, the program that sent it, its
urgency if it is not normal, its title, and its body's lines indented
(ADR 0212):

```
2026-10-30 23:40:01  notify-send (backup): Backup done
    All 3 disks are copied.
2026-10-30 23:41:15  notify-send [critical]: Disk full
```

The program is the one the kernel names, with the name it said it goes
by after it. notifyd keeps notifications only in memory, so the list
starts again when it does.

# OPTIONS

*-f*, *--follow*
	Then show each new notification as it comes, until interrupted.

*-n*, *--lines* _count_
	Only the newest _count_.

*--json*
	Each as notifyd sends it, a line of JSON (*notifyd*(8)).

# EXIT STATUS

0, or 1 if notifyd is not running.

# SEE ALSO

*notify-send*(1), *notifyd*(8)
