lsusb(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

lsusb - list the USB devices

# SYNOPSIS

*lsusb* [*-t*] [*-s* [[_bus_]:][_devnum_]] [*-d* [_vendor_]:[_product_]]

# DESCRIPTION

lsusb lists the USB devices the kernel set up, from
_/kernel/bus/usb/devices_ (*radix-sysfs*(5)), in the order of their
buses and numbers, as usbutils' lsusb lists them on Linux: each
device's bus, its number on the bus, its vendor and product IDs, and
their names.

```
$ lsusb
Bus 001 Device 002: ID 0bda:5411 Realtek Semiconductor Corp. RTS5411 Hub
Bus 001 Device 003: ID 046d:c52b Logitech, Inc. Unifying Receiver
```

The names come from the Linux USB ID Repository's database
(http://www.linux-usb.org/usb-ids.html),
_/system/resources/hwdata/usb.ids_, which may be shared under the GPL,
version 2 or later, or the 3-clause BSD license. A vendor or product the
database does not name takes the device's own string for it, else
_[unknown]_.

The root hubs, one for each USB controller, are in the tree (*-t*) and
not in the list: Linux gives them the IDs of the Linux Foundation, which
Dukovo does not borrow.

# OPTIONS

*-t*, *--tree*
	The tree of each bus: its root hub, with its controller's driver and
	ports, then each interface of each device on its ports, with the
	interface's class and the driver that took it (*[none]* when no
	driver did), and its speed in Mb/s; a hub's devices are a level
	deeper. *-s* and *-d* change nothing here, as in usbutils.

```
/:  Bus 001.Port 001: Dev 001, Class=root_hub, Driver=xhci_hcd/4p, 5000M
    |__ Port 001: Dev 002, If 0, Class=Hub, Driver=hub/4p, 480M
        |__ Port 002: Dev 003, If 0, Class=Human Interface Device, Driver=usbhid, 12M
```

*-s* [[_bus_]:][_devnum_]
	Only the devices on that bus, or of that number, both in decimal.

*-d* [_vendor_]:[_product_]
	Only the devices with those IDs, in hexadecimal.

Not yet: *-v*, the devices' descriptors in full, which the kernel does
not show.

# EXIT STATUS

0 when a device was listed; 1 when none was, when the kernel's list
cannot be read, or for an option it does not know.

# SEE ALSO

*lspci*(8), *radix-sysfs*(5), *systools*(1)
