orthros-boot(7) "OrthrOS" "OrthrOS Manual"

# NAME

orthros-boot - how OrthrOS boots, and the kernel command line

# DESCRIPTION

This page describes the boot path as of Phase 0 of the roadmap, where the
Radix kernel is a stub that proves the path from firmware to Rust code. It
grows with every phase that changes how the system starts.

## Boot image

*ob image* builds a hybrid image with *grub-mkrescue*(1). It holds GRUB for
legacy BIOS (an El Torito boot record and an MBR boot record) and for UEFI
(an EFI system partition with _EFI/BOOT/BOOTX64.EFI_), the kernel at
_/boot/radix_, and the menu at _/boot/grub/grub.cfg_, generated from
_image/grub/grub.cfg.in_. The same file therefore boots from a USB stick, a
hard disk or optical media, on UEFI and BIOS machines.

## Sequence

. The firmware loads GRUB from the boot medium.
. GRUB shows its menu for *boot.timeout* seconds (see *orthros.toml*(5)),
  on the screen and on the first serial port.
. GRUB loads _/boot/radix_, an ELF executable linked at physical address
  1 MiB, through the Multiboot2 protocol. Under UEFI it exits the firmware's
  boot services first.
. GRUB jumps to the kernel's 32-bit entry point in protected mode, with the
  Multiboot2 magic value in EAX and the address of the boot information in
  EBX.
. The kernel's trampoline checks that the CPU supports long mode, maps the
  first 4 GiB of physical memory one-to-one with 2 MiB pages, enables
  64-bit long mode and calls the Rust function *kernel_entry*.
. *kernel_entry* checks the magic value, parses the boot information and
  prints a report on the console: version, boot loader, firmware, command
  line and usable memory. It ends with the line *Boot complete.* and halts
  the CPU.

## Console

The kernel writes to the first serial port (COM1, I/O port 0x3F8, 115200
baud, 8 data bits, no parity, 1 stop bit). On BIOS boots it also writes to
the VGA text screen.

On UEFI boots the Phase 0 kernel shows nothing on the screen: it does not
request a framebuffer, so GRUB prints "no suitable video mode found" and
"no console will be available to OS" before starting it. Both messages are
expected. The framebuffer console arrives in Phase 1.

# KERNEL COMMAND LINE

GRUB passes everything after the kernel path on the *multiboot2* line. *ob*
fills it from *boot.cmdline* in _orthros.toml_. Flags are separated by
spaces; unknown flags are ignored.

*qemu-exit*
	When the kernel finishes, or panics after reading its command line, leave QEMU through its
	*isa-debug-exit* device at I/O port 0xF4. QEMU then exits with status 33
	on success and 35 after a panic. *ob test* boots every test image with
	this flag. On real hardware the flag has no effect.

# REAL HARDWARE

The reference machine is a Dell Inspiron 15 5570. It has no serial port, so
in Phase 0 the kernel's report is visible only when the laptop boots the USB
stick in legacy BIOS mode, where the VGA text screen is used. Before booting
OrthrOS on it, set *SATA Operation* to *AHCI* and turn *Secure Boot* off in
the firmware setup (F2). See the handbook chapter "Supported hardware".

# SEE ALSO

*ob*(1), *orthros.toml*(5), *grub-mkrescue*(1)

The Multiboot2 specification:
https://www.gnu.org/software/grub/manual/multiboot2/
