dukovo-boot(7) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

dukovo-boot - how Dukovo boots, and the kernel command line

# DESCRIPTION

This page describes the boot path as of Phase 7 of the roadmap: the Radix
kernel moves itself to a random address in the higher half of the address
space, brings up its consoles, builds its page tables, reads the ACPI
tables, hands the machine's memory to its frame allocator and heap,
programs the interrupt controllers and timers, sets up its capability
store, user mode and the root of the file tree, starts every CPU, finds
the disks on PCI, reports what it found, and starts the first program,
Spark. The page grows with every phase that changes how the system
starts.

*radix-debug*(7) describes what happens when something goes wrong: the
panic report, the crash log, the kernel debugger and trace points.

## Boot image

*ob image* builds a hybrid image with *grub-mkrescue*(1). It holds GRUB for
legacy BIOS (an El Torito boot record and an MBR boot record) and for UEFI
(an EFI system partition with _EFI/BOOT/BOOTX64.EFI_), so the same file
boots from a USB stick, a hard disk or optical media, on UEFI and BIOS
machines. After those partitions an image for a disk has a fifth, the
Dukovo root (GPT type 90C9A303-3954-4174-8F8B-EE75A92A9EB0), whose
Btrfs holds the system and what changes (*spark*(8)). The files GRUB
reads are:

_/boot/radix_
	The kernel, an ELF executable, without its debug information: GRUB
	loads every section of the file into memory, and the debug
	information would be most of it. Backtraces need only the kernel's
	own symbol table; *ob*(1) keeps the debug information in
	_out/sysroot/boot/radix_, for *gdb*(1).

_/boot/grub/grub.cfg_
	The menu, generated from _image/grub/grub.cfg.in_.

_/boot/firmware/intel-ucode.bin_
	Intel microcode updates, only when the image was built with
	*image.microcode* (see *dukovo.toml*(5)) or *ob image --microcode*.
	See *MICROCODE* below.

_/system/resources/licenses/_
	The license manifest. _dukovo.spdx_ lists every component of the
	image (the kernel's crates, the Rust standard library crates compiled
	into it, GRUB, and the microcode if any) with its SPDX license, in
	the SPDX 2.3 tag-value format; _README_ summarises it, _texts/_ holds
	the text of each license, and _notices/_ the copyright and license
	files that came with each component, such as the Spleen font
	license. See *ob*(1).

## Sequence

. The firmware loads GRUB from the boot medium.
. GRUB shows its menu for *boot.timeout* seconds, on the screen and on the
  first serial port.
. GRUB loads _/boot/radix_ through the Multiboot2 protocol, the
  microcode file as a module if the image has one, the boot archive
  (_/boot/bootfs_, the programs the system starts with) as a module with
  the word *bootfs*, and the self-tests' programs (_/boot/usertests_) as
  a module with the word *usertests* if the image has them. The kernel's
  header
  asks for a linear framebuffer of any size with 32 bits per pixel. Under
  UEFI, GRUB keeps the firmware's graphics mode (*gfxpayload=keep*), which
  is normally the screen's native resolution; under BIOS it sets a VESA
  mode. Under UEFI it then exits the firmware's boot services.
. GRUB jumps to *\_start*, the kernel's 32-bit entry point at physical
  address 1 MiB, in protected mode with paging off, with the Multiboot2
  magic value in EAX and the address of the boot information in EBX.
. The trampoline checks that the CPU supports 64-bit long mode (it halts
  silently if not), maps the first 4 GiB of physical memory with 2 MiB
  pages at 0 and at 0xFFFF800000000000, and the first GiB at the kernel's
  link address, and enables long mode. It seeds the random number
  generator (see *RANDOM NUMBERS* below). Unless the *nokaslr* flag is
  given it then moves the kernel (see *KASLR* below): it picks a random slide,
  maps the image a second time that much higher, and adds the slide to
  every absolute address in the image, using the relocation table *ob
  build* wrote into it. It calls *radix_arch_entry* at the kernel's new
  address.
. *radix_arch_entry*, the first Rust function, belongs to the x86-64
  layer. It starts the serial port, loads the boot CPU's GDT, TSS and IDT
  (so every exception from here on gives a panic report), checks the magic
  value and parses the boot information. It starts recording trace points
  if the *trace* flag is given, picks random places for the direct map
  and the mapping window, sets aside the memory the kernel must not
  reuse, applies early microcode, then replaces the trampoline's page
  tables with the kernel's own (see *Address space* below), checks that
  no page is both writable and executable, and calls
  *radix_kernel_main*.
. *radix_kernel_main* is architecture-independent. It starts the
  framebuffer console, which replays everything logged so far, and the
  crash log, then reports the version, boot loader, firmware, command
  line, memory map, the kernel's reservations and the address-space
  layout. If memory held the log of a previous boot, it shows how that
  boot ended and its last lines.
. The x86-64 layer then identifies the CPU, reads the ACPI tables (RSDP,
  RSDT or XSDT, MADT, FADT, HPET, MCFG, and the SRAT and SLIT that
  describe NUMA nodes), masks the legacy 8259 PICs, enables the local
  APIC in x2APIC mode where available (xAPIC otherwise), and masks every
  I/O APIC input. It hands the free RAM to the frame allocator (see
  *MEMORY* below), from which the kernel heap and everything after it
  allocate. It calibrates the time-stamp counter and the local APIC
  timer against the HPET, the ACPI PM timer or the 8254 PIT, in that
  order of preference. The periodic timer interrupt then runs at
  100 Hz, and the kernel reports the frame allocator's zones. Last,
  before any driver starts a device, it takes over the IOMMU's DMA
  remapping units that the ACPI DMAR table lists, or the AMD IOMMUs the
  IVRS table lists (see *IOMMU* below).
. The kernel sets up the root resource domain, *system*, and the
  scheduler: from here on the boot code runs as the thread *kmain*, and
  the *Scheduler:* line names the classes, the timer, how idle CPUs wait
  and which extended processor state every switch saves. The *Lock
  checker:* line says whether the lock checker runs (see *lockdep*
  below). The kernel then turns interrupts on.
. The kernel sets up its capability store (see *radix-capabilities*(7))
  and reports its limits on a *Capabilities: ready* line.
. The kernel sets up user mode: the *User mode:* line gives the system
  call entry, the user half of every address space and whether SMEP and
  SMAP protect the kernel from it, and the *User:* line the size of the
  *usertests* image, if GRUB loaded one, which only the self-tests start
  programs from (see *radix-debug*(7)).
. The kernel makes the root of the file tree, an empty tmpfs, which Spark
  builds the directory hierarchy on (*Files:* line; see
  *radix-files*(7)).
. The other CPUs listed in the MADT are started one at a time with INIT
  and start-up IPIs. Each loads its own GDT, TSS and IDT, applies
  microcode, enables its local APIC and timer, and idles.
. The kernel prints the CPU list. Every CPU then runs threads: each gets
  its run queue and its idle thread, and the kernel starts its own
  threads, one *work/*_N_ worker per CPU, the *reaper* that frees the
  stacks of threads that have exited, the out-of-memory *supervisor*
  (see *radix-scheduling*(7)) and the *writeback* thread that syncs the
  filesystems every five seconds (see *radix-files*(7)).
. The kernel walks PCI through the windows of the ACPI MCFG table, logs
  every function it finds on a *PCI:* line, with its vendor and device
  IDs and class, and sets up each disk: a *Disk:* line names it (_vda_
  for a virtio block device, _sda_ for a SATA disk, _nvme0n1_ for an NVMe
  namespace, ...) and its size, and the next lines its partition table
  and partitions. The functions are listed in
  _/kernel/bus/pci/devices_ (*radix-sysfs*(5)).
. It checks that every CPU takes timer
  interrupts and answers an inter-processor interrupt, runs the self-test
  named by *selftest* if any, and prints *Boot complete.*
. Unless a self-test ran, the kernel starts the first program: it checks
  the boot archive, finds the program *init* names in it
  (_/system/commands/spark_ by default) and starts it in a process of its
  own, with the words after *--* on the command line as its arguments.
  The *Init:* lines name the program and its process. That program,
  Spark, starts every other one, and the kernel stops if it ends; see
  *spark*(8) and *radix-processes*(7).
. With the *kdb* flag the kernel then waits for debugger commands;
  otherwise *kmain* exits and the CPUs idle until a thread has work.

A boot on QEMU with four CPUs and one virtio disk ends like this (times in
seconds since the kernel started; messages before timer calibration show
0):

```
[    0.254500] Time: TSC at 2807.603 MHz (measured against the HPET); APIC timer input 1004.640 MHz; tick 100 Hz
[    0.383220] Scheduler: EEVDF fair class (3 ms slices) and FIFO and round-robin real-time classes; timer APIC one-shot, idle with HLT; extended state XSAVEOPT on every switch: x87, SSE, AVX, PKRU; 2696-byte areas
[    0.419834] Lock checker: off (the lockdep flag turns it on)
[    0.445393] Capabilities: ready; 18 object kinds, 12 rights, spaces of up to 65536 slots, derivations up to 64 deep
[    0.469304] User mode: SYSCALL/SYSRET entry, user half 0x10000-0x7ffffffff000; SMEP on, SMAP on (lifted for user copies only)
[    0.494239] Files: the root is a tmpfs; Spark mounts the rest
[    0.520196] User: the usertests image is 183 KiB
[    0.613107] CPUs: 4 of 4 online:
[    0.634020]   cpu0: APIC ID 0, boot CPU, online
[    0.655178]   cpu1: APIC ID 1, online
[    0.676009]   cpu2: APIC ID 2, online
[    0.696883]   cpu3: APIC ID 3, online
[    0.720307] Scheduler: 4 CPUs run threads, on 1 NUMA node
[    0.745062] PCI: 0000:00:00.0 8086:29c0 class 060000: host bridge
[    0.769244] PCI: 0000:00:01.0 1234:1111 class 030000: display controller
[    0.792540] PCI: 0000:00:02.0 1b36:000d class 0c0330: USB controller
[    0.828349] PCI: 0000:00:03.0 1af4:1001 class 010000: SCSI controller, virtio
[    0.864507] Disk: vda is a virtio block device of 32 MiB at 0000:00:03.0, 512 bytes a block, write cache, MSI-X vector 0x50
[    0.893952] Disk: vda has no partition table; it is used whole
[    0.916262] PCI: 0000:00:1f.0 8086:2918 class 060100: ISA bridge
[    0.941169] PCI: 0000:00:1f.2 8086:2922 class 010601: SATA controller
[    0.964452] PCI: 0000:00:1f.3 8086:2930 class 0c0500: SMBus controller
[    0.985756] PCI: 7 functions on 1 segment; 1 virtio disk
[    1.006700] Timer: all 4 CPUs take timer interrupts at 100 Hz (cpu0 27, cpu1 44, cpu2 42, cpu3 41)
[    1.028489] IPI: all 3 other CPUs answered a ping (xAPIC)
[    1.049978] Boot complete.
[    1.073162] Init: the boot archive is 206 KiB with 9 entries; starting /system/commands/spark
[    1.150677] Init: /system/commands/spark is process 1
```

## Address space

The kernel's page tables map, in the higher half:

- the kernel image at 0xFFFFFFFF80000000 plus its physical address plus
  the KASLR slide, with 4 KiB pages: text read-only and executable,
  read-only data and the symbol and relocation tables read-only, and
  data writable but not executable;
- every RAM range of the memory map in the direct map, at a random 1 GiB
  boundary between 0xFFFF800000000000 and 0xFFFFC00000000000, plus the
  physical address: writable but not executable, in 1 GiB pages where
  the CPU and the range allow, 2 MiB pages next and 4 KiB pages at the
  edges. The kernel image's own text and read-only data are read-only
  there too;
- the mapping window, 512 GiB at a random 1 GiB boundary between
  0xFFFFC00000000000 and 0xFFFFE00000000000, where device registers, the
  framebuffer and the CPUs' stacks are mapped on demand, each with an
  unmapped guard page below it.

The lower half, below 0x0000800000000000, belongs to address spaces,
which hold programs from Phase 5 on; a program may use it from 0x10000 to
one page below its end (see *radix-ipc*(7)). Nothing is mapped at address
0, so a null pointer faults.

No page is ever writable and executable at once (W^X); the kernel checks
this by walking its finished page tables and reports it at boot. Where
the CPU has them, SMEP stops the kernel from running code in a user page,
and SMAP from reading or writing one except where it says it means to;
PCIDs tag TLB entries by address space, so switching between them does
not flush the TLB (the *nopcid* flag turns that off). When the kernel
removes a mapping that other CPUs may have cached, it sends them an
inter-processor interrupt and waits until each has flushed it (a TLB
shootdown).

Each CPU's stack has an unmapped guard page below it, and double faults,
NMIs and machine checks run on separate stacks, so a stack overflow still
produces a panic report, which says that the stack overflowed.

A boot reports the layout and the protections like this:

```
KASLR: kernel moved by 0x20200000 (one of 509 places, 2612 addresses patched); direct map one of 65533 places, mapping window one of 32257
Layout: kernel at 0xffffffffa0300000, direct map at 0xffff962340000000, mapping window at 0xffffd0de40000000
Paging: W^X holds: none of 1025 MiB of kernel mappings is writable and executable
Paging: SMEP on, SMAP on, PCID on; direct map in 1 GiB pages
```

## Reserved memory

Before it allocates anything the kernel sets aside, and lists at boot: the
first MiB (firmware data and the page that starts the other CPUs), the
kernel image, the boot information, boot modules, and the 1 MiB crash log.
The crash log goes at the first 2 MiB boundary above 4 GiB where there is
RAM, or else at the first one from 64 MiB, clear of the kernel image and
the modules, so it is in the same place on every boot of the same machine;
see *radix-debug*(7). Until the frame allocator starts, a
simple allocator hands out frames for page tables and stacks; those stay
allocated, and the rest of RAM goes to the frame allocator.

# KASLR

Kernel address space layout randomization places the kernel at a
different address on every boot, so that an attacker who finds a bug
cannot rely on knowing where kernel code and data are. Three things move:

- the kernel image, by a whole number of 2 MiB pages within the top GiB
  of the address space (about 500 places);
- the direct map, on a 1 GiB boundary (about 65,000 places);
- the mapping window, on a 1 GiB boundary (about 32,000 places).

The random numbers come from the kernel's random number generator (see
*RANDOM NUMBERS* below), which is seeded before the kernel moves. To move
the image, *ob build* links the kernel with its
relocation records kept, lists every absolute address that points into
the image in the image's _.krelocs_ section (format in the
*radix-relocs* crate), and removes the records, which GRUB refuses to
load. The trampoline adds the slide to each listed address before the
kernel runs. A kernel built without *ob* has an empty table and stays
where it was linked, and says so.

The boot log shows the layout (see *Address space* above), and kernel
addresses in panic reports and *kdb* are run-time addresses; symbol
lookups subtract the slide (*radix-debug*(7)). The *nokaslr* flag keeps
everything at fixed addresses: the image where it was linked, the direct
map at 0xFFFF800000000000 and the mapping window at 0xFFFFC00000000000.
*ob run --gdb* adds it, so that GDB's symbols match.

# RANDOM NUMBERS

The kernel has one cryptographically secure random number generator,
ChaCha20 with fast key erasure: each request replaces the key, so the
numbers it already gave out cannot be worked out from the generator's
state afterwards. It is seeded first thing at boot, before KASLR picks
the kernel's address, from every source the machine has: on x86-64, 256
bits from the CPU's *RDSEED* instruction, 256 bits from *RDRAND*, and
4096 readings of the time-stamp counter around a small piece of work,
whose timing varies (jitter). All of it is hashed together with BLAKE2s,
so the seed is unpredictable if any one source is; no source is trusted
alone.

Each source also gets a health test: a hardware word that is zero, all
ones or a repeat fails it, and so does timing that changes too rarely.
A source that fails is still mixed in, but the boot log stops counting on
it. The boot log names the sources the seed rests on:

```
Random: ChaCha20 seeded before KASLR from RDSEED, RDRAND and timing jitter (4096 samples, 37 stuck)
```

In a virtual machine the time-stamp counter is the host's, and on some
hosts the timing jitter fails its test while *RDSEED* and *RDRAND* pass.
The line then says so, and the seed is as unpredictable as the CPU's
generators.

A line that says no source passed means the kernel's random numbers, and
so its layout, may be predictable on that machine. Programs get random
numbers from the same generator through the random capability every one
of them receives (see *radix-random*(2)).

# MEMORY

Physical memory is managed in 4 KiB frames by a buddy allocator, which
hands out blocks of 1 to 1024 contiguous frames (4 KiB to 4 MiB). Every
frame has a 16-byte record, about 0.4% of RAM, which the boot log
reports. RAM is divided into zones: each NUMA node has a *DMA32* zone,
its RAM below 4 GiB for devices that can only address 32 bits, and a
*Normal* zone for the rest. The ACPI SRAT says which node each range of
RAM and each CPU belongs to, and the SLIT how far apart the nodes are;
machines without them have one node. An allocation takes memory from the
calling CPU's node first and falls back to the nearest other nodes.

Every zone keeps a small reserve, between 64 KiB and 4 MiB, that only
allocations made where the kernel cannot wait (interrupt handlers, code
holding a spin lock) may take. On machines with memory above 4 GiB,
allocations that can use it leave part of each DMA32 zone for the devices
that need it.

The kernel heap serves smaller objects from 32 size classes, 16 bytes to
8 KiB, each a cache of slabs with a per-CPU cache of free objects in
front, so most allocations never take a shared lock. Larger requests get
frames of their own. Every allocation in the kernel says whether it may
wait and which memory it needs, and running out of memory is an error the
caller handles, never a panic. Freed objects are filled with a pattern
that is checked when they are handed out again, so a write to freed
memory is caught and reported as heap corruption (a panic).

The boot log reports the frame allocator's zones:

```
Memory: frame allocator took 7 regions on 2 NUMA nodes, 1009 MiB free; 4068 KiB of frame records
Memory: 1009 MiB free of 1009 MiB in the frame allocator's zones:
  node 0 DMA32  0x000000100000-0x00001fffffff   502 MiB managed,   502 MiB free, 4020 KiB reserve
  node 1 DMA32  0x000020000000-0x00003fedbfff   507 MiB managed,   507 MiB free, 4056 KiB reserve
```

*kdb*'s *mem* and *heap* commands show the same at any time
(*radix-debug*(7)).

# CONSOLES

The kernel writes every message to all of these at once:

*Serial port*
	COM1, I/O port 0x3F8, 115200 baud, 8 data bits, no parity, 1 stop bit,
	when the port exists. The kernel debugger also runs here.

*Framebuffer*
	The linear framebuffer GRUB set up, under UEFI and BIOS. Text is drawn
	with one of the built-in Spleen fonts (8x16, 12x24 or 16x32 pixels):
	by default the largest that still gives at least 100 columns and 40
	rows, so 12x24 on a 1920x1080 screen. The *font* flag chooses one.
	When the screen is full the text scrolls. Messages are coloured by
	level. On machines without a serial port this is the only console, so
	it starts first and replays the messages logged before it.

*VGA text screen*
	Used instead of the framebuffer when GRUB could not set a graphics
	mode and left the BIOS text mode.

*Crash log*
	A copy of everything, debug messages included, kept in reserved
	memory for the next boot; see *radix-debug*(7).

Each line starts with the time since the kernel started. The consoles show
messages of level info and above unless *loglevel* says otherwise.

# KERNEL COMMAND LINE

GRUB passes everything after the kernel path on the *multiboot2* line.
*ob* fills it from *boot.cmdline* in _dukovo.toml_, or from
*--cmdline* on *ob image* and *ob run*. Flags are separated by spaces and
are either a bare name or _name_=_value_. The kernel warns about unknown
flags and otherwise ignores them. A lone *--* ends the kernel's flags:
the words after it are the first program's arguments, as Linux passes
them to *init*, and the kernel does not read them.

*init*=_path_
	Start the program at _path_ in the boot archive as the first
	program, instead of _/system/commands/spark_. See *spark*(8).

*--* _arguments_...
	Pass _arguments_ to the first program, after its path; at most 32
	are passed. For example *-- /system/tests/hello one two* has Spark
	start _/system/tests/hello_ with the arguments *one* and *two*.

*loglevel*=_level_
	The lowest level of message shown on the serial port and the screen:
	*error*, *warn*, *info* (the default) or *debug*. The crash log and
	the in-memory log always keep every level.

*kdb*
	Start the kernel debugger on the debug console (the serial port, and
	the Debug Capability with *dbc*) at the end of boot, at every
	breakpoint, and after a panic. See *radix-debug*(7).

*dbc*
	Put the console on the Debug Capability of the first USB controller
	that has one, for a second computer joined to one of its USB 3 ports
	with a debug cable: the log, the panic report and *kdb* go there too.
	See *USB* and *radix-debug*(7).

*trace*
	Record trace points from the start of *radix_arch_entry*. See
	*radix-debug*(7).

*lockdep*[=on|off]
	Turn the lock checker on (*lockdep* or *lockdep=on*) or off
	(*lockdep=off*). It watches every spin lock and mutex and reports
	locks taken in orders that can deadlock, locks an interrupt handler
	shares with code that runs with interrupts on, and sleeping where
	that is not allowed. It is on by default in debug builds and off in
	release builds, since it adds work to every lock taken. See
	*radix-debug*(7) and *radix-scheduling*(7).

*panic*=_seconds_
	After a panic, restart the machine after this many seconds instead of
	waiting for a key press. *panic=0* restarts at once.

*nosmp*
	Start only the boot CPU.

*nokaslr*
	Keep the kernel, the direct map and the mapping window at fixed
	addresses instead of random ones (see *KASLR*). For debugging; *ob
	run --gdb* adds it.

*nopcid*
	Do not tag TLB entries with process-context identifiers (PCIDs), even
	when the CPU has them: every switch between address spaces then
	flushes the TLB. For debugging and for comparison.

*nox2apic*
	Drive the local APICs in xAPIC mode (registers in memory) even when
	the CPUs support x2APIC mode (registers as MSRs). Ignored, with a
	warning, when the firmware already switched to x2APIC mode, because
	the hardware cannot switch back without a reset.

*mce*=off
	Do not turn machine-check reporting on: the CPUs' banks are not
	read, and a hardware error that would raise a machine check shuts
	the CPU down instead. See *MACHINE CHECKS*.

*mce-poll*=_seconds_
	Read the CPUs' machine-check banks this often for errors the
	hardware corrected, instead of every 300 seconds. *ob test* uses 1.

*usb*=off
	Leave the USB controllers as the firmware left them: none is reset
	or driven, and no USB device is found. See *USB*.

*iommu*=off
	Leave the IOMMU's DMA remapping off, so that every device reaches
	all memory, as before Phase 11d. See *IOMMU*.

*font*=_size_
	Draw the framebuffer console with the Spleen font of this size:
	*8x16*, *12x24* or *16x32*.

*crashlog*=off
	Do not reserve the crash log, and do not show a previous boot's log.

*selftest*=_name_
	Run a self-test at the end of boot: *page-fault*, *crash-reboot*,
	*breakpoint*, *mm-stress*, *stack-overflow*, *smep*, *smap*, *tlb*,
	*caps*, *fpu*, *sched-fair*, *sched-rt*, *sched-pi*, *sched-balance*,
	*lockdep*, *rcu*, *oom*, *user*, *ipc*, *fuzz* or *bench*.
	See *radix-debug*(7). *ob test* uses these. The first program does
	not start after a self-test, unless *init-after-selftest* is given.

*init-after-selftest*
	Start the first program after the self-test of *selftest*, as after a
	boot without one. It is not process 1 then, but takes the next number
	after the test's programs. The boot entry of the disk check
	(*ob image --disk-check*) runs *selftest=bench* this way before its
	program.

*fuzz-seed*=_number_
	Give the programs of *selftest=fuzz* the seeds from _number_ on, to
	repeat a run whose log named that first seed. Random by default.

*qemu-exit*
	When boot finishes, or when the kernel panics, leave QEMU through its
	*isa-debug-exit* device at I/O port 0xF4. QEMU then exits with status
	33 on success and 35 after a panic. *ob test* boots its test images
	with this flag. On real hardware the flag has no effect.

# MICROCODE

Processors run microcode that the vendor fixes with updates. Firmware
applies some; an operating system applies newer ones as early as possible.
The updates are closed firmware, so Dukovo never includes them itself:
the builder adds a file of them to an image with *image.microcode* or *ob
image --microcode*, naming a file or a directory of Intel updates such as
_/lib/firmware/intel-ucode_ from the *intel-microcode* package. *ob*
checks every update and fails if it finds none.

GRUB loads the file as a module with the word *microcode* on its command
line. Each CPU, the boot CPU before it builds the kernel's page tables and
every other CPU as it starts, picks the newest update for its signature and
platform and writes it to the processor if it is newer than the running
revision. The boot CPU logs the revision before and after, or why nothing
was loaded; the count of other CPUs that loaded the update follows the CPU
list. Hyper-threads share microcode, so the second thread of a core finds
the update already applied. AMD's format is not supported yet, and the
module is ignored on AMD processors.

# MACHINE CHECKS

x86-64 processors report hardware errors (in their caches, TLBs, buses
and the memory they read) in machine-check banks, each holding the last
error it saw. Each CPU turns reporting on for every bank as it starts,
unless *mce=off*; the log says how many banks there are, or that the CPU
has none. At the end of boot the kernel reads every CPU's banks, which
finds what an earlier boot left there (the banks keep their errors across
a restart), and again every 300 seconds (*mce-poll*), logging each error
with its CPU and bank: a warning for one the hardware corrected, an error
for one it did not but the CPU went on after. For example:

	Machine check: CPU 1 bank 2: corrected data read error in the level 0
	data cache, address 0x12345000 (status 0x9c00000000000134, misc 0x86)

An error the CPU cannot go on after raises a machine check, and the kernel
panics with the bank's error: see *radix-debug*(7). Linux decodes the
same status with *mcelog* or *rasdaemon*. Memory error counts by DIMM and
error recovery that ends only the program that read bad memory come later
(ADR 0145).

# IOMMU

An Intel VT-d unit sits between a group of PCI devices and memory and
translates every address they read or write by DMA. At the end of the
x86-64 layer's start, unless *iommu=off*, the kernel takes over each
unit the ACPI DMAR table lists: it switches off what the firmware left
on, gives the unit empty tables and turns translation on (ADR 0164). From
then on a device reaches no memory but what its driver gave it: its DMA
memory, the buffers of its requests in flight, which are taken back
when the device is done, and what the firmware reserved for it (RMRRs).
Each unit says what it translates and how:

	IOMMU: VT-d 1.0 unit 0 at 0xfed90000 translates 10 devices: 3-level page tables, 65536 domains, queued invalidation

A unit that translates every device on its PCI segment says so instead
of a count. A unit that translates only the display controller is left
off, which the log says, as no graphics driver hands it memory yet.
Each device gets a domain of its own the first time its driver asks for
DMA memory, and the kernel logs it:

	IOMMU: 0000:00:04.0 reaches only what is mapped for it (domain 2)

A device that reaches for memory it was not given is stopped, and the
unit records a fault; the kernel logs at most 10 of them in each 5 s:

	IOMMU: 0000:00:03.0 was stopped writing to 0x3f89e000: the page is not mapped for it to write

On AMD machines the IOMMU (AMD-Vi) does the same (ADR 0165). It is a PCI
function of its own, which the ACPI IVRS table names along with the
device IDs it translates and the memory the firmware keeps for devices
(IVMD blocks). The kernel takes each one over, every device blocked
until its domain is made, maps each IVMD block's memory for the devices
it names, and turns translation on:

	IOMMU: AMD-Vi unit 0 at 0xfed80000 translates 11 device IDs: 4-level page tables, 65536 domains, command buffer invalidation, caching absent entries

A device whose requests carry another's ID, as behind a PCIe-to-PCI
bridge, shares that device's domain, and the log says so. The IOMMU logs
a stopped access in its event log, whose interrupt the kernel points at
a handler of its own, and the kernel logs it as above.

Interrupt remapping, and VT-d devices behind a PCIe-to-PCI bridge, come
later in Phase 11d. Devices behind a unit left off, and every device
with *iommu=off*, reach all memory. The *iommu* and *iommu-amd* boot
cases of *ob*(1) test it under QEMU's VT-d unit and AMD IOMMU.

# USB

The PCI scan hands each xHCI controller to the USB driver, unless
*usb=off*, which gives it a kernel thread, _usb1_, _usb2_ and on. The
thread takes the controller from the firmware, resets and starts it, and
logs what it is, its ports (USB 3 and USB 2 ones apart) and its interrupt:

	USB: bus 1 is the xHCI 1.0 controller at 0000:00:02.0: 8 ports (USB 3
	on 1-4, USB 2 on 5-8), 64 device slots, MSI-X vector 0x50

Each device plugged into a root port or a hub, at boot or later, is
reset, addressed and named with its bus, its root port and the hub ports
it is behind, as Linux names them: its product, vendor and product IDs,
maker, class and speed. It is then configured, with the endpoints of its
first configuration. A hub says how many ports it has, and the devices
on them follow. Unplugging a device is logged, and unplugging a hub lets
go of everything behind it:

	USB: 1-6: QEMU USB Keyboard (0627:0001, QEMU), keyboard, high speed
	USB: 1-7: QEMU USB Hub (0409:55aa, QEMU), hub, full speed
	USB: 1-7: a hub of 8 ports
	USB: 1-7.1: QEMU USB Mouse (0627:0001, QEMU), mouse, full speed
	USB: 1-7: QEMU USB Hub was unplugged
	USB: 1-7.1: QEMU USB Mouse was unplugged

A device pulled out while it is being set up is logged as unplugged too,
once it has been named. A device that cannot be set up twice is left
alone until it is unplugged.
Hub ports past 15 and hubs more than five deep are not used. Before the
machine is powered off or restarted, every controller is halted.
Keyboards, mice and tablets are read through their HID reports (ADR
0148).

A USB stick, card reader or USB disk speaking the Bulk-Only Transport
gets a kernel thread, _usb-storage_, which finds the disks behind it and
serves them (ADR 0161). Each disk takes the lowest _sd_ letter free after
the SATA disks', and is logged with where it is, its SCSI vendor,
product and revision, its block size, and whether it is removable,
write-protected or has a write cache; its partitions follow, as for any
disk (*radix-files*(7)). The boot waits up to 5 s, before Spark starts,
for the devices plugged in at boot to be set up, so that their disks
take letters in the order they were found and Spark mounts them.
Unplugged, a stick's disks go:

	USB: 1-1: QEMU USB HARDDRIVE (46f4:0001, QEMU), mass storage, SuperSpeed
	USB: 1-2: QEMU USB HARDDRIVE (46f4:0001, QEMU), mass storage, SuperSpeed
	Disk: sda is a USB disk of 151.3 MiB on 1-1: QEMU QEMU HARDDISK 2.5+, 512 bytes a block, removable, write cache
	Disk: sda has a GPT with 4 partitions
	Disk: sdb is a USB disk of 1.4 MiB on 1-2: QEMU QEMU HARDDISK 2.5+, 512 bytes a block, write cache
	Disk: sdb has no partition table; it is used whole
	USB: 1-2: QEMU USB HARDDRIVE was unplugged
	Disk: sdb is gone with its USB device

A USB 3 disk that speaks USB Attached SCSI (UAS) is driven over it, many
commands at once, when it is at SuperSpeed on a controller with streams,
or below SuperSpeed (ADR 0162). Its thread finds its logical units with
REPORT LUNS; its disks' lines end in _USB Attached SCSI_, and when it has
more than one unit each is named after its place:

	Disk: sdb is a USB disk of 1.4 MiB on 1-2 LUN 0: QEMU QEMU HARDDISK 2.5+, 512 bytes a block, write cache, USB Attached SCSI
	Disk: sdc is a USB disk of 32.0 MiB on 1-2 LUN 1: QEMU QEMU HARDDISK 2.5+, 512 bytes a block, write cache, USB Attached SCSI

A device that speaks both is driven over the Bulk-Only Transport when its
controller has no streams, which the log says; one that speaks UAS alone
is then not used. A UAS device that does not answer a request in 30 s,
or breaks the protocol, is reset, and its requests in flight go again:

	Disk: USB 1-2: it did not answer in time; it is reset

With the *dbc* flag the first controller with a Debug Capability turns
it on, and the console goes there as well (ADR 0160); each controller
says what it did with its own:

	USB: bus 1 has no Debug Capability, so the console is not on it
	USB: bus 2: the Debug Capability is on, waiting for a debug host
	USB: bus 2: a debug host is connected
	USB: bus 2: the debug host on port 3 reads the console

See *DEBUG CAPABILITY CONSOLE* in *radix-debug*(7).

# REAL HARDWARE

The reference machine is a Dell Inspiron 15 5570. Before booting Dukovo on
it, set *SATA Operation* to *AHCI* and turn *Secure Boot* off in the
firmware setup (F2), then choose the USB stick in the one-time boot menu
(F12). It has no serial port: the boot log appears on the screen under
UEFI and under legacy BIOS boot, and after a crash the next boot shows the
end of the previous one. The handbook chapter "Supported hardware" lists
the steps of a smoke test and its results.

# SEE ALSO

*spark*(8), *radix-processes*(7), *radix-debug*(7), *radix-capabilities*(7), *radix-scheduling*(7), *ob*(1), *dukovo.toml*(5), *grub-mkrescue*(1)

The Multiboot2 specification:
https://www.gnu.org/software/grub/manual/multiboot2/

The ACPI specification, version 6.5: https://uefi.org/specifications

The Intel Virtualization Technology for Directed I/O Architecture
Specification, revision 4.1:
https://www.intel.com/content/www/us/en/content-details/774206/intel-virtualization-technology-for-directed-i-o-architecture-specification.html
and the AMD I/O Virtualization Technology (IOMMU) Specification, 48882,
revision 3.10: https://www.amd.com/content/dam/amd/en/documents/processor-tech-docs/specifications/48882_IOMMU.pdf
