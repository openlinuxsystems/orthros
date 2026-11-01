gve(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

gve, gvnic - Google's virtual network card, gVNIC

# DESCRIPTION

gVNIC, which Google Compute Engine gives its machines, in its GQI-QPL
queue format: the card reads and writes frames only in pages the
driver registered with it. The DQO format of Google's third-generation
machine types is not driven yet, and a card without GQI-QPL is left
alone. Compute Engine's networks default to an MTU of 1460, which the
interfaces do not take from DHCP yet: set the network's MTU to 1500.

The card has only MSI-X: every entry of its table is given the same
message, so each queue's notification block comes to the card's thread.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, one queue each way, with no
offloads (ADR 0197). The kernel log names the card as it starts:

```
Net: net0 is a Google gVNIC network card at 0000:00:04.0, MAC 42:01:0a:80:00:02, 1024 receive and 127 transmit slots, MSI-X vector 0x31
```

A card the kernel cannot give an interrupt is polled: every millisecond
while frames move, every 10 ms when quiet. A card that stops with an
error is left stopped, and the log says so.

# SEE ALSO

*ena*(4), *ip*(8), *netd*(8)
