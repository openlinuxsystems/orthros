e1000(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

e1000, e1000e - Intel's 8254x and 8257x gigabit Ethernet cards

# DESCRIPTION

Intel's 8254x PCI and PCI-X controllers (82540, 82541, 82543, 82544,
82545, 82546 and 82547: Linux's *e1000*, and the card QEMU, VirtualBox
and VMware give a machine by default) and their PCI Express successors,
the 82571, 82572, 82573, 82574 and 82583 (Linux's *e1000e*, and QEMU's
*e1000e*), copper cards only, with their legacy 16-byte descriptors.

The 8254x are always polled, as Linux leaves their MSI unused; the
PCI Express cards get an MSI when they have one.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, one queue each way, with no
offloads (ADR 0197). The kernel log names the card as it starts:

```
Net: net0 is an Intel 82574L network card at 0000:00:03.0, MAC 52:54:00:12:34:57, 255 receive and 255 transmit slots, MSI vector 0x31
```

A card the kernel cannot give an interrupt is polled: every millisecond
while frames move, every 10 ms when quiet. A card that stops with an
error is left stopped, and the log says so.

# AS A PROGRAM

With the boot flag *e1000=program* (*dukovo-boot*(7)), which the images
*ob image* builds have (*features.userspace-e1000* in *dukovo.toml*(5)),
the kernel leaves each of these cards that the IOMMU confines to its own
memory, with interrupt remapping on, to *e1000d*(8), which runs the same
driver as a program (ADR 0198). The card keeps its interface, down until
e1000d attaches it; a card the IOMMU cannot confine, alone or with
another function that shares its requests' ID, stays the kernel's:

```
Net: net0 is an Intel 82540EM network card at 0000:00:03.0, left for a driver program (e1000=program)
```

# SEE ALSO

*igb*(4), *igc*(4), *ixgbe*(4), *ip*(8), *netd*(8), *e1000d*(8)
