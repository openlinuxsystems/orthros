//! Multiboot2 header and the 32-bit to 64-bit boot trampoline.
//!
//! GRUB enters `_start` in 32-bit protected mode with paging disabled, the
//! Multiboot2 magic in EAX and the physical address of the boot information
//! in EBX. The trampoline builds temporary page tables that map the first
//! 4 GiB three times, enables long mode and calls [`crate::entry`] in the
//! higher half on the boot CPU's stack:
//!
//! - at 0, so the trampoline keeps running after paging is switched on;
//! - at the direct map base (`0xFFFF_8000_0000_0000`), the same alias the
//!   kernel's own page tables use for RAM, so pointers made from boot
//!   information stay valid when those tables replace these;
//! - at `0xFFFF_FFFF_8000_0000` (first GiB only), where the kernel is linked.
//!
//! The header also asks GRUB for a linear framebuffer, which the kernel's
//! framebuffer console draws on. The request is optional: if GRUB cannot set
//! a graphics mode it passes the VGA text screen instead.

core::arch::global_asm!(
    r#"
    /* ---- Multiboot2 header (in the first 32 KiB, 8-byte aligned) ---- */
    .section .multiboot2, "a"
    .balign 8
mb2_header_start:
    .long 0xE85250D6                                    /* magic */
    .long 0                                             /* architecture: i386 protected mode */
    .long mb2_header_end - mb2_header_start             /* header length */
    .long 0x100000000 - (0xE85250D6 + (mb2_header_end - mb2_header_start))
    /* Framebuffer tag, optional: any size the boot loader picks, 32 bpp. */
    .balign 8
    .short 5
    .short 1
    .long 20
    .long 0
    .long 0
    .long 32
    /* End tag. */
    .balign 8
    .short 0
    .short 0
    .long 8
mb2_header_end:

    /* ---- 32-bit entry ---- */
    .section .boot.text, "ax"
    .code32
    .global _start
_start:
    cli
    cld
    movl $boot_low_stack_top, %esp
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

    /* Zero the seven page-table pages. */
    movl $boot_pml4, %edi
    movl $(7 * 4096 / 4), %ecx
    xorl %eax, %eax
    rep stosl

    /* PML4[0] and PML4[256] -> the low PDPT; PML4[511] -> the high PDPT. */
    movl $boot_pdpt_low, %eax
    orl $0x3, %eax
    movl %eax, boot_pml4
    movl %eax, boot_pml4 + 256 * 8
    movl $boot_pdpt_high, %eax
    orl $0x3, %eax
    movl %eax, boot_pml4 + 511 * 8

    /* Low PDPT[0..4] -> the four page directories (4 GiB). */
    xorl %ecx, %ecx
.Lfill_pdpt:
    movl %ecx, %eax
    shll $12, %eax
    addl $boot_pd, %eax
    orl $0x3, %eax
    movl %eax, boot_pdpt_low(,%ecx,8)
    incl %ecx
    cmpl $4, %ecx
    jne .Lfill_pdpt

    /* High PDPT[510] (0xFFFFFFFF80000000) -> the first page directory. */
    movl $boot_pd, %eax
    orl $0x3, %eax
    movl %eax, boot_pdpt_high + 510 * 8

    /* 2048 entries of 2 MiB pages: present, writable, large. */
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

    /* ---- 64-bit, still running from the identity mapping ---- */
    .code64
.Llong_mode:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    xorw %ax, %ax
    movw %ax, %fs
    movw %ax, %gs

    /* Switch to the boot CPU's stack in the higher half and call
       entry(magic: u32, info: u32). */
    movabsq $bsp_stack_top, %rsp
    xorl %ebp, %ebp
    movl boot_mb_magic, %edi
    movl boot_mb_info, %esi
    movabsq $radix_arch_entry, %rax
    call *%rax
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

    /* ---- Boot data, below 1 MiB + kernel size ---- */
    .section .boot.bss, "aw", @nobits
    .balign 4096
boot_pml4:
    .skip 4096
boot_pdpt_low:
    .skip 4096
boot_pdpt_high:
    .skip 4096
boot_pd:
    .skip 4 * 4096
boot_low_stack:
    .skip 4096
boot_low_stack_top:
boot_mb_magic:
    .skip 4
boot_mb_info:
    .skip 4

    /* ---- The boot CPU's kernel stack, with an unmapped guard page ---- */
    .section .bss.stack, "aw", @nobits
    .balign 4096
    .global bsp_stack_guard
bsp_stack_guard:
    .skip 4096
    .global bsp_stack_bottom
bsp_stack_bottom:
    .skip 64 * 1024
    .global bsp_stack_top
bsp_stack_top:

    /* ---- Room for the kernel symbol table (radix-ksyms) ----
     * `ob build` writes the table here after linking. The kernel finds it
     * between these two symbols; it is zeros (no table) until then. */
    .section .ksyms, "a", @progbits
    .balign 16
    .global __ksyms_start
__ksyms_start:
    .skip 1024 * 1024
    .global __ksyms_end
__ksyms_end:

    .code64
    .text
"#,
    options(att_syntax)
);
