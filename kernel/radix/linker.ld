/*
 * Radix kernel linker script (x86-64, Phase 0).
 *
 * GRUB loads the kernel at physical 1 MiB through Multiboot2 and jumps to
 * _start in 32-bit protected mode. The Multiboot2 header must sit in the
 * first 32 KiB of the file, so .boot comes first. A higher-half layout
 * arrives with the memory manager in Phase 2.
 */

ENTRY(_start)

SECTIONS
{
    . = 1M;

    .boot : ALIGN(8)
    {
        KEEP(*(.multiboot2))
        *(.boot.text)
        *(.boot.rodata)
    }

    .text : ALIGN(4K)
    {
        *(.text .text.*)
    }

    .rodata : ALIGN(4K)
    {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K)
    {
        *(.data .data.*)
    }

    .bss : ALIGN(4K)
    {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame_hdr)
        *(.comment)
    }
}
