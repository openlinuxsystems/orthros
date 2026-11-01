ena(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

ena - Amazon's Elastic Network Adapter

# DESCRIPTION

ENA, the network card of every current EC2 instance, set up through
its admin queue, with its link and errors from its event queue. Its
descriptors are in host memory: the low-latency queues some instance
types prefer, with descriptors in the card's memory, are not used yet.
A card whose PCI revision says register reads do not work is read
through DMA instead, as Amazon's driver reads it.

The card has only MSI-X: every entry of its table is given the same
message, so the queues' interrupt and the admin queue's come to the
card's thread.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, one queue each way, with no
offloads (ADR 0197). The kernel log names the card as it starts:

```
Net: net0 is an Amazon ENA network card at 0000:00:05.0, MAC 0a:1b:2c:3d:4e:5f, 255 receive and 255 transmit slots, MSI-X vector 0x31
```

A card the kernel cannot give an interrupt is polled: every millisecond
while frames move, every 10 ms when quiet. A card that stops with an
error is left stopped, and the log says so.

# SEE ALSO

*gve*(4), *ip*(8), *netd*(8)
