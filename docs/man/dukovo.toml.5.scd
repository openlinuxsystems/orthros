dukovo.toml(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

dukovo.toml - Dukovo build configuration

# SYNOPSIS

_dukovo.toml_ at the top of the repository

# DESCRIPTION

*ob*(1) reads _dukovo.toml_ for the few choices a builder makes. The file
is TOML (https://toml.io). Every key is optional; a missing key, section or
file means the default shown below. Unknown keys and invalid values are
errors, so a typo cannot be silently ignored. Command-line options of *ob*
override the file.

# BUILD SECTION

*[build]* controls what is compiled.

*build.arch* = "x86_64"
	Target architecture. *x86_64* is the only value until AArch64 support
	arrives in Phase 34 of the roadmap.

*build.profile* = "release" | "debug"
	Cargo profile for the kernel. *release* (the default) is optimised;
	*debug* compiles faster and runs slower. *ob build*, *ob image*, *ob
	run* and *ob test* accept *--debug* and *--release* to override it.

# BOOT SECTION

*[boot]* controls how GRUB starts the kernel in images built by *ob image*
and *ob run*. Test images built by *ob test* ignore it: they use a zero
timeout and the flags each test needs.

*boot.timeout* = _seconds_
	How long the GRUB menu waits before booting Dukovo, from 0 to 600.
	The default is 3.

*boot.cmdline* = "_flags_"
	Kernel command-line flags, separated by spaces, such as
	"loglevel=debug". The flags the kernel understands are listed in
	*dukovo-boot*(7). Only letters, digits, spaces and the characters
	*. \_ = , : / + -* are allowed, because the value is written into
	GRUB's configuration. The default is empty. *ob image --cmdline* and
	*ob run --cmdline* override it.

# IMAGE SECTION

*[image]* adds optional files to the images *ob image* and *ob run* build.

*image.microcode* = "_path_"
	A file or directory of Intel microcode updates to load early in boot,
	such as "/lib/firmware/intel-ucode" from the *intel-microcode*
	package. Microcode is closed firmware, so Dukovo does not ship it;
	this key adds the updates of the build host to the image. *ob* checks
	each update and fails if _path_ holds none. The default is empty: no
	microcode. *ob image --microcode* and *ob run --microcode* override it.
	See *MICROCODE* in *dukovo-boot*(7).

# RUN SECTION

*[run]* describes the virtual machine *ob run* starts.

*run.firmware* = "uefi" | "bios"
	Firmware to boot through: *uefi* (OVMF, the default) or *bios*
	(SeaBIOS). *ob run --uefi* and *--bios* override it.

*run.media* = "usb" | "cdrom" | "disk"
	How the image is attached: as a USB stick on an xHCI controller (the
	default, like a real install medium), a SATA CD-ROM, or a SATA hard
	disk. *ob run --media* overrides it.

*run.memory* = "_size_"
	Guest memory in QEMU's notation, a number followed by *M* or *G*. The
	default is "1G". With more than 4 GiB the crash log moves above 4 GiB,
	as on most real machines (see *radix-debug*(7)).

*run.cpus* = _count_
	Number of virtual CPUs, from 1 to 64. The default is 4. The kernel
	starts all of them.

# FEATURES SECTION

*[features]* turns on or off what the image's system does that it need
not. Each one only adds flags to the kernel command line of the images
*ob image* and *ob run* build; the boot tests set their own.

*features.userspace-e1000* = true | false
	Whether Intel's e1000 and e1000e network cards are driven by a
	program, *e1000d*(8), rather than by the kernel (ADR 0198). When true,
	the default, the image boots with *e1000=program*, unless its command
	line sets *e1000=* itself; the kernel then leaves each such card that
	an IOMMU confines to its own memory for e1000d, and drives the others
	itself. See *e1000*(4) and *dukovo-boot*(7).

# EXAMPLE

The file shipped in the repository:

```
[build]
arch = "x86_64"
profile = "release"

[boot]
timeout = 3
cmdline = ""

[image]
microcode = ""

[run]
firmware = "uefi"
media = "usb"
memory = "1G"
cpus = 4

[features]
userspace-e1000 = true
```

# SEE ALSO

*ob*(1), *dukovo-boot*(7), *radix-debug*(7), *e1000d*(8)
