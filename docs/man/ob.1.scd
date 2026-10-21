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

Build output goes to _out/_ and Cargo's intermediate files to _target/_
(the kernel's to _target/kernel/_).

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
	*rustup*(1), including rustfmt, Clippy, the Rust sources (the kernel
	compiles its own copy of the *core* library) and the
	aarch64-unknown-none target (*lint* checks the AArch64 stubs of the
	hardware abstraction layer against it), then run *doctor*. Run it once
	after cloning and again whenever the pinned toolchain changes.

*doctor*
	Check every host tool the build needs: rustup, the pinned toolchain
	with its sources and AArch64 target, QEMU, OVMF, GRUB for BIOS and
	UEFI, xorriso, mtools, scdoc, mandoc and mdBook. For anything missing,
	print the exact install command for Debian, Ubuntu, Fedora or Arch
	Linux. Also report whether KVM is usable (without it QEMU still works,
	only slower) and whether GDB is installed for *run --gdb*. Exits with
	status 1 when a required tool is missing.

*build* [*--debug* | *--release*]
	Build the Radix kernel for its own target,
	_kernel/targets/x86_64-radix-kernel.json_, write the kernel's symbol
	table into its *.ksyms* section (see *radix-debug*(7)), and install it
	as _out/sysroot/boot/radix_.

	*--debug*
		Build without optimisation.

	*--release*
		Build with optimisation. This is the default unless
		*build.profile* in _orthros.toml_ says otherwise.

*image* [*--iso*] [*--cmdline* _flags_] [*--microcode* _path_] [*--debug* | *--release*]
	Build the kernel, then the boot image
	_out/orthros-VERSION-x86_64.img_. The image is a hybrid: it starts
	under UEFI and under legacy BIOS, from a USB stick, a hard disk or
	optical media. Write it to a USB stick as shown in *EXAMPLES*.
	*orthros-boot*(7) lists what the image contains.

	Every image carries a license manifest. *ob* lists each component
	that goes into it: the crates linked into the kernel (from *cargo
	tree*), the standard library crates built from the Rust sources, GRUB,
	and the microcode if any. It checks each component's SPDX license
	against the allow-list of its area, and fails if a component declares
	no license or one outside that list. Kernel crates follow
	_kernel/deny.toml_, which allows permissive licenses only; GRUB, a
	separate program, may be GPL-3.0-or-later. It then writes an SPDX 2.3
	document, the text of every license it names, and each component's
	own notice files to _/system/share/licenses/_ in the image, and a copy
	of the document next to the image.

	*--iso*
		Name the output _out/orthros-VERSION-x86_64.iso_ instead, for
		virtual machines and optical media. The contents are the same.

	*--cmdline* _flags_
		Kernel command-line flags instead of *boot.cmdline*, such as
		*"kdb loglevel=debug"*. *orthros-boot*(7) lists the flags.

	*--microcode* _path_
		Add the Intel microcode updates in _path_, a file or a directory
		such as _/lib/firmware/intel-ucode_, instead of those named by
		*image.microcode*. Every update is checked, and the command fails
		if _path_ holds none. See *MICROCODE* in *orthros-boot*(7).

*run* [*--uefi* | *--bios*] [*--media* _kind_] [*--headless*] [*--gdb*] [*--cmdline* _flags_] [*--microcode* _path_] [*--debug* | *--release*] [*--* _qemu-args_...]
	Build the image and boot it in QEMU on a q35 machine with
	*run.memory* of memory and *run.cpus* CPUs. The guest's serial port is
	this terminal; press *Ctrl-A* then *X* to quit QEMU, or *Ctrl-A* then
	*C* for the QEMU monitor. A QEMU window shows the screen unless there
	is no display. KVM is used when _/dev/kvm_ is usable. When the guest
	restarts, as after a panic, it boots the image again and shows the
	crash log of the previous boot. The image file itself is never
	modified.

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

	*--gdb*
		Start QEMU paused, with its GDB stub on localhost port 1234, and
		write the GDB script _out/qemu/radix.gdb_, which loads the
		kernel's symbols, connects and stops at the kernel's first Rust
		function. Run *gdb -x out/qemu/radix.gdb* in another terminal.
		The kernel is booted with *nokaslr* added to its flags, so it
		runs at the addresses GDB's symbols name. See *radix-debug*(7).

	*--cmdline* _flags_, *--microcode* _path_
		As for *image*.

	*--* _qemu-args_
		Pass everything after *--* to *qemu-system-x86_64*(1) unchanged,
		for example *-- -d int* to log interrupts.

*test* [*unit* | *boot* | *kani* | *tla*] [*--case* _name_]... [*--timeout* _seconds_] [*--debug* | *--release*]
	Run the unit tests of the build tool and the kernel libraries on this
	machine, then the boot tests: build a test image and boot it in QEMU
	once per case below. Each case follows the serial console, types
	*kdb* commands where it needs to, and reads the framebuffer console's
	text from QEMU screenshots by matching the kernel's own font. A case
	passes when every expected line appears in order and QEMU exits as
	expected, usually through the *qemu-exit* flag (see
	*orthros-boot*(7)). Serial logs and screenshots go to _out/test/_.
	Give *unit* or *boot* to run only that part. Without KVM the boot
	tests take a few minutes.

	*kani* checks the Kani proof harnesses of the kernel's memory manager
	and capability core instead (see *Verification* in
	_docs/design/memory.md_ and _docs/design/capabilities.md_). Each takes
	from seconds to several minutes, so plain *ob test* leaves them out,
	and CI runs them in a job of their own. It needs the Kani release
	that *ob doctor* names, installed with *cargo install --locked
	kani-verifier --version* _version_ and then *cargo kani setup*.

	*tla* checks the TLA+ models with the TLC model checker instead: the
	capability store's derivation tree and revocation, in
	_kernel/cap/tla/_, once for the safety properties and once, on a
	smaller model, for liveness. That takes about two minutes on 4 CPUs,
	so plain *ob test* leaves it out too, and CI runs it in a job of its
	own. TLC needs Java 11 or later (the *default-jre-headless* package on
	Debian, Ubuntu and Linux Mint). The first run downloads the pinned
	release of the TLA+ tools from GitHub into _.cache/tla/_ and checks
	its SHA-256; TLC's working files go to _out/tla/_.

	*uefi-usb*, *bios-usb*, *uefi-cdrom*, *bios-cdrom*
		Boot with 4 CPUs under each firmware and medium, and check the
		memory map, the KASLR layout, that W^X holds, the frame
		allocator's zones, the CPU list, timer interrupts on every CPU
		and an IPI to each. *uefi-cdrom* boots with *nox2apic* and checks
		for xAPIC mode; with KVM, which always offers x2APIC, the other
		three check for x2APIC mode. *bios-cdrom* boots with *nokaslr*
		and *nopcid*.

	*screen*
		Boot under UEFI and read the boot log back from the framebuffer.

	*page-fault*
		Boot with *selftest=page-fault* and check the panic report on the
		serial port and on the screen, with its symbolized backtrace.

	*crash-log-uefi*, *crash-log-bios*
		Boot with *selftest=crash-reboot*, let the panic restart the
		machine, and check that the next boot shows the panic from the
		crash log. The UEFI case has 5 GiB of memory, so the log lies
		above 4 GiB; the BIOS case has 1 GiB.

	*kdb*
		Boot with *kdb* and *selftest=breakpoint*, and run every *kdb*
		command at the breakpoint and at the end of boot.

	*mm-stress*
		Boot 4 CPUs on 2 NUMA nodes (QEMU's *-numa* options), check that
		the kernel reads both nodes from the SRAT and SLIT, and run
		*selftest=mm-stress*: tens of millions of heap allocations and
		frees, and areas mapped and unmapped, on every CPU at once, with
		nothing leaked or damaged at the end.

	*tlb*
		Boot 4 CPUs with *selftest=tlb*, which checks that every CPU
		sees a changed kernel mapping after a TLB shootdown and that
		address spaces switched on several CPUs keep their own mappings.

	*caps*
		Boot 4 CPUs with *selftest=caps*, the capability store's test
		suite: no capability can be forged or have its rights widened,
		revokes reach everything derived even while 3 other CPUs keep
		deriving, and nothing leaks.

	*stack-overflow*
		Boot with *selftest=stack-overflow* and check that the panic
		report names the stack guard page.

	*smep*, *smap*
		Boot with *selftest=smep* or *selftest=smap* and check that the
		page fault is reported as SMEP or SMAP stopping the kernel from
		running, or reading, a user page.

	*microcode*
		Boot an image with a generated microcode file on an emulated
		Intel CPU, and check that the kernel finds the matching update.

	*--case* _name_
		Run only this boot test; repeat the option for more.

	*--timeout* _seconds_
		Fail a boot test that has not finished after this long. The
		default is 300.

*docs* [*--check*]
	Build the man pages from _docs/man/\*.scd_ into _out/docs/man/_ and the
	handbook from _docs/handbook/_ into _out/docs/handbook/_. Every man page
	is checked with *mandoc -T lint*, and any warning fails the command.

	*--check*
		Also fail when a man page title does not match its file name, or
		when one of these is not documented in bold in its page: an *ob*
		command (this page), an _orthros.toml_ key (*orthros.toml*(5)), a
		kernel flag (*orthros-boot*(7)), a *kdb* command or a self-test
		(*radix-debug*(7)).

*lint*
	Check formatting with rustfmt and run Clippy, treating every warning as
	an error, on the build tool, on the kernel for x86-64, on the
	architecture-independent kernel against the AArch64 stubs of the
	hardware abstraction layer, and on the kernel libraries with their
	tests. CI runs the same command.

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

_kernel/targets/x86_64-radix-kernel.json_
	The kernel's Rust target specification.

_out/sysroot/boot/radix_
	The kernel built by *ob build*.

_kernel/deny.toml_
	The license allow-list of the kernel, used by *ob image* and by
	cargo-deny in CI.

_image/licenses/_
	License texts, copied verbatim from the SPDX license list, that
	*ob image* puts into the image beside _LICENSE-APACHE_ and
	_LICENSE-MIT_.

_out/orthros-VERSION-x86_64.img_
	The boot image built by *ob image*.

_out/orthros-VERSION-x86_64.spdx_
	The image's license manifest (SPDX 2.3 tag-value).

_out/qemu/radix.gdb_
	The GDB script written by *ob run --gdb*.

_out/test/_
	Test images, serial logs and screenshots from *ob test*.

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

Boot with the kernel debugger and debug messages:

```
./ob run --cmdline "kdb loglevel=debug"
```

Debug the kernel with GDB, from two terminals:

```
./ob run --gdb --debug
gdb -x out/qemu/radix.gdb
```

Build an image that loads Intel microcode, here from Debian's
*intel-microcode* package:

```
./ob image --microcode /lib/firmware/intel-ucode
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

*orthros.toml*(5), *orthros-boot*(7), *radix-debug*(7),
*qemu-system-x86_64*(1), *grub-mkrescue*(1), *rustup*(1), *gdb*(1), *dd*(1)

The OrthrOS handbook, chapter "Building OrthrOS with ob", in
_docs/handbook/_ or _out/docs/handbook/index.html_ after *ob docs*.
