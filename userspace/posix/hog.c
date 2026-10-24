// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the spark-oom boot test runs as a service: it takes memory a MiB
// at a time and touches every page, until the out-of-memory policy stops
// its domain. It never ends by itself.

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

// The blocks taken, each holding the one before it, so the compiler
// cannot leave them out.
static void *volatile last;

int main(void) {
	printf("hog: taking memory\n");
	fflush(stdout);
	for(size_t mib = 1;; mib++) {
		volatile char *block = malloc(1 << 20);
		if(!block) {
			// A failed allocation is not the end: the policy stops us.
			usleep(10000);
			continue;
		}
		// A write to every page, so each is really there.
		for(size_t at = 0; at < 1 << 20; at += 4096)
			block[at] = (char)mib;
		*(void *volatile *)block = last;
		last = (void *)block;
		if(mib % 64 == 0) {
			printf("hog: %zu MiB\n", mib);
			fflush(stdout);
		}
	}
}
