ob(1) "OrthrOS" "OrthrOS Manual"

# NAME

ob - build, run and test OrthrOS

# SYNOPSIS

*./ob* [*--verbose*] _command_ [_options_]

*./ob* *--container* _command_ [_options_]

# DESCRIPTION

*ob* is the single entry point for building OrthrOS. It compiles the Radix
kernel, assembles bootable images, boots them in QEMU, runs the test suites
and builds the documentation. It needs no root access.

*./ob* at the top of the repository is a small shell script. It builds the
real tool from _tools/xtask_ with the Rust toolchain pinned in
_rust-toolchain.toml_ and runs it, so the first command takes a little
longer. Settings come from _orthros.toml_ (see *orthros.toml*(5)); command
options override them.

Build output goes to _out/_ and Cargo's intermediate files to _target/_.

# GLOBAL OPTIONS

*-v*, *--verbose*
	Print every external command (cargo, grub-mkrescue, qemu-system-x86_64)
	before running it.

*--container*
	Run the command inside the pinned build container rather than on the
	host. The wrapper uses Podman, or Docker if Podman is not installed, and
	builds the container image from _tools/container/Containerfile_ on first
	use. Build output still lands in the repository. This is how to build on
	macOS, Windows, or a Linux distribution *ob doctor* does not know.

*-h*, *--help*
	Show the list of commands. After a command name, show that command's
	help.

*-V*, *--version*
	Print the OrthrOS version.

# COMMANDS

*setup*
	Install the Rust toolchain pinned in _rust-toolchain.toml_ with
	*rustup*(1), including rustfmt, Clippy and the x86_64-unknown-none
	target, then run *doctor*. Run it once after cloning and again whenever
	the pinned toolchain changes.

*doctor*
	Check every host tool the build needs: rustup, the pinned toolchain,
	QEMU, OVMF, GRUB for BIOS and UEFI, xorriso, mtools, scdoc, mandoc and
	mdBook. For anything missing, print the exact install command for
	Debian, Ubuntu, Fedora or Arch Linux. Also report whether KVM is usable;
	without it QEMU still works, only slower. Exits with status 1 when a
	required tool is missing.

*build* [*--debug* | *--release*]
	Build the Radix kernel and install it as _out/sysroot/boot/radix_.

	*--debug*
		Build without optimisation.

	*--release*
		Build with optimisation. This is the default unless
		*build.profile* in _orthros.toml_ says otherwise.

*image* [*--iso*] [*--debug* | *--release*]
	Build the kernel, then the boot image
	_out/orthros-VERSION-x86_64.img_. The image is a hybrid: it starts
	under UEFI and under legacy BIOS, from a USB stick, a hard disk or
	optical media. Write it to a USB stick as shown in *EXAMPLES*.

	*--iso*
		Name the output _out/orthros-VERSION-x86_64.iso_ instead, for
		virtual machines and optical media. The contents are the same.

*run* [*--uefi* | *--bios*] [*--media* _kind_] [*--headless*] [*--debug* | *--release*] [*--* _qemu-args_...]
	Build the image and boot it in QEMU on a q35 machine. The guest's
	serial port is this terminal; press *Ctrl-A* then *X* to quit QEMU, or
	*Ctrl-A* then *C* for the QEMU monitor. A QEMU window shows the screen
	unless there is no display. KVM is used when _/dev/kvm_ is usable. The
	image file itself is never modified.

	*--uefi*
		Boot through OVMF. This is the default unless *run.firmware* says
		otherwise.

	*--bios*
		Boot through SeaBIOS, as a legacy BIOS machine.

	*--media* _kind_
		Attach the image as *usb* (a USB stick on an xHCI controller, the
		default), *cdrom* (a SATA CD-ROM) or *disk* (a SATA hard disk).

	*--headless*
		Do not open a QEMU window. This is automatic when neither
		*DISPLAY* nor *WAYLAND_DISPLAY* is set.

	*--* _qemu-args_
		Pass everything after *--* to *qemu-system-x86_64*(1) unchanged,
		for example *-- -s -S* to wait for a debugger.

*test* [*unit* | *boot*] [*--timeout* _seconds_] [*--debug* | *--release*]
	Run the host unit tests, then build a test image and boot it four times
	in QEMU: under UEFI and BIOS, from USB and from CD-ROM. Each boot passes
	when the kernel reports the right firmware on the serial port and exits
	QEMU through the *qemu-exit* flag (see *orthros-boot*(7)). Serial logs
	go to _out/test/_. Give *unit* or *boot* to run only that part.

	*--timeout* _seconds_
		Fail a boot that has not finished after this long. The default is
		300.

*docs* [*--check*]
	Build the man pages from _docs/man/\*.scd_ into _out/docs/man/_ and the
	handbook from _docs/handbook/_ into _out/docs/handbook/_. Every man page
	is checked with *mandoc -T lint*, and any warning fails the command.

	*--check*
		Also fail when a man page title does not match its file name, when
		an *ob* command is missing from this page, or when an
		_orthros.toml_ key is missing from *orthros.toml*(5).

*lint*
	Check formatting with rustfmt and run Clippy on the build tool, the
	kernel and the kernel libraries, treating every warning as an error.
	CI runs the same command.

*fmt*
	Format all Rust code with rustfmt.

*clean* [*--all*]
	Remove _out/_ and the kernel's build directory.

	*--all*
		Also remove _target/_ and _.cache/_, which hold the build tool
		itself and the container's Cargo cache.

*help* [_command_]
	Show the list of commands, or the help for one command.

# ENVIRONMENT

*OB_CONTAINER_ENGINE*
	Program to use for *--container*, such as *podman* or *docker*.

*OB_OVMF_CODE*, *OB_OVMF_VARS*
	Paths to OVMF's code and variable-store images, when they are installed
	somewhere *ob* does not look.

*OB_TARGET_DIR*
	Directory for Cargo output instead of _target/_.

*SOURCE_DATE_EPOCH*
	Timestamp for files in the image. By default *ob* uses the time of the
	current Git commit.

# FILES

_orthros.toml_
	Build configuration; see *orthros.toml*(5).

_rust-toolchain.toml_
	The pinned Rust toolchain.

_image/grub/grub.cfg.in_
	Template for the GRUB menu in every image.

_out/orthros-VERSION-x86_64.img_
	The boot image built by *ob image*.

_out/test/_
	Test image and serial logs from *ob test*.

# EXIT STATUS

*ob* exits with 0 on success and 1 on any failure. Errors are printed with
an *ob: error:* prefix, usually followed by an *ob: hint:* line saying what
to do next.

# EXAMPLES

Prepare a new machine, then boot OrthrOS in QEMU:

```
./ob setup
./ob run
```

Boot as a legacy BIOS machine from a virtual hard disk, without a window:

```
./ob run --bios --media disk --headless
```

Build the image and write it to a USB stick. Check the device name with
*lsblk*(8) first: everything on that device is erased.

```
./ob image
lsblk -d -o NAME,SIZE,MODEL,TRAN
sudo dd if=out/orthros-0.0.1-x86_64.img of=/dev/sdX bs=4M conv=fsync status=progress
```

Graphical alternatives that write the same image are balenaEtcher, GNOME
Disks ("Restore Disk Image") and Rufus in DD mode.

Build on a host without the tools installed, using Podman or Docker:

```
./ob --container test
```

# SEE ALSO

*orthros.toml*(5), *orthros-boot*(7), *qemu-system-x86_64*(1),
*grub-mkrescue*(1), *rustup*(1), *dd*(1)

The OrthrOS handbook, chapter "Building OrthrOS with ob", in
_docs/handbook/_ or _out/docs/handbook/index.html_ after *ob docs*.
