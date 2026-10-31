remount(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

remount - mount a volume again, to write or only to read

# SYNOPSIS

*remount* [*-w*|*-r*] [*--discard-hibernation* [*-y*]] _volume_

# DESCRIPTION

remount mounts a volume at _/volumes_ again, from its disk, with its
filesystem: to write it (the default), or only to read it, as Linux's
*mount -o remount* does. The _volume_ is named as *eject*(1) names one:
its label or name under _/volumes_, its path, or its disk or partition.

A volume whose filesystem refuses writes is mounted read-only again, as
*volumed*(8) mounts it; the kernel's log says why (*dmesg*(1)).

## Windows' hibernation

Windows hibernates its volumes when it hibernates, and when it shuts down
with Fast Startup, Windows' default: what it holds in memory of each
volume is written back when it starts again, over whatever another system
wrote. So Dukovo only reads an NTFS that Windows hibernated with (ADR
0237). The best way to write one is to start Windows and shut it down
fully: hold Shift while choosing Shut down.

When that cannot be done, *--discard-hibernation* writes it anyway, as
ntfs-3g's *remove_hiberfile* does (ADR 0238). remount says what is lost
and asks first. Windows' hibernation is thrown away: Windows then starts
afresh, and what it had open, its programs and what it had not yet
written to the volume, is lost. The volume stays marked to be checked,
and Windows checks it at its next start.

```
# remount --discard-hibernation WINDOWS
```

Mounting needs the administrator: run remount with *elevate*(8).

# OPTIONS

*-w*, *--write*
	Mount it to write; the default.

*-r*, *--read-only*
	Mount it only to read.

*--discard-hibernation*
	Write an NTFS that Windows hibernated with, throwing the hibernation
	away, after the warning above. A volume Windows did not hibernate
	with is mounted as without it.

*-y*, *--yes*
	Do not ask before throwing a hibernation away.

# EXIT STATUS

0 when the volume is mounted as asked; 1 when it is not at _/volumes_,
when a name stands for more than one volume, when its filesystem is
only read although writing was asked, when the question was answered
no, or for a mistake in the command line.

# SEE ALSO

*eject*(1), *volumed*(8), *elevate*(8), *radix-files*(7), *systools*(1)
