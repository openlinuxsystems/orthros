// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Where every signal handler returns (radix-signal(2)): the kernel puts
// this address below the frame, so the handler's `ret` comes here with
// the stack pointer on the frame's ucontext_t, which the process's
// signal_return method restores. It does not return.

	.section .text
	.global __radix_signal_restorer
	.type __radix_signal_restorer, @function
__radix_signal_restorer:
	// cap_call(SLOT_PROCESS, signal_return with one argument, context).
	mov %rsp, %rdx
	mov $6, %edi
	movabs $0x1000a001a, %rsi
	xor %eax, %eax
	syscall
	ud2
	.size __radix_signal_restorer, . - __radix_signal_restorer

	.section .note.GNU-stack,"",%progbits
