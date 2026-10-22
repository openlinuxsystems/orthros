// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The first C program on Radix: the C library starts it, and stdio,
// malloc and the environment work (radix-posix(7)).

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
	(void)argc;
	char cwd[256];
	if(!getcwd(cwd, sizeof(cwd))) {
		perror("getcwd");
		return 1;
	}
	char *buffer = malloc(1 << 20);
	if(!buffer) {
		perror("malloc");
		return 1;
	}
	strcpy(buffer, "a megabyte from malloc");
	const char *path = getenv("PATH");
	printf("hello: %s in %s, pid %d, PATH=%s, %s\n", argv[0], cwd, (int)getpid(),
			path ? path : "(none)", buffer);
	free(buffer);
	return 0;
}
