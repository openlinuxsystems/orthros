// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The start of the _init and _fini functions, which crtn.S ends: the
// linker puts the .init and .fini sections of the objects between them.
// The push keeps the stack 16-byte aligned for what they call.

	.section .init
	.global _init
	.type _init, @function
_init:
	push %rax

	.section .fini
	.global _fini
	.type _fini, @function
_fini:
	push %rax

	.section .note.GNU-stack,"",%progbits
