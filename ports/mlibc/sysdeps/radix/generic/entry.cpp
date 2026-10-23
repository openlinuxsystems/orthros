// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The C library's start (crt1.S calls it): the working directory from the
// auxiliary vector, then the dynamic linker's work (or, in a static
// program, its own relocation and TLS), then main.

#include <bits/ensure.h>
#include <mlibc/elf/startup.h>
#include <stdint.h>
#include <stdlib.h>

#include "radix.hpp"

extern "C" void __dlapi_enter(uintptr_t *);

extern char **environ;

namespace {

// The auxiliary vector, past the arguments and the environment on the
// stack the program started with.
const uintptr_t *auxv_of(const uintptr_t *entry_stack) {
	const uintptr_t *p = entry_stack + 1 + entry_stack[0] + 1;
	while(*p)
		p++;
	return p + 1;
}

} // namespace

extern "C" void __mlibc_entry(int (*main_fn)(int argc, char *argv[], char *env[]),
		uintptr_t *entry_stack) {
	// Before constructors run, which may use relative paths. It touches
	// neither TLS nor anything the C library's relocation changes (the
	// kernel relocates a static program before it starts).
	radix::init_from_auxv(auxv_of(entry_stack));
	__dlapi_enter(entry_stack);
	auto result = main_fn(mlibc::entry_stack.argc, mlibc::entry_stack.argv, environ);
	exit(result);
}
