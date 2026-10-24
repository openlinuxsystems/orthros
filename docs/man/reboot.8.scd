reboot(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

reboot, poweroff, halt - restart, power off or halt the machine

# SYNOPSIS

*reboot* [*-f*] [*--poweroff* | *--halt*]

*poweroff* [*-f*] [*--reboot* | *--halt*]

*halt* [*-f*] [*-p*] [*--reboot* | *--poweroff*]

# DESCRIPTION

Each asks Spark, over its control socket _/run/spark/control_
(*spark*(8)), to stop the system now: Spark stops the services, the most
recently started group first, giving each group 5 seconds to end after
SIGTERM before it stops them, and then restarts the machine, powers it
off, or halts it. The Radix kernel writes back every filesystem's
changes first (*radix-power*(2)).

*halt* stops the processors and leaves the machine on, with the
message *the system is halted; it is safe to turn the machine off* on
the screen and in the log. A machine the kernel cannot power off, one
without ACPI, stays running when asked to: *poweroff* says so.

The options are systemd's; sysvinit's *-w*, *-d*, *-n*, *--wtmp-only*,
*--no-wtmp*, *--no-sync* and *--no-wall* are taken and change nothing,
since there is no wtmp file and the kernel always writes the disks back.

# OPTIONS

*-f*, *--force*
	Stop at once, without stopping the services. The filesystems are
	still written back.

*--reboot*, *--poweroff*, *--halt*
	Restart, power off or halt, whatever the command's name.

*-p*
	With *halt*: power off instead.

*--help*
	Show the options.

# EXIT STATUS

0 when Spark took the request, 1 when it refused it or could not be
reached, 2 for an option it does not know.

# KEYS

On the virtual consoles (*consoled*(8)), Ctrl+Alt+Del runs *reboot*.

# FILES

_/run/spark/control_
	Spark's control socket.

# SEE ALSO

*shutdown*(8), *spark*(8), *consoled*(8), *radix-power*(2), *systools*(1)
