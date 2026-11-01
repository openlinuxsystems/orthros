ixgbe(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

ixgbe - Intel's 82599, X540 and X550 10 gigabit Ethernet cards

# DESCRIPTION

Intel's 82599 (SFP+ and KX4), X540 and X550 (10GBASE-T) controllers,
with advanced descriptors and an MSI for everything. The link is the
NVM's: the X540's and X550's PHYs negotiate it by themselves, and the
82599 is asked to negotiate it again as its NVM set it up, which brings
up direct-attach cables and most SFP+ modules; the sequences some
modules need are not run yet. On the X540 and X550 the card is reset
with the semaphore of the PHY the firmware shares held, as Intel's
datasheets say.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, one queue each way, with no
offloads (ADR 0197). The kernel log names the card as it starts:

```
Net: net0 is an Intel X550-T2 network card at 0000:04:00.0, MAC a0:36:9f:00:00:01, 255 receive and 255 transmit slots, MSI vector 0x31
```

A card the kernel cannot give an interrupt is polled: every millisecond
while frames move, every 10 ms when quiet. A card that stops with an
error is left stopped, and the log says so.

# SEE ALSO

*igb*(4), *e1000*(4), *ip*(8), *netd*(8)
