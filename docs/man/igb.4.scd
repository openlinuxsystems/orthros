igb(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

igb - Intel's 82576, 82580, I350, I210 and I211 gigabit Ethernet cards

# DESCRIPTION

Intel's 82576 (QEMU's *igb*), 82580, I350, I210 and I211 controllers,
on servers' cards and many boards, copper ports only, with advanced
descriptors and an MSI for everything. The PHY the card's firmware
shares is taken through its semaphore, as Intel's datasheets say.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, one queue each way, with no
offloads (ADR 0197). The kernel log names the card as it starts:

```
Net: net0 is an Intel 82576 network card at 0000:00:04.0, MAC 52:54:00:12:34:59, 255 receive and 255 transmit slots, MSI vector 0x32
```

A card the kernel cannot give an interrupt is polled: every millisecond
while frames move, every 10 ms when quiet. A card that stops with an
error is left stopped, and the log says so.

# SEE ALSO

*e1000*(4), *igc*(4), *ixgbe*(4), *ip*(8), *netd*(8)
