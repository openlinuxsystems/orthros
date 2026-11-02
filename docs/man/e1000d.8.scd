e1000d(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

e1000d - drive Intel's e1000 and e1000e network cards as a program

# SYNOPSIS

*e1000d*

# DESCRIPTION

e1000d drives the Intel e1000 and e1000e network cards (*e1000*(4)) that
the kernel leaves for a driver program, with the kernel's own driver for
them running in a process of its own (ADR 0198). A bug in the driver, or
a card that misbehaves, then ends a program that Spark starts again, not
the kernel.

The kernel leaves a card only with the boot flag *e1000=program*
(*dukovo-boot*(7)), which the images *ob image* builds have, and only if
the IOMMU confines the card to its own memory, with interrupt remapping
on: the card can then reach nothing but the memory the kernel gave it,
and raise no interrupt but its own. Spark runs e1000d as the *e1000d*
service, whose file names Intel's cards (*devices.pci = ["8086:\*"]*,
*service.toml*(5)), and gives it each card the kernel left
(*spark*(8)). Without such a card, Spark does not start it.

For each card, e1000d maps its registers and memory the card reaches,
both through the card's device capability (*radix-device*(2)), takes
its interrupt (an MSI on the PCI Express e1000e; the PCI 8254x are
polled, every millisecond while frames move and every 10 ms when quiet,
as the kernel polls them), resets and starts the card, and attaches it
to its network interface. A thread for each card then moves the frames
the card received to the kernel's network stack, and the frames the
stack sends to the card, through a link of 256 slots each way that the
two share, and tells the stack of the card's link:

```
e1000d: net0 is the 82540EM at 0000:00:03.0, MAC 52:54:00:12:34:56, polled
```

e1000d never touches a card's configuration space, which stays the
kernel's. When it ends, however it ends, the kernel stops the card's
bus mastering and its interrupt first, then takes back its memory, and
puts the interface down; the interface keeps its addresses, and comes
back up when Spark has started e1000d again, which it does at once
after a run of 10 seconds or more, and after a wait that doubles
otherwise. A card that stops with an error, or a link the kernel stops
using because e1000d broke its rules, ends e1000d with an error, so
that it starts again.

# EXIT STATUS

e1000d does not end by itself while its cards run. It exits with 1 when
Spark gave it no card, when a card does not start, or when a card
stops.

# SEE ALSO

*e1000*(4), *service.toml*(5), *dukovo-boot*(7), *dukovo.toml*(5),
*spark*(8), *netd*(8), *ip*(8), *radix-device*(2)
