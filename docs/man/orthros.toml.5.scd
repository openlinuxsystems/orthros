orthros.toml(5) "OrthrOS" "OrthrOS Manual"

# NAME

orthros.toml - OrthrOS build configuration

# SYNOPSIS

_orthros.toml_ at the top of the repository

# DESCRIPTION

*ob*(1) reads _orthros.toml_ for the few choices a builder makes. The file
is TOML (https://toml.io). Every key is optional; a missing key, section or
file means the default shown below. Unknown keys and invalid values are
errors, so a typo cannot be silently ignored. Command-line options of *ob*
override the file.

# BUILD SECTION

*[build]* controls what is compiled.

*build.arch* = "x86_64"
	Target architecture. *x86_64* is the only value until AArch64 support
	arrives in Phase 17 of the roadmap.

*build.profile* = "release" | "debug"
	Cargo profile for the kernel. *release* (the default) is optimised;
	*debug* compiles faster and runs slower. *ob build*, *ob image*, *ob
	run* and *ob test* accept *--debug* and *--release* to override it.

# BOOT SECTION

*[boot]* controls how GRUB starts the kernel in images built by *ob image*
and *ob run*. Test images built by *ob test* always use a zero timeout and
the *qemu-exit* flag.

*boot.timeout* = _seconds_
	How long the GRUB menu waits before booting OrthrOS, from 0 to 600.
	The default is 3.

*boot.cmdline* = "_flags_"
	Extra kernel command-line flags, separated by spaces. The flags the
	kernel understands are listed in *orthros-boot*(7). Only letters,
	digits, spaces and the characters *. \_ = , : / + -* are allowed,
	because the value is written into GRUB's configuration. The default is
	empty.

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
	default is "1G".

*run.cpus* = _count_
	Number of virtual CPUs, from 1 to 64. The default is 2. The Phase 0
	kernel uses only the first.

# EXAMPLE

The file shipped in the repository:

```
[build]
arch = "x86_64"
profile = "release"

[boot]
timeout = 3
cmdline = ""

[run]
firmware = "uefi"
media = "usb"
memory = "1G"
cpus = 2
```

# SEE ALSO

*ob*(1), *orthros-boot*(7)
