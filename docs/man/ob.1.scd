ob(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

ob - build, run and test Dukovo

# SYNOPSIS

*./ob* [*--verbose*] _command_ [_options_]

*./ob* *--container* _command_ [_options_]

# DESCRIPTION

*ob* is the single entry point for building Dukovo. It compiles the Radix
kernel, assembles bootable images, boots them in QEMU, runs the test suites
and builds the documentation. It needs no root access.

*./ob* at the top of the repository is a small shell script. It builds the
real tool from _tools/xtask_ with the Rust toolchain pinned in
_rust-toolchain.toml_ and runs it, so the first command takes a little
longer. Settings come from _dukovo.toml_ (see *dukovo.toml*(5)); command
options override them.

Build output goes to _out/_ and Cargo's intermediate files to _target/_
(the kernel's to _target/kernel/_, the user programs' to
_target/userspace/_).

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
	Print the Dukovo version.

# COMMANDS

*setup*
	Install the Rust toolchain pinned in _rust-toolchain.toml_ with
	*rustup*(1), including rustfmt, Clippy, the Rust sources (the kernel
	compiles its own copy of the *core* library) and the
	aarch64-unknown-none target (*lint* checks the AArch64 stubs of the
	hardware abstraction layer against it), then run *doctor* and
	*crates*, so that editors' Rust tools find the crates the kernel
	patches. Run it once after cloning and again whenever the pinned
	toolchain changes.

*doctor*
	Check every host tool the build needs: rustup, the pinned toolchain
	with its sources and AArch64 target, QEMU, OVMF, GRUB for BIOS and
	UEFI, xorriso, mtools, dosfstools, exfatprogs, The Sleuth Kit,
	btrfs-progs, e2fsprogs, the tools that build
	the ports (Clang 20 or later with compiler-rt's builtins, LLD, LLVM's
	tools, Meson 1.3 or later, Ninja, *localedef*, git, curl and patch,
	the last two of which the kernel's patched crates need too), scdoc,
	mandoc and mdBook. For anything missing, print the exact install
	command for Debian, Ubuntu, Fedora or Arch Linux. Also report whether KVM is usable (without it QEMU still works,
	only slower), whether GDB is installed for *run --gdb*, and whether
	dash, bash, *unshare*, *prlimit* and *setpriv* are there for *test
	corpus*. Exits with status 1 when a required tool is missing.

*build* [*--debug* | *--release*]
	Build the Radix kernel for its own target,
	_kernel/targets/x86_64-radix-kernel.json_, write the kernel's symbol
	table into its *.ksyms* section (see *radix-debug*(7)), and install it
	as _out/sysroot/boot/radix_. Then write the C headers of the system
	call interface, which *ridlc* generates from _abi/\*.ridl_, into
	_out/sysroot/system/include/radix/_ (see *radix-ipc*(7)), build the
	ports that changed (see *ports*), the C test programs of
	_userspace/posix/_ and the programs on Rust's standard library in
	_userspace/programs/_, and build the native user programs in
	_userspace/_ for _userspace/targets/x86_64-radix-user.json_. Their
	test image goes to _out/sysroot/boot/usertests_, which every image
	loads as a boot module, and Spark, the programs it starts and the
	files the ports put there go into the boot archive,
	_out/sysroot/boot/bootfs_ (see *spark*(8)). Dukovo's own manual
	pages from _docs/man/_ go there too, but not this one or
	*dukovo.toml*(5), and *makewhatis*(8) indexes every page. The build
	fails if a program in _/system/commands_ has no page; from each program's
	page it writes the program's completion file for *nukesh*(1) into
	_/system/resources/nukesh/completions/_.

	*--debug*
		Build without optimisation.

	*--release*
		Build with optimisation. This is the default unless
		*build.profile* in _dukovo.toml_ says otherwise.

*ports* [*--rebuild* _name_]...
	Build the ports in _ports/_, software from elsewhere built for Radix,
	each after the ports it needs: the C library (mlibc) and the
	C++ headers it is built with, Rust's standard library, the C.UTF-8
	locale, the crates.io crates patched for Radix, uutils' core
	utilities and findutils, ncurses, zlib, less, mandoc, GNU grep, sed,
	diffutils and patch, awk and bc, gzip, bzip2, xz, zstd and lz4,
	libarchive, Zip, file and jq, and Lua, mlibc's own tests and
	libc-test, which the *posix* boot test runs. A port is built again only when something that went into
	its last build changed: its directory, the toolchain (and the pinned
	Rust toolchain, for Rust's standard library) or a port it needs.
	After Rust's standard library is built, its *libc* crate is checked
	against the C library (see *PORTS*). *build* runs this first.

	*--rebuild* _name_
		Build this port even if nothing changed; repeat the option for
		more.

*crates*
	Fetch each crate from crates.io that _kernel/patches/crates.toml_
	lists, check the SHA-256 of its _.crate_ file, apply its patch from
	_kernel/patches/_ and put it in _kernel/patched/_, where
	_kernel/Cargo.toml_ takes it from in place of crates.io's: smoltcp,
	patched so that a closed socket's memory comes back (ADR 0182). A
	crate already there from the same file and patch is left as it is,
	and one the list no longer has is removed. Every *ob* command that
	runs Cargo on the kernel does this first; run it before Cargo
	commands of your own on the kernel, such as cargo-deny's, which CI
	runs after it.

*image* [*--iso*] [*--cmdline* _flags_] [*--microcode* _path_] [*--disk-check* _serial_] [*--debug* | *--release*]
	Build the kernel, then the boot image
	_out/dukovo-VERSION-x86_64.img_. The image is a hybrid: it starts
	under UEFI and under legacy BIOS, from a USB stick, a hard disk or
	optical media. Write it to a USB stick as shown in *EXAMPLES*.
	*dukovo-boot*(7) lists what the image contains.

	After grub-mkrescue's partitions the image has a Dukovo root
	partition, the fifth, of 512 MiB: Btrfs made by *mkfs.btrfs* (the
	pinned btrfs-progs of the Btrfs tests when the system's is older
	than 6.17), with the system in the read-only subvolume _@system.a_
	and empty subvolumes for settings, state, applications, users and
	shared files. Booted from a disk Radix sees, Spark mounts them
	(*spark*(8)).

	Every image carries a license manifest. *ob* lists each component
	that goes into it: the crates linked into the kernel (from *cargo
	tree*), the standard library crates built from the Rust sources, GRUB,
	and the microcode if any. It checks each component's SPDX license
	against the allow-list of its area, and fails if a component declares
	no license or one outside that list. Kernel crates follow
	_kernel/deny.toml_, which allows permissive licenses only; GRUB, a
	separate program, may be GPL-3.0-or-later. It then writes an SPDX 2.3
	document, the text of every license it names, and each component's
	own notice files to _/system/resources/licenses/_ in the image, and a copy
	of the document next to the image.

	*--iso*
		Name the output _out/dukovo-VERSION-x86_64.iso_ instead, for
		virtual machines and optical media. It has no Dukovo root
		partition, as optical media are read-only.

	*--cmdline* _flags_
		Kernel command-line flags instead of *boot.cmdline*, such as
		*"kdb loglevel=debug"*. *dukovo-boot*(7) lists the flags.

	*--microcode* _path_
		Add the Intel microcode updates in _path_, a file or a directory
		such as _/lib/firmware/intel-ucode_, instead of those named by
		*image.microcode*. Every update is checked, and the command fails
		if _path_ holds none. See *MICROCODE* in *dukovo-boot*(7).

	*--disk-check* _serial_
		Add a second boot menu entry, *Dukovo disk check: erases the disk
		with serial* _serial_, which runs the disk check of a real
		machine on the disk with that serial number and on no other
		(ADR 0122), after the kernel's benchmarks (*selftest=bench
		init-after-selftest*) and then the POSIX benchmarks, whose
		numbers its report keeps, with Spark mounting no disk (ADR
		0124). Put Linux's side of the
		check in the image's _crosscheck_ directory: the program built
		for Linux, linked against the C library of the live system it
		runs on, *linux-bench* and the POSIX benchmarks built for Linux
		as static programs, and _run.sh_, which runs them there and
		saves both systems' numbers for *bench --from*. Everything on the
		disk is lost. The handbook's Supported hardware chapter gives the
		steps. The kernel command line may not have a *--* part, or a
		*selftest* flag, with it.

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

*test* [*unit* | *boot* | *kani* | *tla* | *corpus*] [*--case* _name_]... [*--timeout* _seconds_] [*--debug* | *--release*]
	Run the unit tests of the build tool and the kernel libraries on this
	machine, then the boot tests: build a test image and boot it in QEMU
	once per case below. Each case follows the serial console, types
	*kdb* commands where it needs to, and reads the framebuffer console's
	text from QEMU screenshots by matching the kernel's own font. A case
	passes when every expected line appears in order and QEMU exits as
	expected, usually through the *qemu-exit* flag (see
	*dukovo-boot*(7)). Serial logs and screenshots go to _out/test/_.
	Give *unit* or *boot* to run only that part. Without KVM the boot
	tests take a few minutes.

	The Btrfs tests make their filesystems with *mkfs.btrfs*. When the
	system's lacks *--subvol*, *--compress* or *--inode-flags*, as Ubuntu
	24.04's btrfs-progs 6.6 does, *test* fetches Ubuntu 26.04's
	btrfs-progs package (6.17.1), checks its SHA-256, and unpacks it into
	_.cache/btrfs-progs/_ with *dpkg-deb*(1), without installing it.

	*kani* checks the Kani proof harnesses of the kernel's memory manager,
	capability core, scheduling policy and IPC arithmetic instead (see
	*Verification* in _docs/design/memory.md_,
	_docs/design/capabilities.md_, _docs/design/scheduler.md_ and
	_docs/design/ipc.md_), and of the storage libraries' bounds: block
	requests and partitions, paths, and FAT's table entries and long
	names. Each takes
	from seconds to several minutes, so plain *ob test* leaves them out,
	and CI runs them in a job of their own. It needs the Kani release
	that *ob doctor* names, installed with *cargo install --locked
	kani-verifier --version* _version_ and then *cargo kani setup*.

	*tla* checks the TLA+ models with the TLC model checker instead: the
	capability store's derivation tree, retyping and revocation, in
	_kernel/cap/tla/_, twice for the safety properties (once with more
	capabilities, once with memory nested deeper) and once, on a smaller
	model, for liveness; how the scheduler blocks, wakes and switches
	threads between CPUs, once for safety and once for liveness, and how
	an idle CPU is woken, in _kernel/sched/tla/_; how endpoints hand
	messages, calls and replies between threads, in _kernel/ipc/tla/_,
	once for each; how the ends of a shared-memory channel wake each
	other, in _userspace/libradix/tla/_; how a FAT volume's clusters
	change hands while their data waits to be written back, in
	_kernel/fs/fat/tla/_; and how futex waits (twice: wakers that wake
	one each, and one that wakes several in batches), *poll*, the timer
	thread and file locks wait without losing a wake-up, in
	_kernel/radix/tla/_. That takes about twelve minutes on 4 CPUs, so
	plain *ob test* leaves it out too, and CI runs it in a job of its
	own. TLC needs Java 11 or later (the *default-jre-headless* package on
	Debian, Ubuntu and Linux Mint). The first run downloads the pinned
	release of the TLA+ tools from GitHub into _.cache/tla/_ and checks
	its SHA-256; TLC's working files go to _out/tla/_.

	*corpus* runs the shell corpus instead (ADR 0103): the sources that
	_userspace/programs/nukesh/tests/corpus.toml_ lists, each a git
	repository pinned by its commit and fetched into
	_target/ports/sources/_ once. smoosh's tests of the POSIX shell must
	give what their files expect under Nukesh as *sh*; every other
	source's tests and scripts must give the same exit status and
	standard output under Nukesh as under dash or bash, whichever the
	source is written for. A test listed in its source's *known* table,
	with the reason, may differ; one that no longer does is reported, so
	that its entry goes. *--case* _name_ runs only that source. The corpus
	runs in a sandbox: new user, mount, PID and network namespaces, the
	system read-only, the corpus copied into a tmpfs on _/tmp_, and each
	test as *nobody* in a PID namespace of its own. It needs dash, bash,
	*unshare*(1), *prlimit*(1) and *setpriv*(1), a C compiler, and, as a
	user other than root, user namespaces, which Ubuntu 23.10 and later
	allow only after *sudo sysctl
	kernel.apparmor_restrict_unprivileged_userns=0*. Plain *ob test*
	leaves it out, and CI runs it in a job of its own; it takes about
	seven minutes.

	*uefi-usb*, *bios-usb*, *uefi-cdrom*, *bios-cdrom*
		Boot with 4 CPUs under each firmware and medium, and check the
		memory map, the KASLR layout, that W^X holds, the frame
		allocator's zones, the CPU list, timer interrupts on every CPU
		and an IPI to each. *uefi-cdrom* boots with *nox2apic* and checks
		for xAPIC mode; with KVM, which always offers x2APIC, the other
		three check for x2APIC mode. *bios-cdrom* boots with *nokaslr*
		and *nopcid*, and *bios-usb* with *lockdep*, so the lock checker
		watches a whole boot of a release build.

	Every boot test also fails if the kernel logs a lock checker report
	(*Lock checker:* lines), except in the *lockdep* case that seeds
	them, or a thread that would sleep with preemption held off.

	*screen*
		Boot under UEFI and read the boot log back from the framebuffer.

	*consoles*
		Boot normally, with no program on the command line, so Spark
		starts its services: read Nukesh's prompt off consoled's tty1,
		type a command on QEMU's PS/2 keyboard and read its answer back,
		switch to tty2 with Alt+F2, switch the keyboard to the German
		layout with *keymap*(1) and type a Z, an E with a dead acute and
		an @ with AltGr, restart consoled with *service*(8), which must
		keep the layout, switch back to US, and power the machine off
		with *poweroff*(8) (*consoled*(8), *spark*(8)).

	*power-button*
		Boot normally under BIOS, and once consoled has started, press
		the machine's power button with QMP's _system_powerdown_, which
		raises ACPI's power button event. The kernel must say it hears
		the button through uACPI, then that it was pressed; Spark must
		stop the services and the kernel power the machine off through
		uACPI, and QEMU end on ACPI's S5 (*radix-power*(2), *spark*(8),
		*dukovo-boot*(7)'s *ACPI*).

	*aml-off*
		As *power-button*, under UEFI and with *aml=off*: no AML runs, so
		the kernel must hear the button and power the machine off by
		itself.

	*acpi-power*
		Boot normally under BIOS with an SSDT of the test's
		(_tools/xtask/src/acpi/power.asl_, through QEMU's _-acpitable_),
		as QEMU has no battery, AC adapter, lid or power button of the
		ACPI namespace: the kernel must say what each is, through uACPI.
		At the console, the battery's and the adapter's files in
		_/sys/class/power_supply_ and the lid's
		_/proc/acpi/button/lid/LID0/state_ must read as the table says,
		and the fixed power button must still power the machine off
		(*radix-sysfs*(5), *radix-proc*(5), *dukovo-boot*(7)'s *ACPI*).

	*usb-devices*
		Boot normally under UEFI with the lock checker on and the *dbc*
		flag, with QEMU's USB keyboard on the xHCI controller's second
		port beside the boot stick, and a hub on the third with a mouse
		behind it and a second hub with a keyboard behind that. The kernel
		must name the controller, say it has no Debug Capability, and name
		each hub and each device on its port; USB tablets
		plugged into the fourth port and into the first hub with QEMU's
		_device_add_ must be named too, and pulling them out with
		_device_del_ logged; pulling out the second hub must let go of its
		keyboard. The power button ends the test (*dukovo-boot*(7)'s
		*USB*).

	*usb-input*
		Boot normally under UEFI with the lock checker on, with QEMU's
		USB keyboard and mouse on the xHCI controller's second and
		third ports, the keyboard taking QEMU's input from the screen.
		The kernel must say what each is read into. On the console,
		typing on the USB keyboard and on the PS/2 one must reach the
		same shell; holding a key on the USB keyboard must repeat it;
		_/devices/mouse_ must read the mouse moving and its button
		going down and up, then where a USB tablet plugged into the
		fourth port with _device_add_ points; and pulling out the
		keyboard with a key held must stop its repeat
		(*radix-input*(2)).

	*watchdog*, *watchdog-tco*
		Boot with *--watchdog 12 --test-hang* for Spark, with QEMU's
		i6300esb under BIOS (*watchdog*) or with the TCO timer of q35's
		ICH9 under UEFI (*watchdog-tco*). The kernel must find the
		watchdog and Spark start it, then stop responding; the watchdog
		must restart the machine, and the next boot must say it did.
		*watchdog* also boots with *kdb*, which must keep the machine
		from restarting while it waits, for twice the timeout. Spark
		then pets it for twice its timeout without a restart, and the
		power button powers the machine off (*spark*(8)'s *WATCHDOG*).

	*machine-check*
		Boot with *mce-poll=1 panic=0* and fill the CPUs'
		machine-check banks with QEMU's _mce_ monitor command, as a
		CPU would: a corrected cache error on CPU 1, which the kernel
		must log, decoded, within a second; corrected memory read
		errors, two on channel 1 of bank 3 and one naming no channel,
		which _/kernel/devices/system/edac/mc/mc0_ must count, as the
		console reads them (*radix-sysfs*(5)); then an uncorrected memory
		error on CPU 0 with the processor's state corrupt, which must
		panic with the decoded error. The machine restarts, the next
		boot must find the error in its bank, and the power button
		powers the machine off (*dukovo-boot*(7)'s *MACHINE CHECKS*).

	*machine-check-program*
		Boot 2 CPUs of an emulated Intel Skylake, whose software error
		recovery QEMU emulates, with *mce-poll=1*, and point Spark at
		_/system/tests/posix/memfail_, which reads a page of its own in a
		loop. A memory scrubbing error the CPU goes on after, on CPU 0,
		must be logged and its page retired. With the machine stopped
		where a CPU runs memfail in user mode, an uncorrected data load
		error naming its page's physical address (QEMU's _gva2gpa_) must
		end only memfail, with SIGBUS (*BUS_MCEERR_AR*, the page's
		address, *si_addr_lsb* 12); its handler's next read of the page
		must fault without a machine check, and _/processes/meminfo_
		must count 8 KiB of bad memory (ADR 0168).

	*spark-oom*
		Boot with 256 MiB and point Spark at the test service _hog_,
		which takes memory until the out-of-memory policy stops its
		domain; check that Spark says so and starts it again, waiting
		longer the second time (*spark*(8)).

	*swap*
		Boot 2 CPUs with 256 MiB and a 1 GiB virtio disk the host's
		*mkswap* made a swap area of. _/system/tests/posix/swaptest_
		must turn it on with *swapon*(8), find that it cannot be turned
		on twice and that the disk refuses writes, fill 384 MiB with a
		pattern and read it all back, fork a child that checks half of
		it and changes a quarter while the parent's stays, and turn the
		area off with *swapoff*(8) while some of the 64 MiB it keeps is
		still on it, all without the out-of-memory policy stopping a
		domain. The area's header must be as *mkswap* made it after the
		boot.

	*spark-stop*
		Point Spark at three test services: _stubborn_ ignores SIGTERM,
		and Spark stops its domain 5 seconds after *service stubborn
		restart* and again after *stop*; _leaver_ ends and leaves a
		process in its domain, which Spark stops; _driver_ makes the
		requests and powers the machine off (*spark*(8)).

	*spark-network*
		Boot 2 CPUs under UEFI from USB and point Spark at two test
		services that run _/system/tests/posix/netgrant_: _granted_ asks
		to connect out and to listen on ports 8000 to 8009 and 9000, and
		checks that it holds exactly those three network capabilities,
		that the C library's sockets listen on ports 8005 and 9000 and
		connect out, and that binding port 8010 fails with EACCES; _none_
		asks for nothing, and checks that it holds no network capability
		and cannot make a socket (*spark*(8)).

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

	*fpu*
		Boot 4 CPUs with *selftest=fpu*: three threads per CPU fill the
		vector registers with their own values and check them after
		every switch, a thousand times each.

	*sched-fair*
		Boot 4 CPUs under UEFI with *selftest=sched-fair*: fair threads
		on one CPU get equal shares, a thread reniced to nice 5 gets the
		share its weight gives, and two resource domains of CPU weights
		200 and 100 get two thirds and one third of what they share.

	*sched-rt*
		Boot 4 CPUs with *selftest=sched-rt*: a woken real-time thread of
		higher priority runs at once, a timer wakes a real-time thread
		within a millisecond (median) even on a busy CPU, FIFO threads of
		one priority run in turn and round-robin ones share the CPU.

	*sched-pi*
		Boot 4 CPUs under UEFI with *selftest=sched-pi*: a low-priority
		thread holding a mutex that a high-priority thread wants runs at
		the high priority until it lets go, so a thread of middle
		priority cannot keep both waiting, also when the owner itself
		waits for a mutex held by a third thread.

	*sched-balance*
		Boot 4 CPUs on 2 NUMA nodes under UEFI with
		*selftest=sched-balance*: eight threads started on one CPU spread
		out evenly over all four, across both nodes.

	*lockdep*
		Boot 4 CPUs with *selftest=lockdep*, which seeds deadlocks: two
		spin locks and two mutexes taken in opposite orders on different
		CPUs, and a mutex taken under a spin lock. The case checks that
		the lock checker reports each one, with the places where both
		orders were seen.

	*rcu*
		Boot 4 CPUs under UEFI with *selftest=rcu*: readers on the other
		three CPUs never see an object freed after it was replaced
		before a grace period passed.

	*user*
		Boot 2 CPUs under UEFI with *selftest=user*. A user program
		from the test image calls every system call and capability
		method with good and bad arguments and checks each answer: its
		start page, retyping memory into frames and mapping them,
		deriving, moving, sealing and revoking capabilities, random
		bytes, threads with their own thread pointer, the kernel log
		(cut at 256 bytes, with control characters and bad UTF-8
		replaced), rings, and processes: making one and the objects
		charged to it, building it from outside, ending it, hearing of
		its end, and that a process made for it ends with it. Then eight
		programs each make one fault: a
		null pointer, a write to a read-only page, code on the stack, a
		privileged instruction, a jump into the kernel's half, an
		undefined instruction, a breakpoint and a division by zero. The
		case checks the kernel's report of each and that it ended only
		that program.

	*ipc*
		Boot 2 CPUs with *selftest=ipc*: pairs of user programs, on one
		CPU and then on two, call and answer over an endpoint, pass
		capabilities in messages, signal each other's notifications,
		send messages through a shared-memory channel, and post to each
		other's rings, a thousand times each, checking every answer.

	*fuzz*
		Boot 2 CPUs with *selftest=fuzz* and the lock checker on: eight
		groups of three user programs make 4,000 random system calls,
		methods and ring entries each, sharing endpoints, notifications,
		memory and a tmpfs of their own, making, building, starting and
		ending processes, and calling every directory and file method on
		files they make. The case fails if the kernel panics or the lock
		checker reports, if a failed call returns anything but an error
		that its documentation lists with 0 in every result register, if
		the capability store fails its audit or keeps anything
		afterwards, or if the tmpfs, with whatever they mounted in it,
		cannot be taken away. The log names the seeds; boot with
		*fuzz-seed=*_n_ to repeat a run.

	*spark*
		Boot 2 CPUs under UEFI from USB with the command line *lockdep --
		/system/tests/hello one two*. The kernel starts Spark from the
		boot archive (see *spark*(8)); Spark starts _/system/tests/hello_
		with the arguments *one two*, the program prints them, fills its
		heap and exits, and Spark reaps it. The case checks every line
		of both programs, that each was placed at a random page-aligned
		address in the image window, the two apart, that the program
		exited with 0, and that all of its memory came back.

	*storage*
		Boot 2 CPUs under UEFI with four virtio disks, formatted on the
		host by *mkfs.fat*: FAT12, FAT16 and FAT32 disks used whole, and
		one with an MBR partition table holding a FAT32 partition. Spark
		builds the directory hierarchy and mounts each volume at
		_/volumes/<name>_; _/system/tests/storage write_ writes files of
		many sizes, a long-named one and directories to each, checks
		_/tmp_, _/devices_, _/kernel_ and _/processes_, and syncs. On the host,
		*fsck.fat -n* checks every volume and mtools reads every file
		back; the host then writes a file to one. A second boot from the
		same disks runs _/system/tests/storage check_, which reads
		everything back, the host's file too, and checks that _/processes_
		lists the running processes. dosfstools and mtools must be
		installed (*ob doctor* says how).

	*storage-sata*
		The *storage* test with the same four disks on SATA, the ports
		of an AHCI controller added to the machine (QEMU's *ich9-ahci*),
		the last of them solid state, with its partition in a GPT rather
		than an MBR. The kernel must name them _sda_ to _sdd_, find
		their queue depth, write cache and TRIM, and name the partition
		_sdd1_ with its GPT name. The host checks that both copies of
		the GPT are as it wrote them.

	*storage-usb*
		The *storage* test with the same four disks as USB sticks
		(QEMU's *usb-storage*) on the boot stick's xHCI controller: two
		at SuperSpeed on root ports, and two at full speed behind a hub,
		the partition in a GPT. The kernel must name them _sdb_ to
		_sde_ after the boot stick's _sda_, say where each is, and
		name the partition _sde1_; Spark leaves the boot stick's EFI
		system partition alone. After the second boot's check, the
		first stick is pulled out, which takes _sdb_ away, and plugged
		in again, which gives it _sdb_ again.

	*storage-uas*
		The *storage* test with the same four disks on three USB devices
		that speak USB Attached SCSI (QEMU's *usb-uas*, with a *scsi-hd*
		for each logical unit), on the boot stick's controller with two
		SuperSpeed root ports: two logical units of one at SuperSpeed,
		whose commands go on streams, and one unit each of two at high
		speed, which have none, the partition in a GPT. The kernel must
		find the first device's units with REPORT LUNS, name them _sdb_
		and _sdc_, and the others _sdd_ and _sde_, and say each is driven
		over UAS. After the second boot's check, the device with two
		units is pulled out, which takes _sdb_ and _sdc_ away.

	*volumes*
		A FAT stick labelled _DUKOVOUSB_ on the boot stick's controller,
		with the default services (Spark's *--services*) and
		_/system/tests/posix/volumetest DUKOVOUSB_. Spark mounts it at
		_/volumes/sdb_ at boot, and *volumed*(8) leaves it there. Pulled
		out, it must be unmounted; plugged in again, mounted at
		_/volumes/DUKOVOUSB_, where the program writes _ejected.txt_,
		checks *volumed --list* shows it, ejects it with *eject*(1) and
		checks it is gone. Pulled out once more, the host reads the file
		back with mtools.

	*storage-nvme*
		The *storage* test with the same four disks as NVMe namespaces,
		two on each of two controllers (QEMU's *nvme* and *nvme-ns*), on
		4 CPUs, the partition in a GPT as in *storage-sata*. The kernel
		must name them _nvme0n1_, _nvme0n2_, _nvme1n1_ and _nvme1n2_, the
		partition _nvme1n2p1_, and give each controller 4 I/O queues with
		an MSI-X vector for each CPU.

	*storage-4k*
		The *storage* test on disks of 4 KiB sectors (QEMU's
		_logical_block_size=4096_): FAT12 and FAT16 on two virtio disks,
		and FAT32 on two 288 MiB NVMe namespaces, the smallest FAT32
		volumes of such sectors, the second in a GPT counted in 4 KiB
		blocks. Each filesystem has sectors of 4 KiB too
		(*mkfs.fat -S 4096*). The kernel must say that the disks have
		4096-byte sectors and give the partition's start in 512-byte
		sectors, as Linux does.

	*exfat-write*
		The *storage* test on two exFAT disks *mkfs.exfat* made: a 64
		MiB NVMe namespace with clusters of 32 KiB, larger than a page,
		and a 32 MiB virtio disk with clusters of a sector. Spark must
		mount both, as exFAT, to write. After each boot, exfatprogs'
		*fsck.exfat -n* must find nothing wrong; between the boots, The
		Sleuth Kit (*fls* and *icat*), an exFAT reader of its own, must
		read every file back as written, and none of those renamed or
		removed. The host writes no file of its own to them: no tool on
		Linux writes to exFAT without mounting it. exfatprogs and The
		Sleuth Kit must be installed (*ob doctor* says how).

	*disk-raw*
		Boot 2 CPUs under UEFI with three 8 MiB disks of known bytes, a
		virtio disk, a solid state SATA disk and an NVMe namespace, and no
		filesystem. _/system/tests/storage raw vda sda nvme0n1_ asks each
		disk Linux's block
		requests (*radix-block*(2)) and checks them against _/kernel/block_,
		reads at odd offsets and across the end, writes at odd offsets,
		parts of sectors and across the end, reads every write back,
		discards a megabyte, flushes, and checks what is refused: a
		discard of part of a sector or past the end, a terminal request,
		and a discard or a write without the write right. On the host,
		every byte of every disk must be what the writes left, with the
		discarded range zeroed (QEMU punches holes for discards).

	*disk-raw-4k*
		The *disk-raw* test on a virtio disk and an NVMe namespace of
		4 KiB sectors, where a part of a sector is a part of 4 KiB: the
		kernel reads the whole sectors around it and writes them back,
		and a discard must cover whole 4 KiB sectors.

	*iommu*
		The *disk-raw* test behind QEMU's Intel VT-d unit (*intel-iommu*),
		with the virtio disk told to go through it and QEMU's *edu* test
		device added, on 2 CPUs under UEFI booted from the USB stick: the
		unit must translate with 3-level page tables and queued
		invalidation, every disk and the stick must work with each device
		reaching only what is mapped for it, and *selftest=iommu*
		(*radix-debug*(7)) must pass, its two stopped writes logged (ADR
		0164). The unit remaps interrupts (QEMU's *intremap=on*, with
		*kernel-irqchip=split* under KVM), and the case ends with the
		power button, whose SCI comes through the I/O APIC's interrupt
		entry: Spark must power the machine off (ADR 0166).

	*iommu-amd*
		The *iommu* test behind QEMU's AMD IOMMU (*amd-iommu*), which the
		ACPI IVRS table describes: the IOMMU must translate with 4-level
		page tables and 65536 domains, every disk and the stick must work
		with each device reaching only what is mapped for it, and
		*selftest=iommu* must pass, and the power button must power the
		machine off through the remapped SCI. QEMU's AMD IOMMU logs no
		event for a page that is not mapped, so the test counts none (ADR
		0165, 0166).

	*net*
		Boot 2 CPUs under UEFI from the USB stick with a virtio network
		card on QEMU's user network, behind QEMU's VT-d unit, and
		*selftest=net* with *ip=10.0.2.15/24* and *gateway=10.0.2.2*: the
		card must be found with the MAC address QEMU gives it and 256
		slots each way, take the address and router of the flags, and
		QEMU's router must answer all four pings (*radix-debug*(7), ADR
		0180, 0181).

	*sockets*
		Boot 2 CPUs under UEFI from the USB stick with a virtio network
		card on QEMU's user network, which runs *cat* for each connection
		to 10.0.2.100 port 7, and *ip=10.0.2.15/24* and
		*gateway=10.0.2.2*. Spark starts _/system/tests/sockets virtio_
		with its network capability (*--network*): the program derives a
		capability that may only connect out and one that may only listen
		on ports 8000 to 8009; connects, listens and takes connections
		over TCP on the loopback interface, sends a line each way and a
		mebibyte one way, and shuts each end down in turn; sends UDP
		datagrams each way; checks that each method refuses what the
		capability it is given does not allow; and connects through the
		card to QEMU's *cat*, which must send back what it sent. Every
		socket's memory must come back to the kernel (*radix-network*(2),
		ADR 0183).

	*disk-check*
		Boot 2 CPUs under UEFI with a 128 MiB NVMe disk whose serial
		number is _dukovo-test-0_, and run the disk check of a real
		machine on it (*ob image --disk-check*, ADR 0122):
		_/system/tests/posix/disktest --yes-erase dukovo-test-0_ must
		find the disk by its serial number, write its 64 places, read
		them back, measure its transfers and save its report on the
		disk. The kernel's benchmarks run first, Spark mounts no disk
		(*--no-volumes*), and the program then runs the POSIX benchmarks
		and keeps their lines and the kernel's in its report (ADR 0124). The host then builds Linux's side of
		the same program and runs it on the disk's file, which must find
		the report, read back every place and give the benchmarks'
		lines.

	*disk-errors*
		Boot 2 CPUs under UEFI with an 8 MiB SATA disk and an 8 MiB NVMe
		namespace through QEMU's *blkdebug* driver, which fails a read
		of sector 6000 once, and every read of sector 10000 and write of
		sector 12000. _/system/tests/storage errors sda nvme0n1_ reads the
		first, which must come back on its retry, and the others, which
		must end in EIO after five tries each, with a read that reaches
		the failing sector ending short; then reads and writes elsewhere
		must work. The kernel must log each failed request, with the
		NVMe status for the namespace's.
		The kernel must log each recovery of the port, 16 in all, and
		the disk must hold on the host what it held before.

	*disk-health*
		Boot 2 CPUs under UEFI with an 8 MiB SATA disk, NVMe namespace
		and virtio disk, as on a normal boot. Spark must start the
		*diskhealth* service after *consoled*, and it must log that the
		NVMe drive and the SATA disk pass. At the console, *diskhealth*
		must show both passing and the virtio disk keeping no record,
		with exit status 0, and *diskhealth -a* the SATA disk's
		attributes and the NVMe drive's health log (ADR 0157).

	*storage-tools*
		Boot 2 CPUs under UEFI with a 48 MiB virtio disk holding FAT32 in
		a GPT partition and a 128 MiB virtio disk of Btrfs, as on a
		normal boot. At the console, *lsblk* must list the partition and
		the Btrfs disk with their kinds, labels and where Spark mounted
		them; *blkid* their labels and kinds, the GPT and the disk with
		the Btrfs label; and *findmnt* the Btrfs mount's source, kind and
		options and the FAT mount's target, and nothing for a kind no
		mount has, with exit status 1. *fstrim -v* must trim the Btrfs and
		say how much, and refuse the FAT, which does not discard;
		*iostat* must show the Btrfs disk, and its _stat_ count the
		discards. On the host, *btrfs check* must pass on the trimmed
		disk.

	*btrfs*
		Boot 2 CPUs under UEFI with two 128 MiB Btrfs disks that
		*mkfs.btrfs* made of the same files: an NVMe namespace of 4 KiB
		sectors with BLAKE2b, nodes of 4 KiB, metadata once, data
		compressed with zstd and its holes named (*-O ^no-holes*), and a
		virtio disk as it makes one by default (CRC-32C, nodes of 16 KiB,
		metadata twice) but for data compressed with zlib. The files are of
		sizes around the inline and sector limits, sparse, deep and wide,
		with text that compresses, links of both kinds, a FIFO, a name
		that is not ASCII and a subvolume; the host changes a byte of one
		file's second sector on each disk. Radix does not write named
		holes, and the virtio disk is attached read-only (QEMU's
		_readonly=on_), so Spark must mount both read-only, and
		_/system/tests/storage list_ must list every file, directory and
		link as the host made it, with each file's size and FNV-1a hash,
		the damaged file failing with *io*, and every write refused with
		*read_only*. On the host, each disk must be byte for byte as it
		was made.

	*btrfs-write*
		The *storage* test on two 128 MiB Btrfs disks of the *btrfs*
		test's kinds, uncompressed, which *mkfs.btrfs* made holding only
		the file the host writes to FAT disks between the boots. Spark
		must mount both to write; on Btrfs the program also writes
		_grow.bin_, 12 MiB, past the first data chunk. After each boot,
		Linux's *btrfs check --check-data-csum* must find nothing wrong;
		between the boots, *btrfs restore* must copy out every file as
		written, and none of those renamed or removed.

	*btrfs-stress*
		The long filesystem stress run of Phase 10. Boot 4 CPUs under
		UEFI with the *btrfs-write* test's two disks, the NVMe one
		compressing with zstd. _/system/tests/posix/torture_ starts two
		workers on each volume, all at once, which for two minutes make
		random changes, each in a directory of its own: files written
		whole, in part, past their end and appended to, cut and grown,
		saved through a temporary file and a rename, renamed over each
		other and between directories, linked and removed, symbolic
		links made and renamed over each other, and directories of small
		files made and removed, with *fsync*(2) of files and directories
		between. Each worker keeps in memory what its files should hold,
		reads one back now and then, checks everything at the end and
		syncs it. *btrfs check --check-data-csum* must then find nothing
		wrong. Booted again, _torture --check_ works out each worker's
		changes again from the same seed, as many as it made, and every
		file's bytes, size and link count, every symbolic link and every
		directory's entries must be as they leave them; then it removes
		it all, and *btrfs check* must pass once more. The program runs
		on Linux too, where its answers were checked.

	*btrfs-power-cut*
		Boot 2 CPUs under UEFI with a 128 MiB Btrfs disk as *mkfs.btrfs*
		makes one by default. _/system/tests/storage churn_ writes a file,
		syncs it, renames it and syncs the directory, then removes the
		file of eight rounds before and syncs again, saying so after each
		sync, and the host kills QEMU 0 to 1.5 seconds after round 30,
		mid-write. *btrfs check* must find nothing wrong, and as *btrfs
		restore* copies the files out, each file said synced must be
		whole unless said removed, each said removed gone, and any other
		whole or a start of itself. Booted again,
		_/system/tests/storage churned_ must find the same, remove the
		files and write and sync another, and *btrfs check* must pass
		again.

	*btrfs-log*
		Boot 2 CPUs under UEFI with the 128 MiB Btrfs disk Linux left
		with changes in its log of fsyncs alone, which *ob fixture
		btrfs-log* made. Spark must mount it, which replays the log;
		_/system/tests/storage replayed_ must list every file, directory
		and link as Linux listed them after its own replay of the same
		disk, then write and sync a file. The superblock must name no
		log after the boot, and *btrfs check* must find nothing wrong.

	*btrfs-raid*
		Boot 2 CPUs under UEFI with two 512 MiB virtio disks of one
		Btrfs RAID1 *mkfs.btrfs* made, and a third and a fourth of raw
		bytes. Spark must mount it once, from _vda_, leave _vdb_ alone as
		in use and _vdc_ and _vdd_ as holding nothing;
		_/system/tests/storage write_ writes its files. Between the boots
		the host wipes _vdb_'s superblock, as if it were pulled out. The
		second boot must mount _vda_ alone, with the kernel's warning of
		a disk missing, and _/system/tests/storage
		check --no-host-file --quota --replace 2 vdc --add vdd --balance --remove
		1_ must read every byte back, turn simple quotas on (ADR 0170), put
		_vdc_ in the place of _vdb_ (device 2), which the
		kernel rebuilds from _vda_'s copies with none left unmended, add
		_vdd_ (device 3), move every chunk, and take _vda_ out (device 1),
		its chunks moved to the others (ADR 0159), reading every byte
		back after each, and show what the top subvolume's quota group
		counts. After each boot *btrfs check* must find nothing wrong,
		its quota groups' counts among it, given every disk still there,
		and given each of them alone.

	*btrfs-raid-pull*
		Boot 2 CPUs under UEFI with two 512 MiB virtio disks of one
		Btrfs RAID1 *mkfs.btrfs* made, one of which QEMU's blkdebug
		fails, as if pulled out, at a random write: every read, write and
		flush from then on fails with EIO. _/system/tests/storage write_
		must write its files without an error, and the kernel must warn
		that a disk failed and was left out. *btrfs check* must then find
		nothing wrong from each disk alone. The second boot has the disk
		back: the kernel must warn that it missed changes, and
		_/system/tests/storage check --no-host-file --scrub_ must read
		every byte back and scrub, leaving nothing no copy mends. After
		it *btrfs check* must find nothing wrong from both disks, and
		from each alone. A failure names the disk pulled and after how
		many writes.

	*ext4-write*
		The *storage* test on two ext4 disks *mkfs.ext4* made holding
		only the file the host writes to FAT disks between the boots: a
		64 MiB NVMe namespace of 4 KiB sectors with blocks of 4 KiB, as
		it makes one by default, and a 32 MiB virtio disk with blocks of
		1 KiB, smaller than a page. Spark must mount both, as ext4, to
		write. After each boot, e2fsprogs' *e2fsck -fn* must find nothing
		wrong, both on the disk as Radix left it and on a copy whose
		journal *e2fsck* replayed; between the boots, *debugfs* must copy
		out every file as written, and none of those renamed or removed.
		e2fsprogs must be installed (*ob doctor* says how).

	*ext4-power-cut*
		The *btrfs-power-cut* test on a 64 MiB ext4 disk as *mkfs.ext4*
		makes one by default: the host kills QEMU after round 30 of
		_/system/tests/storage churn_, mid-write, and on a copy of the
		disk whose journal *e2fsck* replayed, as Linux would at its next
		mount, *e2fsck -fn* must find nothing wrong, and as *debugfs*
		copies the files out, each file said synced must be whole unless
		said removed, each said removed gone, and any other whole or a
		start of itself. Booted
		again, Radix replays the journal itself, and
		_/system/tests/storage churned_ must find the same, remove the
		files and write and sync another, and *e2fsck* must pass again.

	*root*
		Boot 2 CPUs under UEFI from the test image as a USB stick, with
		a copy of it and its Dukovo root partition, as *image* makes
		one, as a virtio disk. Spark must find Dukovo's root on _vda5_,
		mount it again to compress, mount its subvolumes and copy the
		default settings in. _/system/tests/storage root_ checks in
		_/processes/mounts_ that _/system_ is _@system.a_, read-only,
		and the others are on the same disk, that _/system_ refuses a
		file, and writes and syncs a file in _/system/settings_,
		_/system/state_, _/applications_, _/users_ and _/shared_.
		Booted again, Spark must add no default settings, and
		_/system/tests/storage rooted_ must read each file back. After
		each boot, *btrfs check --check-data-csum* must find nothing
		wrong in the partition.

	*posix*
		Boot 4 CPUs under UEFI from USB with the lock checker on; Spark
		starts _/system/tests/posix/run_ on a pseudo-terminal (see
		*spark*(8)). It runs every test of the POSIX layer as a shell
		runs a job, in a process group of its own in the foreground:
		the C programs of _userspace/posix/_, on the C library (fork,
		exec, signals, pipes, *poll*, terminals and job control, clocks
		and timers, files and their locks, threads and their
		cancellation, *posix_spawn*, resource limits and *printf*'s
		formats), a program linked dynamically with a library of its
		own and one it opens with *dlopen*, the test of Rust's standard
		library, the C library's own tests and Lua's test suite (see
		*radix-posix*(7)). The case checks that every test passed and
		that the runner exited with 0.

	*oom*
		Boot 4 CPUs with *selftest=oom*: a resource domain's memory limit
		refuses what passes it, and when memory runs out the kernel stops
		an app domain, then a service domain that its supervisor
		restarts, and never a system domain.

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

*bench* [*--gate*] [*--linux-kernel* _file_] [*--runs* _n_] [*--no-linux*] [*--instructions*] [*--from* _file_] [*--summary* _file_] [*--debug* | *--release*]
	Measure what system calls, IPC round trips, context switches, fresh
	pages, TLB shootdowns, memory copies, files and POSIX calls cost on
	Radix, next to Linux.
	Build a test image and boot it in QEMU on 2 CPUs with
	*selftest=bench*, where the benchmark programs of the test image time
	the cheapest system calls (an unknown number, which only enters and
	leaves the kernel; a yield; a capability method; 64 random bytes), a
	switch between two programs, a fresh page made, mapped, written and
	given back, a page unmapped while its address space runs on the other
	CPU, a copy between two 32 MiB buffers, round trips between two
	programs over an endpoint, a notification, a shared-memory channel and
	a ring, with both on one CPU and on two, three of the system calls
	and endpoint round trips (a pair of programs on each CPU) on both CPUs
	at once, and a file opened and closed, its status, and 4 KiB read
	from and written to it, on a tmpfs. Then boot it again, with Spark
	running the POSIX benchmarks, a C program on the C library
	(_userspace/posix/bench.c_): *fork*(2) and *waitpid*(2), a program
	run with *execv*(3), a pipe's throughput, a signal to itself,
	*poll*(2), futex round trips through semaphores between two
	threads, and between two processes in shared memory, on one CPU and
	on two, and reads and writes of 4 KiB, 64 KiB and 1 MiB of a virtio
	disk, _vda_ on both systems, a file on this machine.

	Then boot Linux in a virtual machine set up the same way, with
	_tools/linux-bench_ built as a static program and run as its only
	process, which times the closest Linux operations (*getppid*(2),
	*sched_yield*(2), *fcntl*(2), *getrandom*(2), *mmap*(2) with a
	first-touch fault, *mprotect*(2), the same copy, and round trips over
	pipes, eventfds, futexes and Unix sockets, pinned the same way, pipe
	round trips with a pair of threads on each CPU at once, the same file
	calls on a tmpfs, and last the same POSIX benchmarks, built from the
	same file with this machine's C library, with the same disk opened
	with *O_DIRECT*). This
	Linux boots with *mitigations=off*, since Radix has no mitigations of
	CPU vulnerabilities yet. Under KVM, Radix and this Linux are measured
	three times in turn, and each operation counts its fastest run on
	each side. Last, run _tools/linux-bench_ on this machine too, for
	reference.

	The report gives Radix, Linux in the virtual machine, their ratio and
	this machine's Linux side by side, with what the kernel did per
	operation (switches, timer writes, interrupts sent, halts, polls). It
	is printed and written to _out/bench/report.md_, and the numbers to
	_out/bench/results.json_. _docs/design/ipc.md_ says how each is
	measured. Use KVM: without it QEMU emulates every instruction, the
	Radix numbers measure the emulator, the Linux virtual machine does not
	run, and the boot takes minutes. CI runs *ob bench --gate* on every
	pull request and every merge, and shows the report in the job's
	summary.

	*--gate*
		Fail if any operation was not measured on both systems, or Radix
		takes longer than Linux in the virtual machine at any of them:
		the performance gate of the plan. While an operation is over,
		measure Radix and the Linux virtual machine in turn again, up to
		six more times, each operation keeping its fastest run.

	*--linux-kernel* _file_
		The Linux kernel to boot. Without it, *ob* uses
		*OB_LINUX_KERNEL*, else this machine's own
		_/boot/vmlinuz-$(uname -r)_ if it may read it. Debian, Ubuntu
		and Mint make that file readable only by root; copy it once with
		*sudo install -m 644 /boot/vmlinuz-$(uname -r) .cache/vmlinuz*
		and pass *--linux-kernel .cache/vmlinuz*.

	*--runs* _n_
		Measure Radix and the Linux virtual machine in turn _n_ times
		instead of three, keeping each operation's fastest run. Without
		KVM there is always one run.

	*--no-linux*
		Skip both Linux baselines.

	*--instructions*
		Count instead of time: boot Radix under QEMU's instruction
		counting (*-icount shift=0,align=off*, no KVM), where each
		instruction moves the guest's clock by a nanosecond, so each
		benchmark gives the user and kernel instructions an operation
		took, the same on every machine. The report goes to
		_out/bench/instructions.md_; no Linux runs. With *--gate*, fail
		if an operation on one CPU takes more instructions than its
		budget (*BUDGETS* in _tools/xtask/src/bench.rs_: what it took
		when the budget was set, plus a tenth). CI runs
		*ob bench --instructions --gate* on every pull request and every
		merge, so a longer hot path fails there even when the timings are
		noisy.

	*--from* _file_
		Measure nothing, and report the numbers of a real machine
		instead: Dukovo's and Linux's, which the disk check measures on
		it in turn (*image --disk-check*, ADR 0124) and Linux's side
		saves as _dukovo-bench.txt_, given as _file_. Each row compares
		Dukovo with Linux on the same machine, and the report gives the
		processor's clock on each. It is printed and written to
		_out/bench/machine.md_. A real machine's numbers are a check,
		not a gate, so *--from* takes no other option but *--summary*.

	*--summary* _file_
		Also append the Markdown report to _file_. CI passes
		*$GITHUB_STEP_SUMMARY*.

	*--debug*, *--release*
		As for *build*.

*fixture* *btrfs-log*|*btrfs-radix-log* [*--linux-kernel* _file_]
	Make test data that only Linux makes, for the host tests of Radix's
	filesystems. *btrfs-log* makes a 128 MiB Btrfs disk with
	*mkfs.btrfs*, boots Linux in a virtual machine on it with
	_tools/linux-bench_ as its only process (*--btrfs-log make*), which
	commits a set of files, changes them with an *fsync*(2) after each
	change and powers off, so that the changes are in Btrfs's log tree
	alone; then boots it again on a copy (*--btrfs-log list*) to list the
	disk as its last commit has it and as Linux has it after replaying
	the log. The disk, kept as its blocks that are not all zeros, and the
	lists go to _kernel/fs/btrfs/src/tests/fixtures/_, which the
	Btrfs host tests check Radix's replay against (ADR 0151). The
	fixtures are checked in; make them again only to change what the
	log holds.

	*btrfs-radix-log* checks the other way round (ADR 0154): a Btrfs host
	test writes files on a disk *mkfs.btrfs* made, changes them with an
	*fsync*(2) after each change and cuts the power after the last, so
	the changes are in the log Radix wrote alone; Linux boots on a copy
	(*--btrfs-log list*) to list it before and after replaying that
	log, and *btrfs check* checks what the replay left. The disk and the
	lists go to the fixtures, which the host tests check Radix's own
	replay against.

	*--linux-kernel* _file_
		As for *bench*; the kernel needs Btrfs, built in or as a module
		in *OB_LINUX_MODULES*.

*docs* [*--check*]
	Build the man pages from _docs/man/\*.scd_ into _out/docs/man/_ and the
	handbook from _docs/handbook/_ into _out/docs/handbook/_. The pages of
	section 2, one for each system call and one for each interface, are
	generated from _abi/\*.ridl_ by *ridlc* (see *radix-ipc*(7)); their
	scdoc sources go to _out/docs/man-src/_. Every man page
	is checked with *mandoc -T lint*, and any warning fails the command.

	*--check*
		Also fail when a man page title does not match its file name, or
		when one of these is not documented in bold in its page: an *ob*
		command (this page), an _dukovo.toml_ key (*dukovo.toml*(5)), a
		kernel flag (*dukovo-boot*(7)), a *kdb* command or a self-test
		(*radix-debug*(7)).

*lint*
	Check formatting with rustfmt and run Clippy, treating every warning as
	an error, on the build tool, on the kernel for x86-64, on the
	architecture-independent kernel against the AArch64 stubs of the
	hardware abstraction layer, on the kernel libraries with their
	tests, and on the user programs. CI runs the same command.

*fmt*
	Format all Rust code with rustfmt.

*signoff* [_base_]
	Check that every commit after _base_ carries a *Signed-off-by:* line,
	which certifies the Developer Certificate of Origin (see
	_CONTRIBUTING.md_). Without _base_, the check starts where the current
	branch left _origin/main_. A person signs off their own commits, with
	the email address they are the author under. A commit written by a
	tool, such as an AI assistant or a GitHub App, needs the sign-off of
	the person who submits it, and a tool's own sign-off does not count.
	Merge commits are skipped. CI runs this on the commits of every pull
	request; *git rebase --signoff* _base_ adds missing sign-offs.

*clean* [*--all*]
	Remove _out/_ and the kernel's and the user programs' build
	directories.

	*--all*
		Also remove _target/_, _.cache/_ and _kernel/patched/_, which hold
		the build tool itself, the container's Cargo cache and the
		crates the kernel patches.

*help* [_command_]
	Show the list of commands, or the help for one command.

# PORTS

A port is a directory _ports/<name>/_ (ADR 0069) holding:

_recipe.toml_
	The port's name, version, summary, SPDX license and home page; its
	source, a git repository and the full commit it is pinned to (with
	*paths*, the only directories of a large repository it needs, of
	which nothing else is fetched), a
	release archive (*url*, *sha256* and *mirrors* that serve the same
	file, ADR 0086), or *rust-src = true* for the _library_ directory of
	the pinned Rust
	toolchain's *rust-src* component, with the crates it vendors; any
	*extra* sources unpacked inside its tree, each with its own commit
	and license; crates from crates.io unpacked inside it (*[[crate]]*:
	name, version, the SHA-256 of the _.crate_ file that Cargo.lock
	records too, license, and a *path* other than _<name>-<version>_);
	the ports it *depends* on; whether it belongs to the
	*toolchain*, so that what it installs is only used to build and
	never reaches an image; the files of the sysroot it puts in the boot
	archive (*image*, a path ending in _/_ meaning a whole directory);
	for a source without a LICENSE or COPYING file, the files that hold
	its *notices*; and code from other projects the port keeps in its
	own directory (*bundled*: name, version, license, home page and
	notice files), each a package of its own in the license manifest.

_patches/\*.patch_
	Changes to the source, applied in name order with *patch -p1*. Each
	starts with SPDX lines and a paragraph saying what it changes and
	why.

_build.sh_
	Builds the port in a fresh copy of its patched source and installs
	it into the sysroot, _out/sysroot_, or for a toolchain port into
	_out/toolchain_. It runs with _out/toolchain/bin_ at the front of
	*PATH* and gets in its environment the port's directory, source and
	an empty build directory (*PORT_DIR*, *PORT_SRC*, *PORT_BUILD*), an
	empty directory for what *ob* needs to know of the build
	(*PORT_INFO*: _image_, more files of the sysroot for the image, one a
	line, such as links and manual pages; and for a program in Rust,
	_crates.txt_, the crates it links as *cargo tree -e
	normal,no-proc-macro --prefix none --format '{p}|{l}'* lists them,
	and its _Cargo.lock_), *SYSROOT*, *TOOLCHAIN*, Cargo for Radix
	(*CARGO_RADIX*), *PREFIX* (_/system_), *JOBS*, *HOST*
	(*x86_64-radix*), the toolchain's programs as *CC*, *CXX*, *AR*,
	*RANLIB*, *NM*, *STRIP* and *OBJCOPY*, *CFLAGS* and *CXXFLAGS*, the
	Meson cross files (*CROSS_FILE*, *LIBC_CROSS_FILE*), the CMake
	toolchain file (*CMAKE_TOOLCHAIN_FILE*), *RUST_TARGET* (the target
	specification of programs on Rust's standard library), pkg-config's
	settings for the sysroot and *SOURCE_DATE_EPOCH*; the host's own
	*CPPFLAGS*, *LDFLAGS* and *PKG_CONFIG_PATH* are taken away.

Sources are fetched once into _target/ports/_: with git by commit, and
checked to be that commit; release archives from their *url* or, when
that cannot be reached, from each of their *mirrors* in turn, the first
copy whose SHA-256 is the one pinned; and crates from crates.io, checked
against their SHA-256. An unpacked archive or crate is used again only
while it holds the files, of the same sizes, that *ob* noted when it
unpacked it, and a clone only while it has its commit; else the source
is fetched again. A build uses nothing the recipe does not name. A
source is copied into the build with its files' times, so make never
remakes a release's generated files. Every port's license and notices go into the image's license
manifest.

The toolchain is the host's Clang, LLD and LLVM tools, made to target
Radix by the scripts and Meson cross files *ob* writes into
_out/toolchain/_: _bin/x86_64-radix-cc_, _-ar_, _-nm_ and the rest, and
_meson/radix.ini_ for programs and _meson/radix-libc.ini_ for the C
library itself. The C library is built for no operating system, for
which Clang hands linking to the host's GCC, so _bin/ld.lld_ is there
too: the LLD that GCC finds first on *PATH*. *x86_64-radix-cc* compiles for Radix's ABI, which
is Linux's x86-64 one (see *radix-posix*(7)), with *\_\_radix\_\_* and
*\_\_dukovo\_\_* defined and Linux's own macros not, against the
sysroot's headers only. It links a static position-independent program,
as every program in _/system_ is. With *-dynamic* it links a program
that the dynamic linker, _/system/libraries/ld.so_, starts on the shared C
library, and with *-shared* a shared library, as a bundle that ships
libraries of its own needs (ADR 0071). *x86_64-radix-c++* is the same
for C++, on LLVM's libc++, libc++abi and libunwind, which the
*llvm-runtimes* port builds (ADR 0090): it puts libc++'s headers before
the C library's, unless given *-nostdinc++*, and links the three
libraries before the C library, unless given *-nostdlib++* or
*-nodefaultlibs*.

_cmake/radix.cmake_ is the toolchain file for ports that build with
CMake (3.28 or later): it names the wrappers and the sysroot, in which
alone CMake looks for libraries, headers and packages, and the system
*Radix*, which _cmake/Platform/_ describes to CMake as a Unix whose
compilers and linker take Linux's settings, with _/system_ the first
prefix to look in.

_bin/x86_64-radix-configure_ _source_ [_argument_...] runs the
autoconf *configure* script of _source_ for Radix in the current
directory, with the arguments after it: it puts a script that knows
*x86_64-radix* in front of each _config.sub_ in _source_, gives the
build machine with *--build* and *x86_64-radix* with *--host*, so that
configure always knows it is cross compiling, passes the toolchain's
programs and *CFLAGS*, and sets *CONFIG_SITE* to
_autotools/config.site_, which answers what configure would have to run
a program for Radix to find out, and *CONFIG_SHELL* to _/bin/sh_.

_bin/x86_64-radix-cargo_ is Cargo for programs on Rust's standard
library for Radix (ADR 0080), used as *cargo* is: it builds for the
*x86_64-unknown-radix* target against the rust-std port's sysroot,
links with *x86_64-radix-cc*, and gives the program the crates the
rust-crates port patched for Radix in place of crates.io's, each one
the workspace's Cargo.lock names (all of them when there is no
Cargo.lock yet). Cargo warns of a patch it then finds no use for, such
as one for another version of a crate the lock names.

After building Rust's standard library, *ob* checks its *libc* crate
against the C library: the size and alignment of every structure and
type, the offset and size of every field and the value of every
constant the crate's Radix module declares, as Clang sees them in the
C library's headers, and that the C library defines every function it
declares and declares it with as many parameters, each of the same
kind and size as the crate's, and the same result. Any difference
fails the build.

# ENVIRONMENT

*OB_CONTAINER_ENGINE*
	Program to use for *--container*, such as *podman* or *docker*.

*OB_LINUX_KERNEL*
	The Linux kernel *bench* and *fixture* boot, when *--linux-kernel* is
	not given.

*OB_LINUX_MODULES*
	The directory of that kernel's modules, when they are not in
	_/lib/modules/_ under its release: *ob* loads Btrfs from it, and the
	modules its _modules.dep_ says Btrfs needs.

*OB_OVMF_CODE*, *OB_OVMF_VARS*
	Paths to OVMF's code and variable-store images, when they are installed
	somewhere *ob* does not look.

*OB_TARGET_DIR*
	Directory for Cargo output instead of _target/_.

*SOURCE_DATE_EPOCH*
	Timestamp for files in the image. By default *ob* uses the time of the
	current Git commit.

# FILES

_dukovo.toml_
	Build configuration; see *dukovo.toml*(5).

_rust-toolchain.toml_
	The pinned Rust toolchain.

_image/grub/grub.cfg.in_
	Template for the GRUB menu in every image.

_kernel/targets/x86_64-radix-kernel.json_
	The kernel's Rust target specification.

_out/sysroot/boot/radix_
	The kernel built by *ob build*.

_userspace/targets/x86_64-radix-user.json_
	The user programs' Rust target specification.

_out/sysroot/boot/usertests_
	The test image of user programs built by *ob build*.

_abi/\*.ridl_
	The system call interface, from which *ridlc* generates the Rust
	bindings, the C headers in _out/sysroot/system/include/radix/_ and
	the section 2 man pages.

_ports/_
	The ports (see *PORTS*); their sources are fetched into
	_target/ports/_.

_out/toolchain/_
	The C toolchain's wrapper scripts and Meson cross files, and the
	toolchain ports' files.

_out/sysroot/system/_
	What the ports and the C test programs install: headers in
	_include/_, the C library, its start files and the dynamic linker in
	_lib/_, and the programs the boot archive holds.

_out/sysroot/boot/bootfs_
	The boot archive built by *ob build*.

_kernel/deny.toml_
	The license allow-list of the kernel, used by *ob image* and by
	cargo-deny in CI.

_kernel/patches/_
	The crates from crates.io the kernel builds patched: _crates.toml_,
	which lists each with the SHA-256 of its _.crate_ file and its
	license, and each one's _<name>-<version>.patch_.

_kernel/patched/_
	The patched crates, made by *crates* (not in git).

_LICENSES/_
	The texts of the licenses that files in the repository use, as the
	REUSE specification asks. *ob image* copies the ones the image needs.

_image/licenses/_
	The other license texts that *ob image* puts into the image, named
	_LICENSE-<id>.txt_. Each text is kept in only one of the two
	directories, verbatim from the SPDX license list.

_REUSE.toml_
	Copyright and license information for files that cannot carry an SPDX
	header; see *CONTRIBUTING.md*.

_out/dukovo-VERSION-x86_64.img_
	The boot image built by *ob image*.

_out/dukovo-VERSION-x86_64.spdx_
	The image's license manifest (SPDX 2.3 tag-value).

_out/qemu/radix.gdb_
	The GDB script written by *ob run --gdb*.

_out/test/_
	Test images, serial logs and screenshots from *ob test*.

_out/bench/_
	The benchmark report (_report.md_), its numbers (_results.json_),
	the instruction counts of *--instructions* (_instructions.md_),
	the serial logs of the benchmark boots and the Linux virtual machine
	(_boot-bench.log_, _boot-posix-bench.log_, _boot-linux.log_), the
	POSIX benchmarks built for Linux (_posix-bench_), that machine's
	initramfs (_linux-bench.cpio_) and the disk both benchmark
	(_bench-disk.img_), from *ob bench*.

# EXIT STATUS

*ob* exits with 0 on success and 1 on any failure. Errors are printed with
an *ob: error:* prefix, usually followed by an *ob: hint:* line saying what
to do next.

# EXAMPLES

Prepare a new machine, then boot Dukovo in QEMU:

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
sudo dd if=out/dukovo-0.10.4-x86_64.img of=/dev/sdX bs=4M conv=fsync status=progress
```

Graphical alternatives that write the same image are balenaEtcher, GNOME
Disks ("Restore Disk Image") and Rufus in DD mode.

Build on a host without the tools installed, using Podman or Docker:

```
./ob --container test
```

# SEE ALSO

*dukovo.toml*(5), *dukovo-boot*(7), *radix-debug*(7), *radix-ipc*(7),
*radix-posix*(7), *spark*(8), *qemu-system-x86_64*(1), *grub-mkrescue*(1), *rustup*(1), *gdb*(1), *dd*(1)

The Dukovo handbook, chapter "Building Dukovo with ob", in
_docs/handbook/_ or _out/docs/handbook/index.html_ after *ob docs*.
