mkswap(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

mkswap - make a swap area on a disk or partition

# SYNOPSIS

*mkswap* [*-L* _label_] [*-U* _uuid_] [*-f*] [*-q*] _device_ [_size_]

# DESCRIPTION

mkswap writes the header of a Linux swap area, version 1, for pages of
4 KiB, at the start of _device_, as util-linux's *mkswap* does: a disk
or partition in */devices*, or a file. The area is every whole page of
the device, or the first _size_ KiB of it; it needs at least 10 pages
(40 KiB). Linux and Dukovo both take it: *swapon*(8) turns it on.

The first 1024 bytes are cleared, unless they hold a partition table
(the device is a whole disk with partitions), which is kept, as
util-linux keeps it. Btrfs's superblocks further in are cleared too, so
that the device is not taken for one.

mkswap prints the area's size and its label and UUID:

```
Setting up swapspace version 1, size = 256 MiB (268431360 bytes)
no label, UUID=8e2d7c7b-46a2-4e0c-9d7c-bd0a1f6e5a3e
```

# OPTIONS

*-L* _label_, *--label* _label_
	Give the area a label, of up to 16 bytes.

*-U* _uuid_, *--uuid* _uuid_
	Give the area this UUID, as 8-4-4-4-12 hexadecimal digits, in place
	of a random one.

*-f*, *--force*
	Clear the first 1024 bytes even if they hold a partition table, and
	take a _size_ larger than the device.

*-q*, *--quiet*
	Print nothing but errors.

# DIFFERENCES FROM LINUX

- Pages are 4 KiB: *-p* takes only 4096, and *-v* only 1.
- *-c* (look for bad blocks first) is not there yet; a header made
  elsewhere with bad pages listed is taken, and those pages are left out.
- An area whose swap area is on refuses to be written: mkswap fails
  with _Device or resource busy_.

# EXIT STATUS

0 when the area was made; 1 when it was not, and for a mistake in the
command line.

# SEE ALSO

*swapon*(8), *swapoff*(8), *free*(1), *radix-block*(2)
