bge(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

bge - Broadcom's NetXtreme BCM5717, BCM5761, BCM5762, BCM57765 and BCM57780 Ethernet chips

# DESCRIPTION

Broadcom's gigabit PCI Express Ethernet controllers: the BCM5717,
BCM5718, BCM5719 and BCM5720 of servers' boards and cards, among them
many of Dell's PowerEdge and HPE's ProLiant servers; the BCM5761 and
BCM5762 of desktop boards; the BCM57765 family (the BCM57761, BCM57762,
BCM57765, BCM57766, BCM57781, BCM57782, BCM57785 and BCM57786) of
laptops and of Apple's Macs; and the BCM57780 of laptops. FreeBSD's
*bge* drives them all, with older chips this driver leaves out, and
Linux's *tg3* does too. The NetXtreme II chips (the BCM5706 to BCM5716)
and the NetXtreme-E chips are not these. Only copper ports are driven:
a port whose chip says it is a SerDes or fibre is left alone.

The driver resets the chip, waits for its bootcode to say it is done,
reads the MAC address the bootcode left in the chip's memory, and sets
up its state machines as FreeBSD's does, with the fixes FreeBSD applies
for each chip. It loads no firmware: the chips' bootcode is in their
NVRAM. On the chips with an APE, a processor in the chip that can share
the port with a server's management controller, the driver takes the
APE's locks before it resets the chip or reaches the PHY, keeps the
APE's frames moving, and tells its firmware when the driver starts and
stops. The PHY negotiates every speed it has; the driver looks at it
once a second while there is no link, and when the chip says the link
changed, and sets the MAC up for the link it found. It asks the PHY to
negotiate again when no link has come up for 17 seconds.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, below 4 GiB, 256 slots each way,
with no offloads (ADR 0197). The card takes every multicast frame,
which IPv6 and mDNS need. The kernel log names the chip as it starts:

```
Net: net0 is a Broadcom BCM5720 network card at 0000:04:00.0, MAC 00:10:18:01:02:03, 256 receive and 255 transmit slots, MSI vector 0x31
```

The card gets an MSI when it has one, and is polled otherwise, every
millisecond while frames move and every 10 ms when quiet. A card that
reads as gone, says something about its rings that makes no sense, or
holds frames for five seconds without sending any is left stopped, and
the log says so.

No QEMU or other emulator has these chips: the driver is tested against
a model of them, and the first boot on a machine that has one is its
real test.

# SEE ALSO

*alc*(4), *e1000*(4), *re*(4), *ip*(8), *netd*(8)
