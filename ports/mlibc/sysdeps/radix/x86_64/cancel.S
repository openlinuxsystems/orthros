// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The system call of a cancellation point (radix.hpp's `cancellable`):
//
//   uint64_t __radix_cancellable_syscall(uint64_t number,
//           const uint64_t in[10], uint64_t out[10], uint64_t masked);
//
// radix_syscall's, but that a cancel which is due before the call is made
// is acted on instead, by __radix_cancel(masked): it ends the thread, or,
// if `masked`, returns the status that the call returns. Between
// __radix_cancel_begin and __radix_cancel_end the call has not been made,
// or the kernel has put the thread back on the `syscall` instruction to
// make it again (a handler with SA_RESTART ran); SIGCANCEL's handler sends
// a thread it finds there to __radix_cancel_now (CancelInContext, in
// core.cpp), with the registers it has there: the stack as this function
// leaves it from the begin label on, and `masked` in RBX.

	.section .text
	.global __radix_cancellable_syscall
	.hidden __radix_cancellable_syscall
	.global __radix_cancel_begin
	.hidden __radix_cancel_begin
	.global __radix_cancel_end
	.hidden __radix_cancel_end
	.global __radix_cancel_now
	.hidden __radix_cancel_now
	.type __radix_cancellable_syscall, @function
__radix_cancellable_syscall:
	// R12 to R15 carry arguments and results, and RBX `masked`: all
	// callee-saved. With `out` and the padding the stack is 16-byte
	// aligned for the call below.
	push %rbx
	push %r12
	push %r13
	push %r14
	push %r15
	push %rdx
	sub $8, %rsp
	mov %rcx, %rbx
	mov %rdi, %rax
	mov %rsi, %r11
	mov 0(%r11), %rdi
	mov 8(%r11), %rsi
	mov 16(%r11), %rdx
	mov 24(%r11), %r10
	mov 32(%r11), %r8
	mov 40(%r11), %r9
	mov 48(%r11), %r12
	mov 56(%r11), %r13
	mov 64(%r11), %r14
	mov 72(%r11), %r15
__radix_cancel_begin:
	// The Tcb's cancelBits: tcbCancelEnableBit and tcbCancelTriggerBit
	// (mlibc's tcb.hpp, which has the offset checked).
	mov %fs:0x30, %r11d
	and $5, %r11d
	cmp $5, %r11d
	je __radix_cancel_now
	syscall
__radix_cancel_end:
	mov 8(%rsp), %r11
	mov %rdi, 0(%r11)
	mov %rsi, 8(%r11)
	mov %rdx, 16(%r11)
	mov %r10, 24(%r11)
	mov %r8, 32(%r11)
	mov %r9, 40(%r11)
	mov %r12, 48(%r11)
	mov %r13, 56(%r11)
	mov %r14, 64(%r11)
	mov %r15, 72(%r11)
1:
	add $16, %rsp
	pop %r15
	pop %r14
	pop %r13
	pop %r12
	pop %rbx
	ret
__radix_cancel_now:
	mov %rbx, %rdi
	call __radix_cancel
	// Masked: the call returns the status __radix_cancel gave, with no
	// results.
	jmp 1b
	.size __radix_cancellable_syscall, . - __radix_cancellable_syscall

	.section .note.GNU-stack,"",%progbits
