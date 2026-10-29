radix-sysfs(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-sysfs - the kernel's view of the hardware and its settings, one
value per file in /kernel

# DESCRIPTION

A *sys* filesystem, which Spark mounts at _/kernel_ (*spark*(8)), with
_/sys_, Linux's name for it, a link to it (*radix-files*(7)), holds the
kernel's view of the hardware and its settings, one value per file, each
ending in a newline. It is read-only. Every file is written when it is
opened, and its size in *stat* is 0 until then, so read a file to its end
rather than by its size.

Wherever Linux has the same thing, the path beneath the folder and the
format are Linux's, so that ported tools find it where they look through
the link: *lspci* reads _/sys/bus/pci/devices_, *lsblk* reads
_/sys/block_. The files at the top, _version_, _cmdline_ and _cpus_, are
Radix's own.

This page describes _/kernel_ as of Phase 11 of the roadmap; later phases
add devices and settings, and settings that can be changed.

# FILES

_/kernel/version_
	The kernel's name and version, and the Dukovo release it is part
	of: *Radix 0.10.3 (Dukovo Server 0.10.3)*. The C library gives the
	second word as the release, and the whole line as the version, in
	uname(3)'s *struct utsname*.

_/kernel/cmdline_
	The kernel command line the boot loader gave (*dukovo-boot*(7)).

_/kernel/cpus_
	The CPUs online.

_/kernel/block/_*name*_/size_
	The size of a disk or partition in 512-byte sectors, whatever its
	block size, as on Linux.

_/kernel/block/_*name*_/ro_
	*1* if the device is read-only, *0* if not.

_/kernel/block/_*name*_/partition_, _start_
	For a partition: its number in its disk's table, the one its name
	ends in, and its first sector on the disk, in 512-byte sectors. A
	partition has these instead of a _queue_ directory, as on Linux,
	whose tools read the queue of the partition's disk.

_/kernel/block/_*name*_/parttype_
	For a partition: its type, as *lsblk*(8)'s PARTTYPE and *blkid*(8)
	give it: its MBR type byte in hexadecimal (*0x0c*, *0xef*), or its
	GPT type's GUID in lowercase
	(*c12a7328-f81f-11d2-ba4b-00a0c93ec93b* for an EFI system
	partition). Linux has it only through udev; Radix has it here.

_/kernel/block/_*name*_/device/model_, _serial_, _firmware_rev_
	For a SATA, NVMe or USB disk: its model, serial number and firmware
	revision, as it gives them, without the padding. A USB disk's model
	is its SCSI vendor and product, its firmware its SCSI revision, and
	its serial number its USB device's, as udev's *ID_SERIAL_SHORT* has
	it, up to 64 bytes. Linux has these for
	an NVMe disk; for a SATA disk it cuts the model to 16 characters and
	keeps the serial number elsewhere. A tool that must write to one
	particular disk should find it by its serial number, since disk names
	follow the order the controllers are found in.

_/kernel/block/_*name*_/queue/logical_block_size_, _physical_block_size_
	The sizes in bytes of the device's logical blocks, the least it
	reads or writes (*512*, or *4096* for a disk with 4 KiB sectors),
	and of the blocks it writes without reading first (*radix-block*(2)).
	Offsets and sizes elsewhere stay in 512-byte sectors, as on Linux.

_/kernel/block/_*name*_/queue/minimum_io_size_, _optimal_io_size_
	The smallest write it prefers, its physical block size, and the
	size it does best, *0* as it does not say.

_/kernel/block/_*name*_/queue/rotational_
	*1* for a hard disk, or a disk that does not say; *0* for a solid
	state disk.

_/kernel/block/_*name*_/queue/discard_granularity_, _discard_max_bytes_
	Its logical block size, and the most bytes one discard (TRIM)
	covers, if the device discards; *0* and *0* if not.

	Disks and partitions are named as Linux names them: _vda_, _vdb_ for
	virtio disks, _sda_, _sdb_ for SATA and USB disks, _nvme0n1_ for the first
	namespace of the first NVMe controller, _vda1_ for the first
	partition of _vda_ and _nvme0n1p1_ for the first of _nvme0n1_. Linux puts a
	partition's directory inside its disk's (_/sys/block/vda/vda1_) and
	lists both in _/sys/class/block_; Radix lists both here.

_/kernel/bus/pci/devices/_*address*_/vendor_, _device_
	The PCI function's vendor and device IDs, in hexadecimal with four
	digits: *0x1af4* and *0x1001* for a virtio block device as QEMU
	offers it (*0x1042* when it has no legacy interface).

_/kernel/bus/pci/devices/_*address*_/class_
	Its class code, the base class, subclass and programming interface,
	in hexadecimal with six digits: *0x010000* for a SCSI-like storage
	controller, *0x010601* for an AHCI one, *0x010802* for an NVMe one.

_/kernel/bus/pci/devices/_*address*_/revision_
	Its revision, in hexadecimal with two digits: *0x02*.

_/kernel/bus/pci/devices/_*address*_/subsystem_vendor_, _subsystem_device_
	The vendor and model of the board around it, in hexadecimal with
	four digits: *0x1af4* and *0x1100* on QEMU. A bridge's are in its
	Subsystem ID capability; *0x0000* for a function that has none.

	A function's address is its PCI segment, bus, device and function,
	as Linux writes it: _0000:00:03.0_. Every function the kernel's scan
	found is listed, whether or not a driver uses it. *lspci*(8) reads
	them.

_/kernel/firmware/memmap/_*n*_/start_, _end_, _type_
	The firmware's memory map, as the boot loader gave it to the kernel,
	a directory a range numbered from *0* in its order, as Linux's
	_/sys/firmware/memmap_: the range's first and last byte, in
	hexadecimal (*0x0* and *0x9fbff*), and its type, in Linux's names:
	*System RAM*, *Reserved*, *ACPI Tables*, *ACPI Non-volatile Storage*
	or *Unusable memory*. *lsmem*(1) reads it.

_/kernel/class/watchdog/watchdog0/identity_
	The hardware watchdog the kernel drives, if any (*spark*(8)'s
	*WATCHDOG*): _the Intel 6300ESB watchdog_ or _the Intel ICH9 TCO
	timer_. Without one, _/kernel/class_ is not there.

_/kernel/class/watchdog/watchdog0/state_
	*active* while it runs, else *inactive*.

_/kernel/class/watchdog/watchdog0/timeout_, _min_timeout_, _max_timeout_
	Its timeout in seconds, as last set (0 before it first runs), and the
	shortest and longest it takes.

_/kernel/class/watchdog/watchdog0/bootstatus_
	*32*, Linux's WDIOF_CARDRESET, if it restarted the machine at the end
	of the boot before this one; *0* if not.

# EXAMPLES

In the *storage* boot test (*ob*(1)), the 32 MiB disk _vdb_ has
_/kernel/block/vdb/size_ of *65536*, and the boot test's program finds a
virtio function for each of its four disks in _/kernel/bus/pci/devices_.

# SEE ALSO

*radix-proc*(5), *radix-block*(2), *radix-files*(7), *dukovo-boot*(7), *ob*(1),
*spark*(8), *watchdog*(8), *lspci*(8), *lsmem*(1)
