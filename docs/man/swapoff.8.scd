swapoff(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

swapoff - turn swap areas off

# SYNOPSIS

*swapoff* [*-v*] _device_...

*swapoff -a* [*-v*]

# DESCRIPTION

swapoff turns off the swap area on each _device_, as util-linux's
*swapoff* does: every page swapped out to it is read back into memory
first, so it can take a while. An area whose pages do not fit in the
memory that is free stays on.

# OPTIONS

*-a*, *--all*
	Turn off every area on, as */processes/swaps* lists them.

*-v*, *--verbose*
	Say what is turned off.

# DIFFERENCES FROM LINUX

- An area is named by its device only, not by *LABEL=* or *UUID=*.

# EXIT STATUS

As util-linux's: 0 when every area was turned off; 2 when one's pages
did not fit in memory; 4 when one could not be turned off for another
reason; 16 for a mistake in the command line; with *-a*, 32 when none
could be turned off, and 64 when some could not.

# SEE ALSO

*swapon*(8), *mkswap*(8), *free*(1), *radix-block*(2)
