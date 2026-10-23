// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// A new thread's first instruction: PrepareStack (thread.cpp) left the
// thread's function, its TCB and its argument on the stack.

	.section .text
	.global __mlibc_thread_entry
	.type __mlibc_thread_entry, @function
__mlibc_thread_entry:
	xor %ebp, %ebp
	pop %rdi
	pop %rsi
	pop %rdx
	call __mlibc_thread_trampoline
	ud2
	.size __mlibc_thread_entry, . - __mlibc_thread_entry

	.section .note.GNU-stack,"",%progbits
