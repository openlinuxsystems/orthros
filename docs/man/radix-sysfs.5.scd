radix-sysfs(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-sysfs - the kernel's devices and settings, one value per file in
/sys

# DESCRIPTION

A *sys* filesystem, which Spark mounts at _/sys_ (*spark*(8)), holds the
kernel's devices and settings, one value per file, each ending in a
newline. It is read-only. Every file is written when it is opened, and
its size in *stat* is 0 until then, so read a file to its end rather than
by its size.

Wherever Linux has the same thing, the path and the format are Linux's,
so that ported tools find it where they look: *lspci* reads
_/sys/bus/pci/devices_, *lsblk* reads _/sys/block_. The files under
_/sys/kernel_ are Radix's own.

This page describes _/sys_ as of Phase 7 of the roadmap; later phases add
devices and settings, and settings that can be changed.

# FILES

_/sys/kernel/version_
	The kernel's name and version, and the phase of the roadmap it is
	from: *Radix 0.0.1 (Dukovo Phase 7)*.

_/sys/kernel/cmdline_
	The kernel command line the boot loader gave (*dukovo-boot*(7)).

_/sys/kernel/cpus_
	The CPUs online.

_/sys/block/_*name*_/size_
	The size of a disk or partition in 512-byte sectors, whatever its
	block size, as on Linux.

_/sys/block/_*name*_/ro_
	*1* if the device is read-only, *0* if not.

	Disks and partitions are named as Linux names them: _vda_, _vdb_ for
	virtio disks, _vda1_ for the first partition of _vda_. Linux puts a
	partition's directory inside its disk's (_/sys/block/vda/vda1_) and
	lists both in _/sys/class/block_; Radix lists both here.

_/sys/bus/pci/devices/_*address*_/vendor_, _device_
	The PCI function's vendor and device IDs, in hexadecimal with four
	digits: *0x1af4* and *0x1001* for a virtio block device as QEMU
	offers it (*0x1042* when it has no legacy interface).

_/sys/bus/pci/devices/_*address*_/class_
	Its class code, the base class, subclass and programming interface,
	in hexadecimal with six digits: *0x010000* for a SCSI-like storage
	controller.

	A function's address is its PCI segment, bus, device and function,
	as Linux writes it: _0000:00:03.0_. Every function the kernel's scan
	found is listed, whether or not a driver uses it.

# EXAMPLES

In the *storage* boot test (*ob*(1)), the 32 MiB disk _vdb_ has
_/sys/block/vdb/size_ of *65536*, and the boot test's program finds a
virtio function for each of its four disks in _/sys/bus/pci/devices_.

# SEE ALSO

*radix-proc*(5), *radix-files*(7), *dukovo-boot*(7), *ob*(1), *spark*(8)
