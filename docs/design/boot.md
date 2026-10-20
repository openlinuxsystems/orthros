# Boot, memory layout and consoles

This note describes how the Radix kernel gets from GRUB to
`radix_kernel_main`, how its image and address space are laid out, and how
its consoles and logs fit together. It reflects Phase 1. The user-facing
description is `orthros-boot(7)`; debugging aids are in `radix-debug(7)`.

## Crates

```text
kernel/radix            the kernel itself: architecture-independent, no unsafe
kernel/hal              traits the kernel uses to reach hardware, no unsafe
kernel/arch/x86_64      the HAL on PCs: boot, CPU tables, memory, APIC, SMP
kernel/arch/aarch64     HAL stubs that only have to compile (CI lint)
kernel/lib/*            host-tested building blocks, no unsafe except sync
```

`radix` names the architecture's HAL type `Arch` and calls only trait
methods on it, so code that assumes x86-64 fails the AArch64 lint. The
architecture crate owns the entry point and calls `radix_kernel_main` once
the CPU, memory and page tables are set up.

`unsafe` is allowed in two crates only: `radix-arch-x86_64` (hardware
access) and `radix-sync` (handing out references guarded by atomics). Every
other kernel crate is `#![forbid(unsafe_code)]`, and the parsers of
untrusted data (Multiboot2 boot information, ACPI tables, microcode files,
PSF fonts, the symbol table and the crash log) are plain byte-slice code
with unit tests on the build host (ADR 0013).

## Image layout

`kernel/arch/x86_64/linker.ld` produces an ELF executable (`ET_EXEC`) with
two parts:

| Part | Virtual address | Physical address | Contents |
| --- | --- | --- | --- |
| Boot | 1 MiB, identity | 1 MiB | Multiboot2 header, 32-bit trampoline, boot page tables |
| Kernel | `0xFFFFFFFF80000000` + physical | after the boot part | text, rodata, `.ksyms`, data, bss, boot stack |

GRUB loads each segment at its physical address. The kernel part is linked
in the top 2 GiB so it can use the `kernel` code model (32-bit signed
absolute addresses). Sections are page-aligned so the kernel can map them
with different permissions; `__kernel_*` symbols mark their bounds.

`.ksyms` is a 1 MiB read-only section of zeros, defined in `boot.rs`. After
linking, `ob build` writes the symbol table into it in place (format in
`radix-debug(7)`), so backtraces need no debug information and no address
in the image moves. Only symbols within 4 GiB of the section are kept,
which leaves out the boot code at 1 MiB.

The kernel target (`kernel/targets/x86_64-radix-kernel.json`, ADR 0011)
disables the red zone and SSE, keeps frame pointers everywhere, and
`-Zbuild-std` compiles `core` with those settings so backtraces walk
through it.

## The trampoline

GRUB enters `_start` in 32-bit protected mode with paging off. The
trampoline in `boot.rs`:

1. saves the Multiboot2 magic and information pointer;
2. checks CPUID for long mode (halts if absent);
3. zeroes seven page-table pages in `.boot.bss` and maps the first 4 GiB
   with 2 MiB pages at virtual 0 (to keep running), at the direct map base
   `0xFFFF800000000000` (so pointers into boot information stay valid
   after the switch to the kernel's tables), and the first GiB at
   `0xFFFFFFFF80000000` (the link address);
4. enables PAE, long mode and paging, loads a temporary GDT and far-jumps
   into 64-bit code;
5. switches to the boot CPU's stack in the higher half and calls
   `radix_arch_entry`.

## radix_arch_entry

`entry.rs` runs on the boot CPU with interrupts off:

1. Records the TSC (time zero), starts COM1 if it exists, and registers it
   as a log sink.
2. Loads the boot CPU's GDT, TSS (with interrupt stacks for double faults,
   NMIs and machine checks) and IDT. From here an exception produces a
   panic report.
3. Checks the magic value and parses the boot information with
   `radix-bootinfo`.
4. Starts trace points if `trace` is on the command line.
5. Records the memory map and reserves what must not be reused: the first
   MiB, the kernel image, the boot information, boot modules, and the crash
   log. The rest of usable RAM above 1 MiB goes to a bump allocator of
   4 KiB frames that never frees, enough for page tables and stacks until
   Phase 2's frame allocator. Page tables come from below 4 GiB, where
   the trampoline's mapping and the 32-bit part of the AP trampoline can
   reach them.
6. Applies early microcode from the module tagged `microcode`.
7. Builds the kernel's page tables (`paging.rs`) and switches CR3.
8. Fills `BootParams` (command line, firmware, memory map, reservations,
   framebuffer or VGA text screen) and calls `radix_kernel_main`.

## Address space

| Range | Contents | Permissions |
| --- | --- | --- |
| `0xFFFFFFFF80000000` + phys | kernel image, 4 KiB pages | text RX, rodata and `.ksyms` R, data RW, NX on all but text |
| `0xFFFF800000000000` + phys | every usable RAM range (direct map) | RW, NX; 2 MiB pages where aligned |
| `0xFFFFC00000000000`, 512 GiB | device registers, framebuffer, AP stacks with guard pages | mapped on demand, uncached or write-combining as needed |
| everything else | unmapped, including address 0 | |

While the other CPUs start, the page below 1 MiB that holds their
real-mode trampoline is also mapped at its physical address. The page
attribute table is reprogrammed so device memory can be uncached and the
framebuffer write-combining. The boot CPU's stack has an unmapped guard
page below it; a page fault in a guard page is reported as a stack
overflow.

Phase 2 replaces the bump allocator and fixed layout with the real memory
manager, KASLR and SMEP/SMAP.

## radix_kernel_main

`kernel/radix/src/main.rs` starts the framebuffer console first, because on
the reference laptop the screen is the only output; it replays the log
records kept so far. Then the crash log, the boot report (version, boot
loader, firmware, command line, memory map, reservations), the previous
boot's crash log, `Arch::init` (CPU, ACPI, APICs, timers; see
`interrupts.md`), the other CPUs, the CPU list, and the checks that every
CPU takes timer interrupts and answers a ping IPI. A self-test, `kdb` and
the `qemu-exit` flag come last.

## Logging and consoles

`radix-klog` keeps every message in an in-memory ring of records (level,
time, CPU, text) and passes it to each registered sink:

| Sink | Crate | Levels | Notes |
| --- | --- | --- | --- |
| Serial | arch | `loglevel` and above | 16550 at 0x3F8, 115200 8N1 |
| VGA text | arch | `loglevel` and above | only when GRUB left text mode |
| Framebuffer | `radix-fbcon` via `radix` | `loglevel` and above | Spleen PSF fonts (ADR 0015), colours by level, scrolls |
| Crash log | `radix-klog::crashlog` via `radix` | all | reserved RAM, survives warm restarts (ADR 0012) |

A sink registered late can ask for the records logged before it, which is
how the framebuffer shows the start of boot. Sinks take their own locks,
so code must never log while holding a lock that a sink also takes; the
crash log's report of the previous boot copies its lines out and drops its
lock before logging them.

The panic path writes its report with `write_raw`, which skips the record
ring, and draws the framebuffer itself. `radix-sync`'s panic mode lets it
take locks held by CPUs it stopped.

## Time

Log times come from the TSC, counted from the first Rust instruction.
Until calibration the TSC rate is unknown and times print as 0. See
`interrupts.md` for calibration.
