// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The private library of the `dynamic` test (libgreet.so), which its
// program finds next to it, as a bundle's libraries are (ADR 0071).

#ifndef POSIX_DYNAMIC_GREET_H
#define POSIX_DYNAMIC_GREET_H

#include <stddef.h>

// Whether the library's constructor has run.
extern int greet_constructed;

// Data of the library's that the program reads.
extern int greet_shared;

// Writes "hello, <name>" into `buffer`, as snprintf does, and counts the
// call in the calling thread's count.
int greet(const char *name, char *buffer, size_t size);

// How many times the calling thread has called greet.
int greet_calls(void);

#endif
