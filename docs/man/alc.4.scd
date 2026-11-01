alc(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

alc - Atheros' AR813x, AR815x, AR816x and Killer E2x00 Ethernet chips

# DESCRIPTION

Atheros' PCI Express Ethernet controllers, on many laptops and desktop
boards from 2009 to 2018: the AR8131 and AR8151 at 1 Gb/s and the
AR8132 and AR8152 at 100 Mb/s, and the AR8161 and AR8171 at 1 Gb/s
and the AR8162 and AR8172 at 100 Mb/s, with Qualcomm Atheros' Killer
E2200, E2400 and E2500, which are built on the AR816x. FreeBSD's *alc*
drives them all, Linux's *atl1c* the first family and *alx* the second.
The older L1 and L2 chips (the AR8121 and AR8113) and the Killer E2600,
which is a Realtek chip and *re*(4)'s, are not these.

The driver resets the chip's PHY with the vendor's values, as FreeBSD's
does, resets the chip and reads the MAC address it loaded from its
EEPROM. It loads no firmware: the chips have none. The chip does not
set its MAC to the link its PHY negotiated, so the driver looks at the
PHY once a second and does, and asks the PHY to negotiate again when no
link has come up for 17 seconds.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, below 4 GiB, one queue of 256
descriptors each way, with no offloads (ADR 0197). The card takes
every multicast frame, which IPv6 and mDNS need. The kernel log names
the chip as it starts:

```
Net: net0 is an Atheros AR8161 network card at 0000:03:00.0, MAC 00:13:74:01:02:03, 256 receive and 255 transmit slots, MSI vector 0x31
```

The card gets an MSI when it has one, and is polled otherwise, every
millisecond while frames move and every 10 ms when quiet. A card that
reports a DMA timeout or a stuck transmit queue is left stopped, and
the log says so.

No QEMU or other emulator has these chips: the driver is tested against
a model of them, and the first boot on a machine that has one is its
real test.

# SEE ALSO

*bge*(4), *e1000*(4), *re*(4), *rge*(4), *ip*(8), *netd*(8)
