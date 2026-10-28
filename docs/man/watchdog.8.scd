watchdog(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

watchdog - the hardware watchdog Spark pets

# SYNOPSIS

*watchdog*

# DESCRIPTION

watchdog shows the machine's hardware watchdog, which restarts the
machine if Spark stops petting it (*spark*(8)'s *WATCHDOG*), from
_/kernel/class/watchdog/watchdog0_ (*radix-sysfs*(5)), as util-linux's
wdctl shows Linux's: what it is, whether it runs, its timeout and the
timeouts it takes, and whether it restarted the machine at the end of
the last boot.

```
$ watchdog
Device:    the Intel 6300ESB watchdog
State:     running, petted by Spark
Timeout:   60 seconds
Timeouts:  1 to 2082 seconds
Last boot: the last boot did not end with it
```

Linux's watchdog(8) is a daemon that pets _/dev/watchdog_; on Dukovo
Spark does that itself.

# EXIT STATUS

0 if it showed the watchdog, 1 if the kernel drives none or for an
argument, which it takes none of.

# SEE ALSO

*spark*(8), *radix-sysfs*(5), *systools*(1)
