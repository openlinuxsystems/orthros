rge(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

rge - Realtek's RTL8125 and RTL8126 2.5 and 5 gigabit Ethernet chips

# DESCRIPTION

Realtek's PCI Express Ethernet controllers at 2.5 Gb/s, the RTL8125A,
RTL8125B and RTL8125D, and at 5 Gb/s, the RTL8126A, on desktop boards,
cards and laptops since 2019, among them Killer's E3000 and E5000 and
the Framework Desktop's port. OpenBSD's and FreeBSD's *rge* drive them,
and Linux's *r8169*. Realtek's gigabit chips are *re*(4)'s.

Two PCI device IDs, and Killer's two, stand for the chips, which the
driver tells apart by the revision their registers give, as OpenBSD's
does. It refuses a revision it does not know.

The driver sets the chip and its PHY up as OpenBSD's does, translated
from OpenBSD's ISC-licensed code, and reads the MAC address the chip's
boot code left in its registers. Its tests check that the register
reads and writes it makes on each of the six chip revisions are, byte
for byte, those OpenBSD's own code makes on the same model of the chips,
which *tools/rge-openbsd-check* runs.

What it leaves out:

- *Realtek's firmware*. OpenBSD's and Linux's drivers load patches for
  the processors in the chip's MAC and PHY, which Realtek ships as
  firmware: its source is not published, and Realtek's notice allows
  only passing it on. The base image carries no closed firmware, so the
  chips run on the code in their own ROMs, as Linux runs them without
  its *rtl_nic* files. Realtek does not say what its patches fix; a
  link that is slow to come up or drops on one of these chips should be
  reported with the chip the kernel log names.
- *The RTL8127*, Realtek's 10 Gb/s chip, which is not driven yet.
- *Offloads*: no checksums, VLAN tag stripping, receive side scaling,
  interrupt moderation or wake on LAN (ADR 0197).

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, one queue of 256 descriptors
each way. The card takes every multicast frame, which IPv6 and mDNS
need, and negotiates every speed it has, with pause frames. The driver
turns the PCI Express link's power saving states off, as OpenBSD's
does. The kernel log names the chip as it starts:

```
Net: net0 is a Realtek RTL8125B network card at 0000:02:00.0, MAC 00:e0:4c:68:01:02, 256 receive and 255 transmit slots, MSI vector 0x31
```

The card gets an MSI. A card that reports a PCI error, or holds frames
for five seconds without sending any, is left stopped, and the log says
so.

No QEMU or other emulator has these chips: the driver is tested against
a model of them, and the first boot on a machine that has one is its
real test.

# SEE ALSO

*alc*(4), *bge*(4), *e1000*(4), *re*(4), *ip*(8), *netd*(8)
