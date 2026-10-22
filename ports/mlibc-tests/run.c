// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The C library's own tests, in one program (build.sh links every test
// in, its main renamed): `run NAME` runs test NAME; `run` alone runs
// every test in a process of its own, each with its output in a file
// that is shown only if it fails, and exits with how many failed. A test
// passes when it exits with 0; the two that check failing on purpose
// pass when they do not. One program rather than 160 keeps the boot
// archive small: each would carry its own copy of the C library.

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include "tests.h"

#define OUTPUT "/tmp/libc-test.out"

// How long a test may take, in seconds, before SIGALRM ends it.
enum { LIMIT = 120 };

// The tests that pass by ending abnormally: abort() raises SIGABRT, and
// error() with a status exits with it.
static int must_fail(const char *name) {
	return !strcmp(name, "posix-abort") || !strcmp(name, "glibc-error_expect_fail");
}

// Prints what the test that just ran wrote, at most its last 2 KiB.
static void show_output(const char *name) {
	FILE *file = fopen(OUTPUT, "r");
	if(!file)
		return;
	char buffer[2048];
	fseek(file, 0, SEEK_END);
	long size = ftell(file);
	long keep = (long)sizeof(buffer) - 1;
	fseek(file, size > keep ? size - keep : 0, SEEK_SET);
	size_t got = fread(buffer, 1, sizeof(buffer) - 1, file);
	buffer[got] = 0;
	fclose(file);
	for(char *line = strtok(buffer, "\n"); line; line = strtok(NULL, "\n"))
		printf("libc: %s: | %s\n", name, line);
}

// Runs test `name` in a child, as `self name`; returns whether it passed.
static int run(const char *self, const char *name) {
	fflush(stdout);
	pid_t pid = fork();
	if(pid < 0) {
		printf("libc: FAILED: %s: fork: %s\n", name, strerror(errno));
		return 0;
	}
	if(pid == 0) {
		int out = open(OUTPUT, O_WRONLY | O_CREAT | O_TRUNC, 0644);
		if(out >= 0) {
			dup2(out, STDOUT_FILENO);
			dup2(out, STDERR_FILENO);
			close(out);
		}
		// A pending alarm lasts through exec.
		alarm(LIMIT);
		char *argv[] = {(char *)self, (char *)name, NULL};
		execv(self, argv);
		_exit(127);
	}
	int status;
	pid_t got;
	do {
		got = waitpid(pid, &status, 0);
	} while(got < 0 && errno == EINTR);
	if(got != pid) {
		printf("libc: FAILED: %s: waitpid: %s\n", name, strerror(errno));
		return 0;
	}
	int exited = WIFEXITED(status);
	int passed = must_fail(name) ? !(exited && WEXITSTATUS(status) == 0)
				     : exited && WEXITSTATUS(status) == 0;
	if(passed)
		return 1;
	if(must_fail(name))
		printf("libc: FAILED: %s passed, but should have failed\n", name);
	else if(exited)
		printf("libc: FAILED: %s exited with %d\n", name, WEXITSTATUS(status));
	else if(WIFSIGNALED(status))
		printf("libc: FAILED: %s was ended by signal %d (%s)\n", name, WTERMSIG(status),
				strsignal(WTERMSIG(status)));
	show_output(name);
	return 0;
}

int main(int argc, char **argv) {
	if(argc > 1) {
		// The test sees itself as the program, with the arguments after
		// its name.
		const char *name = argv[1];
		for(int i = 1; i < argc; i++)
			argv[i] = argv[i + 1];
		for(size_t i = 0; i < TEST_COUNT; i++)
			if(!strcmp(name, TESTS[i].name))
				return TESTS[i].main(argc - 1, argv);
		fprintf(stderr, "run: there is no test %s\n", name);
		return 127;
	}
	setvbuf(stdout, NULL, _IOLBF, 0);
	size_t passed = 0;
	for(size_t i = 0; i < TEST_COUNT; i++)
		passed += run(argv[0], TESTS[i].name);
	unlink(OUTPUT);
	printf("libc: %zu of %zu tests passed\n", passed, (size_t)TEST_COUNT);
	if(passed == TEST_COUNT)
		printf("libc: every check passed\n");
	size_t failed = TEST_COUNT - passed;
	return failed > 125 ? 125 : (int)failed;
}
