orthros-boot(7) "OrthrOS" "OrthrOS Manual"

# NAME

orthros-boot - how OrthrOS boots, and the kernel command line

# DESCRIPTION

This page describes the boot path as of Phase 1 of the roadmap: the Radix
kernel starts in the higher half of the address space, brings up its
consoles, reads the ACPI tables, programs the interrupt controllers and
timers, starts every CPU and reports what it found. It then idles; later
phases continue from there. The page grows with every phase that changes
how the system starts.

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
  silently if not), maps the first 4 GiB of physical memory three times
  with 2 MiB pages (at 0, at the direct map and, for the first GiB, at the
  kernel's link address), enables long mode and calls *radix_arch_entry*
  in the higher half.
. *radix_arch_entry*, the first Rust function, belongs to the x86-64
  layer. It starts the serial port, loads the boot CPU's GDT, TSS and IDT
  (so every exception from here on gives a panic report), checks the magic
  value and parses the boot information. It starts recording trace points
  if the *trace* flag is given, sets aside the memory the kernel must not
  reuse, applies early microcode, then replaces the trampoline's page
  tables with the kernel's own (see *Address space* below) and calls
  *radix_kernel_main*.
. *radix_kernel_main* is architecture-independent. It starts the
  framebuffer console, which replays everything logged so far, and the
  crash log, then reports the version, boot loader, firmware, command
  line, memory map and the kernel's reservations. If memory held the log
  of a previous boot, it shows how that boot ended and its last lines.
. The x86-64 layer then identifies the CPU, reads the ACPI tables (RSDP,
  RSDT or XSDT, MADT, FADT, HPET and MCFG), masks the legacy 8259 PICs,
  enables the local APIC in x2APIC mode where available (xAPIC
  otherwise), masks every I/O APIC input, and calibrates the time-stamp
  counter and the local APIC timer against the HPET, the ACPI PM timer or
  the 8254 PIT, in that order of preference. The periodic timer
  interrupt then runs at 100 Hz.
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

The kernel's page tables map:

- the kernel image at 0xFFFFFFFF80000000 plus its physical address, with
  text read-only and executable, read-only data and the symbol table
  read-only, and data writable but not executable;
- every RAM range of the memory map at 0xFFFF800000000000 plus its
  physical address (the direct map), not executable;
- device registers, the framebuffer and the other CPUs' stacks in a window
  at 0xFFFFC00000000000, on demand.

Nothing is mapped at address 0, so a null pointer faults. Each CPU's stack
has an unmapped guard page below it, and double faults, NMIs and machine
checks run on separate stacks, so a stack overflow still produces a panic
report.

## Reserved memory

Before it allocates anything the kernel sets aside, and lists at boot: the
first MiB (firmware data and the page that starts the other CPUs), the
kernel image, the boot information, boot modules, and the 1 MiB crash log.
The crash log goes at the first 2 MiB boundary above 4 GiB where there is
RAM, or else at 64 MiB, so it is in the same place on every boot of the
same machine; see *radix-debug*(7).

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
	Run a self-test at the end of boot: *page-fault*, *crash-reboot* or
	*breakpoint*. See *radix-debug*(7). *ob test* uses these.

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

*radix-debug*(7), *ob*(1), *orthros.toml*(5), *grub-mkrescue*(1)

The Multiboot2 specification:
https://www.gnu.org/software/grub/manual/multiboot2/

The ACPI specification, version 6.5: https://uefi.org/specifications
