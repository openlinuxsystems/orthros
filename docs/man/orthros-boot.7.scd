orthros-boot(7) "OrthrOS" "OrthrOS Manual"

; SPDX-FileCopyrightText: 2026 The OrthrOS contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

orthros-boot - how OrthrOS boots, and the kernel command line

# DESCRIPTION

This page describes the boot path as of Phase 3 of the roadmap: the Radix
kernel moves itself to a random address in the higher half of the address
space, brings up its consoles, builds its page tables, reads the ACPI
tables, hands the machine's memory to its frame allocator and heap,
programs the interrupt controllers and timers, sets up its capability
store, starts every CPU and reports what it found. It then idles; later phases continue from there.
The page grows with every phase that changes how the system starts.

*radix-debug*(7) describes what happens when something goes wrong: the
panic report, the crash log, the kernel debugger and trace points.

## Boot image

*ob image* builds a hybrid image with *grub-mkrescue*(1). It holds GRUB for
legacy BIOS (an El Torito boot record and an MBR boot record) and for UEFI
(an EFI system partition with _EFI/BOOT/BOOTX64.EFI_), so the same file
boots from a USB stick, a hard disk or optical media, on UEFI and BIOS
machines. Its files are:

_/boot/radix_
	The kernel, an ELF executable.

_/boot/grub/grub.cfg_
	The menu, generated from _image/grub/grub.cfg.in_.

_/boot/firmware/intel-ucode.bin_
	Intel microcode updates, only when the image was built with
	*image.microcode* (see *orthros.toml*(5)) or *ob image --microcode*.
	See *MICROCODE* below.

_/system/share/licenses/_
	The license manifest. _orthros.spdx_ lists every component of the
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
. GRUB loads _/boot/radix_ through the Multiboot2 protocol, and the
  microcode file as a module if the image has one. The kernel's header
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
  100 Hz, and the kernel reports the frame allocator's zones.
. The kernel sets up its capability store (see *radix-capabilities*(7))
  and reports its limits on a *Capabilities: ready* line. Until user
  programs arrive in Phase 6, only the *caps* self-test makes
  capabilities.
. The other CPUs listed in the MADT are started one at a time with INIT
  and start-up IPIs. Each loads its own GDT, TSS and IDT, applies
  microcode, enables its local APIC and timer, and idles.
. The kernel prints the CPU list, checks that every CPU takes timer
  interrupts and answers an inter-processor interrupt, runs the self-test
  named by *selftest* if any, and prints *Boot complete.* With the *kdb*
  flag it then waits for debugger commands; otherwise all CPUs idle.

A boot on QEMU with four CPUs ends like this (times in seconds since the
kernel started; messages before timer calibration show 0):

```
[    0.341869] Time: TSC at 2807.482 MHz (measured against the HPET); APIC timer input 1005.301 MHz; tick 100 Hz
[    0.409872] CPUs: 4 of 4 online:
[    0.425233]   cpu0: APIC ID 0, boot CPU, online
[    0.456580]   cpu1: APIC ID 1, online
[    0.485571]   cpu2: APIC ID 2, online
[    0.510992]   cpu3: APIC ID 3, online
[    0.539507] Timer: all 4 CPUs take timer interrupts at 100 Hz (cpu0 9, cpu1 16, cpu2 15, cpu3 14)
[    0.558527] IPI: all 3 other CPUs answered a ping (xAPIC)
[    0.572421] Boot complete.
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
which later phases give to processes. Nothing is mapped at address 0, so a
null pointer faults.

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
RAM, or else at 64 MiB, so it is in the same place on every boot of the
same machine; see *radix-debug*(7). Until the frame allocator starts, a
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
so its layout, may be predictable on that machine. From Phase 5, programs
get random numbers from the same generator through their *getrandom*
capability.

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
*ob* fills it from *boot.cmdline* in _orthros.toml_, or from
*--cmdline* on *ob image* and *ob run*. Flags are separated by spaces and
are either a bare name or _name_=_value_. The kernel warns about unknown
flags and otherwise ignores them.

*loglevel*=_level_
	The lowest level of message shown on the serial port and the screen:
	*error*, *warn*, *info* (the default) or *debug*. The crash log and
	the in-memory log always keep every level.

*kdb*
	Start the kernel debugger on the serial port at the end of boot, at
	every breakpoint, and after a panic. See *radix-debug*(7).

*trace*
	Record trace points from the start of *radix_arch_entry*. See
	*radix-debug*(7).

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

*font*=_size_
	Draw the framebuffer console with the Spleen font of this size:
	*8x16*, *12x24* or *16x32*.

*crashlog*=off
	Do not reserve the crash log, and do not show a previous boot's log.

*selftest*=_name_
	Run a self-test at the end of boot: *page-fault*, *crash-reboot*,
	*breakpoint*, *mm-stress*, *stack-overflow*, *smep*, *smap*, *tlb* or
	*caps*.
	See *radix-debug*(7). *ob test* uses these.

*qemu-exit*
	When boot finishes, or when the kernel panics, leave QEMU through its
	*isa-debug-exit* device at I/O port 0xF4. QEMU then exits with status
	33 on success and 35 after a panic. *ob test* boots its test images
	with this flag. On real hardware the flag has no effect.

# MICROCODE

Processors run microcode that the vendor fixes with updates. Firmware
applies some; an operating system applies newer ones as early as possible.
The updates are closed firmware, so OrthrOS never includes them itself:
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

# REAL HARDWARE

The reference machine is a Dell Inspiron 15 5570. Before booting OrthrOS on
it, set *SATA Operation* to *AHCI* and turn *Secure Boot* off in the
firmware setup (F2), then choose the USB stick in the one-time boot menu
(F12). It has no serial port: the boot log appears on the screen under
UEFI and under legacy BIOS boot, and after a crash the next boot shows the
end of the previous one. The handbook chapter "Supported hardware" lists
the steps of a smoke test and its results.

# SEE ALSO

*radix-debug*(7), *radix-capabilities*(7), *ob*(1), *orthros.toml*(5), *grub-mkrescue*(1)

The Multiboot2 specification:
https://www.gnu.org/software/grub/manual/multiboot2/

The ACPI specification, version 6.5: https://uefi.org/specifications
