// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The `syscall` instruction (radix-syscalls(2)): the number in `rax`, ten
//! arguments in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`, `r12` to `r15`;
//! the status back in `rax` and the results in the same ten registers.
//! `rcx` and `r11` are overwritten.

use radix_abi::Syscalls;

/// Makes system call `number` with `args`.
#[inline(always)]
pub fn syscall(number: u64, args: [u64; 10]) -> (u64, [u64; 10]) {
    let mut r = args;
    let status: u64;
    // SAFETY: the kernel reads only these registers, writes only these and
    // rcx and r11 (declared clobbered), and touches no memory of the
    // program's but the buffers the arguments name, which the caller
    // passes as references it holds; it does not use the program's stack.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => status,
            inlateout("rdi") r[0],
            inlateout("rsi") r[1],
            inlateout("rdx") r[2],
            inlateout("r10") r[3],
            inlateout("r8") r[4],
            inlateout("r9") r[5],
            inlateout("r12") r[6],
            inlateout("r13") r[7],
            inlateout("r14") r[8],
            inlateout("r15") r[9],
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    (status, r)
}

/// The kernel, through the `syscall` instruction.
pub struct Sys;

impl Syscalls for Sys {
    #[inline(always)]
    fn syscall(&self, number: u64, args: [u64; 10]) -> (u64, [u64; 10]) {
        syscall(number, args)
    }
}

/// The kernel, for `radix_abi`'s wrappers.
pub static SYS: Sys = Sys;
