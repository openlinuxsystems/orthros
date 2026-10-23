// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// plugin.so, which the `dynamic` test opens with dlopen: it needs
// libgreet.so too, and has thread-local storage of its own, which, in a
// library opened after the program started, the dynamic linker gives
// each thread when it first asks.

#include <stdio.h>

#include "greet.h"

static _Thread_local int uses = 100;
static int constructed;

__attribute__((constructor)) static void construct(void) {
	constructed = 1;
}

// libgreet's data, plus one, once its constructor and this one have run.
int plugin_answer(void) {
	return constructed && greet_constructed ? greet_shared + 1 : -1;
}

// Counts a use in the calling thread's count, and returns the count.
int plugin_use(void) {
	return ++uses;
}

// Greets `name` through libgreet, into `buffer`.
int plugin_greet(const char *name, char *buffer, size_t size) {
	char inner[64];
	greet(name, inner, sizeof(inner));
	return snprintf(buffer, size, "%s, from the plugin", inner);
}
