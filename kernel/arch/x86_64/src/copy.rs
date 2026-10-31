// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel's `memcpy`, which takes the place of compiler_builtins'
//! (whose definition is weak, so that one like this wins).
//!
//! It copies as compiler_builtins' does on a target without ERMS: bytes
//! up to a destination on 8 bytes, `rep movsq`, then the bytes left. But a
//! long copy whose destination is a little ahead of its source within a
//! page goes backward, from the end, as the copies to and from user memory
//! do ([`BACKWARD_WINDOW`]): AMD's Zen 3 and later run `rep movs` many
//! times slower then, every load waiting for a store with the same low 12
//! bits (ADR 0185). Where the bytes of a socket's buffer go into a packet
//! for the loopback interface, the packet's payload is a header's length
//! ahead of where they are in the buffer, page for page.

use crate::user::{BACKWARD_MIN, BACKWARD_WINDOW};

core::arch::global_asm!(
    r#"
    .pushsection .text.memcpy,"ax",@progbits
    .balign 16
    .global memcpy
    .type memcpy, @function
memcpy:
    mov rax, rdi
    cmp rdx, {backward_min}
    jb 2f
    // Backward when the destination is 1 to BACKWARD_WINDOW - 1 bytes
    // ahead of the source, modulo 4 KiB.
    mov ecx, edi
    sub ecx, esi
    and ecx, 4095
    dec ecx
    cmp ecx, {backward_window} - 2
    jbe 3f
2:
    // Forward: bytes until the destination is on 8 bytes, 8 bytes at a
    // time, then the bytes left.
    mov ecx, edi
    neg ecx
    and ecx, 7
    cmp rcx, rdx
    cmova rcx, rdx
    sub rdx, rcx
    rep movsb
    mov rcx, rdx
    shr rcx, 3
    rep movsq
    mov ecx, edx
    and ecx, 7
    rep movsb
    ret
3:
    // From the end: single bytes until a multiple of 8 is left, then 32
    // bytes at a time, then 8. RCX is what is left to copy, at the start.
    mov rcx, rdx
4:
    test cl, 7
    jz 5f
    movzx r8d, byte ptr [rsi + rcx - 1]
    mov byte ptr [rdi + rcx - 1], r8b
    dec rcx
    jmp 4b
5:
    cmp rcx, 32
    jb 7f
6:
    mov r8, qword ptr [rsi + rcx - 8]
    mov r9, qword ptr [rsi + rcx - 16]
    mov r10, qword ptr [rsi + rcx - 24]
    mov r11, qword ptr [rsi + rcx - 32]
    mov qword ptr [rdi + rcx - 8], r8
    mov qword ptr [rdi + rcx - 16], r9
    mov qword ptr [rdi + rcx - 24], r10
    mov qword ptr [rdi + rcx - 32], r11
    sub rcx, 32
    cmp rcx, 32
    jae 6b
7:
    test rcx, rcx
    jz 8f
    mov r8, qword ptr [rsi + rcx - 8]
    mov qword ptr [rdi + rcx - 8], r8
    sub rcx, 8
    jmp 7b
8:
    ret
    .size memcpy, . - memcpy
    .popsection
    "#,
    backward_min = const BACKWARD_MIN,
    backward_window = const BACKWARD_WINDOW,
);
