// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The end of the _init and _fini functions crti.S starts.

	.section .init
	pop %rax
	ret

	.section .fini
	pop %rax
	ret

	.section .note.GNU-stack,"",%progbits
