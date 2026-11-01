re(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

re - Realtek's RTL8169, RTL8168 and RTL810x Ethernet chips

# DESCRIPTION

Realtek's gigabit chips, the PCI RTL8169 and RTL8110 and the PCI Express
RTL8168 and RTL8111 (B to H, EP, FP, and the RTL8411), and their PCI
Express chips at 10 and 100 Mb/s, the RTL8100E to RTL8107E and the
RTL8401E and RTL8402: the onboard Ethernet of most desktop and laptop
boards that is not Intel's, and FreeBSD's *re* and Linux's *r8169*.
D-Link's DGE-528T and DGE-530T, TP-Link's TG-3468, Corega's CG-LAPCIGT,
Linksys's EG1032 (revision 3), US Robotics' 997902 and the Killer E2600
are these chips too. The RTL8125 and RTL8126, at 2.5 and 5 Gb/s, are
*rge*(4)'s, and the RTL8139 family is not driven.

One PCI device ID stands for many chips, which the driver tells apart
by the revision their registers give, as FreeBSD's does, and from which
it names them; it refuses a revision it does not know, and the log says
so. It loads no firmware and no PHY parameters into the chip, as
FreeBSD's driver does not.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, below 4 GiB, one queue of 256
descriptors each way, with no offloads (ADR 0197). The kernel log names
the chip as it starts:

```
Net: net0 is a Realtek RTL8106E network card at 0000:02:00.0, MAC 00:e0:4c:68:01:02, 256 receive and 255 transmit slots, MSI vector 0x31
```

The PCI Express chips get an MSI when they have one; the PCI RTL8169s
are polled, as Linux leaves them on INTx, every millisecond while
frames move and every 10 ms when quiet. A card that stops with an
error, a PCI error among them, is left stopped, and the log says so.

No QEMU or other emulator has these chips: the driver is tested against
a model of them, and on the Dell Inspiron 15 5570's RTL810xE.

# SEE ALSO

*alc*(4), *bge*(4), *e1000*(4), *ip*(8), *netd*(8)
