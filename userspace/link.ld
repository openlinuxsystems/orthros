/* SPDX-FileCopyrightText: 2026 The OrthrOS contributors */
/* SPDX-License-Identifier: Apache-2.0 OR MIT */

/*
 * The layout of a Radix user program: code, read-only data and data in
 * segments of their own, each starting on a page, so that every page has
 * one access and none is both writable and executable. Programs start at
 * 4 MiB, above the start-up page at 0x10000 (radix-startup(2)).
 */

ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);  /* read, execute */
    rodata PT_LOAD FLAGS(4);  /* read */
    data   PT_LOAD FLAGS(6);  /* read, write */
}

SECTIONS
{
    . = 0x400000;

    .text : ALIGN(4096)
    {
        *(.text._start)
        *(.text .text.*)
    } :text

    . = ALIGN(4096);
    .rodata : ALIGN(4096)
    {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(4096);
    .data : ALIGN(4096)
    {
        *(.data .data.*)
        *(.got .got.*)
    } :data

    .bss :
    {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ :
    {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
    }
}
