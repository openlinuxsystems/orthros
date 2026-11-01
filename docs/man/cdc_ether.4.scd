cdc_ether(4) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

cdc_ether, cdc_ncm - Ethernet adapters on USB

# DESCRIPTION

USB Ethernet adapters of the USB communications class's Ethernet Control
Model (CDC-ECM, Linux's *cdc_ether*) and Network Control Model (CDC-NCM,
Linux's *cdc_ncm*), on an xHCI controller: most USB-C docks and
adapters, Realtek's RTL8153 and RTL8156 in their class configuration,
phones and boards that share their network over USB, and QEMU's
*usb-net*. An adapter whose first configuration is its maker's own, as
the RTL8153's is, or Microsoft's RNDIS, as QEMU's is, is set up in its
CDC configuration instead, as Linux sets it up without the maker's
driver; an adapter with only its maker's protocol, or only RNDIS, is not
driven.

The kernel's USB thread sets the adapter up as it is plugged in: it
reads its MAC address from its string, asks an NCM adapter for the
rules of its transfer blocks and has it send blocks of at most a page,
picks its data interface's setting with the bulk endpoints, and has it
pass on the frames to its own address, broadcasts and multicast frames.
The adapter gets an interface, *net0*, *net1*... after the PCI cards,
which *netd*(8) sets up by DHCP or as its settings say, and *ip*(8)
shows. The log names it:

```
Net: net0 is a USB CDC-ECM network adapter on 1-6, MAC 40:54:00:12:34:80
```

Its frames go between the thread and the interface's stack through the
same shared slots as a driver program's (ADR 0198, ADR 0199), one
transfer at a time each way: a frame (ECM) or a block of frames (NCM)
each. The link is taken to be up, at 100 Mb/s, until the adapter says
otherwise on its interrupt endpoint.

An adapter unplugged takes its interface down, which keeps its
addresses; plugged in again, in any port, the adapter gets the same
interface back, by its MAC address, and netd asks for its address again:

```
Net: net0 is down: its USB adapter is gone
Net: net0 is back: the USB CDC-ECM network adapter on 1-6, MAC 40:54:00:12:34:80
```

QEMU's *usb-net* is tested in its CDC-ECM configuration; the NCM blocks
are tested on the host.

# SEE ALSO

*re*(4), *ip*(8), *lsusb*(8), *netd*(8)
