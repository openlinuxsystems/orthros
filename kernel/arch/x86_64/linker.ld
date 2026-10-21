/*
 * Radix kernel linker script (x86-64).
 *
 * GRUB loads the kernel through Multiboot2 at the physical addresses in the
 * program headers (the LMAs below) and jumps to _start in 32-bit protected
 * mode with paging off. The boot code and its page tables therefore sit at
 * 1 MiB with equal virtual and physical addresses. Everything else is
 * linked in the top 2 GiB of the address space (the higher half, at
 * KERNEL_OFFSET + physical address), where the boot trampoline maps it
 * before calling into Rust. With KASLR the trampoline then moves it up by
 * a random multiple of 2 MiB (src/kaslr.rs), patching the addresses that
 * `ob build` lists in .krelocs (the kernel is linked with --emit-relocs).
 *
 * The section boundaries below are read by the kernel to map the image with
 * the right permissions: text executable, rodata and the symbol table
 * read-only, data and bss writable, nothing writable and executable.
 * docs/design/boot.md describes the layout.
 */

ENTRY(_start)

KERNEL_OFFSET = 0xFFFFFFFF80000000;

SECTIONS
{
    . = 1M;
    __kernel_phys_start = .;

    .boot : ALIGN(8)
    {
        KEEP(*(.multiboot2))
        *(.boot.text)
        *(.boot.rodata)
    }

    /* Boot page tables, which the trampoline zeroes itself. The section is
     * @nobits, so it takes no space in the file but GRUB still reserves it. */
    .boot.bss : ALIGN(4K)
    {
        *(.boot.bss)
    }
    __boot_end = .;

    . = ALIGN(4K) + KERNEL_OFFSET;

    __kernel_text_start = .;
    .text : AT(ADDR(.text) - KERNEL_OFFSET)
    {
        *(.text .text.*)
    }
    . = ALIGN(4K);
    __kernel_text_end = .;

    __kernel_rodata_start = .;
    .rodata : AT(ADDR(.rodata) - KERNEL_OFFSET)
    {
        *(.rodata .rodata.*)
    }
    .data.rel.ro : AT(ADDR(.data.rel.ro) - KERNEL_OFFSET)
    {
        *(.data.rel.ro .data.rel.ro.*)
    }
    .got : AT(ADDR(.got) - KERNEL_OFFSET)
    {
        *(.got .got.*)
    }
    /* The kernel symbol table, which `ob build` writes into this section
     * of the linked file (src/boot.rs reserves the space). */
    .ksyms : AT(ADDR(.ksyms) - KERNEL_OFFSET) ALIGN(16)
    {
        KEEP(*(.ksyms))
    }
    /* The KASLR relocation table, filled the same way. */
    .krelocs : AT(ADDR(.krelocs) - KERNEL_OFFSET) ALIGN(16)
    {
        KEEP(*(.krelocs))
    }
    . = ALIGN(4K);
    __kernel_rodata_end = .;

    __kernel_data_start = .;
    .data : AT(ADDR(.data) - KERNEL_OFFSET)
    {
        *(.data .data.*)
    }
    .bss : AT(ADDR(.bss) - KERNEL_OFFSET) ALIGN(4K)
    {
        *(.bss.stack)
        *(.bss .bss.*)
        *(COMMON)
    }
    . = ALIGN(4K);
    __kernel_data_end = .;

    __kernel_phys_end = . - KERNEL_OFFSET;

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame_hdr)
        *(.comment)
        *(.note .note.*)
    }
}
