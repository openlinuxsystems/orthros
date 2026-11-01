vmxnet3(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

vmxnet3 - VMware's paravirtual network card

# DESCRIPTION

VMXNET3, which VMware's hypervisors give a machine when asked for
their fastest card, and QEMU emulates as its *vmxnet3*, revision 1 of
its interface. The card is described to the hypervisor in memory both
share, and raises one MSI for frames received and sent and for events,
masking it as it does.

The kernel finds the card on the PCI bus as it starts, resets it and
gives it an interface, *net0*, *net1*... in the order the cards were
found, which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. Its frames are copied through slots of 2048 bytes in memory the
card alone may reach behind the IOMMU, one queue each way, with no
offloads (ADR 0197). The kernel log names the card as it starts:

```
Net: net4 is a VMware VMXNET3 network card at 0000:00:07.0, MAC 52:54:00:12:34:64, 256 receive and 255 transmit slots, MSI vector 0x35
```

A card the kernel cannot give an interrupt is polled: every millisecond
while frames move, every 10 ms when quiet. A card that stops with an
error is left stopped, and the log says so.

# SEE ALSO

*e1000*(4), *ip*(8), *netd*(8)
