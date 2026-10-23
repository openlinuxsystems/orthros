// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// libgreet.so, the `dynamic` test's private library (greet.h): data,
// a constructor, thread-local storage, and calls into the shared C
// library.

#include <stdio.h>

#include "greet.h"

int greet_constructed;
int greet_shared = 42;

// Each thread's own count, in the static TLS block the dynamic linker
// lays out for the libraries a program starts with.
static _Thread_local int calls;

__attribute__((constructor)) static void construct(void) {
	greet_constructed = 1;
}

int greet(const char *name, char *buffer, size_t size) {
	calls++;
	return snprintf(buffer, size, "hello, %s", name);
}

int greet_calls(void) {
	return calls;
}
