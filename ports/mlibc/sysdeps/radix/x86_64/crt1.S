// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// A program's first instruction (radix-startup(2)): the kernel starts it
// with the stack pointer on argc, then argv, envp and the auxiliary
// vector, as System V's x86-64 ABI has them. The same file is crt1.o and,
// for position-independent programs, Scrt1.o.

	.section .text
	.global _start
	.type _start, @function
_start:
	xor %ebp, %ebp
	lea main(%rip), %rdi
	mov %rsp, %rsi
	and $-16, %rsp
	call __mlibc_entry
	ud2
	.size _start, . - _start

	.section .note.GNU-stack,"",%progbits
