igc(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

igc - Intel's I225 and I226 2.5 gigabit Ethernet cards

# DESCRIPTION

Intel's I225 and I226 controllers, at up to 2.5 Gb/s, on many boards
and small machines since 2019, and their I220-V and I221-V, driven as
*igb*(4) drives its cards, with the few registers that moved. A card
whose NVM is blank, with no address of its own, is left alone.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, one queue each way, with no
offloads (ADR 0197). The kernel log names the card as it starts:

```
Net: net0 is an Intel I226-V network card at 0000:03:00.0, MAC a8:a1:59:00:00:01, 255 receive and 255 transmit slots, MSI vector 0x31
```

A card the kernel cannot give an interrupt is polled: every millisecond
while frames move, every 10 ms when quiet. A card that stops with an
error is left stopped, and the log says so.

# SEE ALSO

*igb*(4), *e1000*(4), *ip*(8), *netd*(8)
