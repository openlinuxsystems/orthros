eject(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

eject - unmount a volume so that its disk can be pulled out

# SYNOPSIS

*eject* [*-v*] _volume_

# DESCRIPTION

eject unmounts a volume at _/volumes_, after the filesystem has written
out everything it holds, and removes its directory, so that its disk can
be pulled out safely (ADR 0163). The _volume_ is its label or name
under _/volumes_ (as *volumed*(8) or Spark mounted it), its path, or a
disk or partition by its name or its path in _/devices_; a disk's name
unmounts each of its partitions' volumes.

```
$ eject DUKOVOUSB
```

Once a volume is ejected, *volumed*(8) leaves its disk alone until it is
pulled out and plugged in again.

# OPTIONS

*-v*, *--verbose*
	Say what is unmounted.

# EXIT STATUS

0 when every volume named was unmounted; 1 when none is mounted at
_/volumes_, when one could not be unmounted (a file on it is open, or
what it holds could not be written), or for a mistake in the command
line.

# SEE ALSO

*volumed*(8), *findmnt*(8), *systools*(1)
