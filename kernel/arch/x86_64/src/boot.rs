//! Multiboot2 header and the 32-bit to 64-bit boot trampoline.
//!
//! GRUB enters `_start` in 32-bit protected mode with paging disabled,
//! the Multiboot2 magic in EAX and the boot information address in EBX.
//! The trampoline identity-maps the first 4 GiB with 2 MiB pages, enables
//! long mode, loads a minimal GDT and calls `kernel_entry`, which the kernel
//! crate defines, with the magic and the information address.

core::arch::global_asm!(
    r#"
    /* ---- Multiboot2 header (must be in the first 32 KiB, 8-byte aligned) ---- */
    .section .multiboot2, "a"
    .balign 8
mb2_header_start:
    .long 0xE85250D6                                    /* magic */
    .long 0                                             /* architecture: i386 protected mode */
    .long mb2_header_end - mb2_header_start             /* header length */
    .long 0x100000000 - (0xE85250D6 + (mb2_header_end - mb2_header_start))
    .balign 8
    .short 0                                            /* end tag: type */
    .short 0                                            /* end tag: flags */
    .long 8                                             /* end tag: size */
mb2_header_end:

    /* ---- 32-bit entry ---- */
    .section .boot.text, "ax"
    .code32
    .global _start
_start:
    cli
    movl $boot_stack_top, %esp
    movl %eax, boot_mb_magic
    movl %ebx, boot_mb_info

    /* Long mode is required: CPUID 0x80000001, EDX bit 29. */
    movl $0x80000000, %eax
    cpuid
    cmpl $0x80000001, %eax
    jb .Lno_long_mode
    movl $0x80000001, %eax
    cpuid
    testl $(1 << 29), %edx
    jz .Lno_long_mode

    /* Zero the six page-table pages: PML4, PDPT and four page directories. */
    movl $boot_pml4, %edi
    movl $(6 * 4096 / 4), %ecx
    xorl %eax, %eax
    rep stosl

    /* PML4[0] -> PDPT. */
    movl $boot_pdpt, %eax
    orl $0x3, %eax
    movl %eax, boot_pml4

    /* PDPT[0..4] -> the four page directories. */
    xorl %ecx, %ecx
.Lfill_pdpt:
    movl %ecx, %eax
    shll $12, %eax
    addl $boot_pd, %eax
    orl $0x3, %eax
    movl %eax, boot_pdpt(,%ecx,8)
    incl %ecx
    cmpl $4, %ecx
    jne .Lfill_pdpt

    /* 2048 entries of 2 MiB pages: present, writable, huge. */
    xorl %ecx, %ecx
.Lfill_pd:
    movl %ecx, %eax
    shll $21, %eax
    orl $0x83, %eax
    movl %eax, boot_pd(,%ecx,8)
    incl %ecx
    cmpl $2048, %ecx
    jne .Lfill_pd

    /* CR3, CR4.PAE, EFER.LME, CR0.PG. */
    movl $boot_pml4, %eax
    movl %eax, %cr3
    movl %cr4, %eax
    orl $(1 << 5), %eax
    movl %eax, %cr4
    movl $0xC0000080, %ecx
    rdmsr
    orl $(1 << 8), %eax
    wrmsr
    movl %cr0, %eax
    orl $(1 << 31), %eax
    movl %eax, %cr0

    lgdt boot_gdt_ptr
    ljmp $0x08, $.Llong_mode

.Lno_long_mode:
    hlt
    jmp .Lno_long_mode

    /* ---- 64-bit entry ---- */
    .code64
.Llong_mode:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    xorw %ax, %ax
    movw %ax, %fs
    movw %ax, %gs
    movq $boot_stack_top, %rsp
    xorl %ebp, %ebp

    /* kernel_entry(magic: u32, info: usize); 32-bit loads zero-extend. */
    movl boot_mb_magic(%rip), %edi
    movl boot_mb_info(%rip), %esi
    call kernel_entry
.Lhalt:
    cli
    hlt
    jmp .Lhalt

    /* ---- Boot GDT: null, 64-bit code (0x08), data (0x10) ---- */
    .section .boot.rodata, "a"
    .balign 8
boot_gdt:
    .quad 0
    .quad 0x00AF9A000000FFFF
    .quad 0x00CF92000000FFFF
boot_gdt_end:
boot_gdt_ptr:
    .word boot_gdt_end - boot_gdt - 1
    .long boot_gdt

    /* ---- Boot data ---- */
    .section .bss.boot, "aw", @nobits
    .balign 4096
boot_pml4:
    .skip 4096
boot_pdpt:
    .skip 4096
boot_pd:
    .skip 4 * 4096
boot_stack_bottom:
    .skip 64 * 1024
boot_stack_top:
boot_mb_magic:
    .skip 4
boot_mb_info:
    .skip 4

    .code64
    .text
"#,
    options(att_syntax)
);
