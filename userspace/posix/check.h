// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What every C test program of userspace/posix uses: CHECK, which
// reports a check that failed and goes on, and the helpers the tests
// share. A test program prints "<name>: every check passed" and exits
// with 0, or exits with how many checks failed (radix-posix(7)).

#ifndef POSIX_CHECK_H
#define POSIX_CHECK_H

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// Where the tests that run programs (programs.h) note each program of
// /system/bin they run, a name a line: the `commands` test checks that
// every program there was run.
#define COMMANDS_RUN "/tmp/posix-commands"

// The test's name, which every line it prints starts with.
static const char *check_name = "posix";
// How many checks have failed in this process.
static int check_failures;

static inline void check_failed(const char *file, int line, const char *what, int error) {
	printf("%s: FAILED: %s:%d: %s (errno %d: %s)\n", check_name, file, line, what, error,
			strerror(error));
	fflush(stdout);
	check_failures++;
}

// Checks that `condition` holds; if not, says so, with errno as it was.
#define CHECK(condition) \
	do { \
		int check_errno_ = errno; \
		if(!(condition)) \
			check_failed(__FILE__, __LINE__, #condition, check_errno_); \
	} while(0)

// Checks that `call` failed with -1 and errno `error`.
#define CHECK_ERROR(call, error) \
	do { \
		errno = 0; \
		long check_result_ = (long)(call); \
		int check_errno_ = errno; \
		if(check_result_ != -1 || check_errno_ != (error)) \
			check_failed(__FILE__, __LINE__, #call " fails with " #error, check_errno_); \
	} while(0)

// Runs part `part` of the test, saying so first, so that a test that
// hangs shows where.
#define RUN(part) \
	do { \
		printf("%s: %s\n", check_name, #part); \
		part(); \
	} while(0)

// Starts the test called `name`.
static inline void check_start(const char *name) {
	check_name = name;
	// Each line whole, even when the test's children print too.
	setvbuf(stdout, NULL, _IOLBF, 0);
}

// Ends the test: says how it went, and returns its exit code.
static inline int check_end(void) {
	if(check_failures)
		printf("%s: %d checks failed\n", check_name, check_failures);
	else
		printf("%s: every check passed\n", check_name);
	fflush(stdout);
	return check_failures > 125 ? 125 : check_failures;
}

// Ends a child the test forked, with how many of its checks failed.
static inline void check_exit_child(void) {
	fflush(stdout);
	_exit(check_failures > 125 ? 125 : check_failures);
}

// fork(), with stdout flushed first so that the child does not print
// what the parent had buffered. The child counts only its own failures.
static inline pid_t check_fork(void) {
	fflush(stdout);
	pid_t pid = fork();
	if(pid < 0)
		check_failed(__FILE__, __LINE__, "fork()", errno);
	if(pid == 0)
		check_failures = 0;
	return pid;
}

// Waits for child `pid` and returns its wait status (-1 if waiting fails).
static inline int check_wait(pid_t pid) {
	int status;
	pid_t got;
	do {
		got = waitpid(pid, &status, 0);
	} while(got < 0 && errno == EINTR);
	if(got != pid) {
		check_failed(__FILE__, __LINE__, "waitpid(pid, &status, 0) == pid", errno);
		return -1;
	}
	return status;
}

// Checks that the child `pid` exited with 0: no check of its failed.
#define CHECK_CHILD(pid) \
	do { \
		int check_status_ = check_wait(pid); \
		if(!WIFEXITED(check_status_) || WEXITSTATUS(check_status_) != 0) { \
			printf("%s: child %d ended with status %#x\n", check_name, (int)(pid), \
					check_status_); \
			check_failed(__FILE__, __LINE__, "the child passed", 0); \
		} \
	} while(0)

// Nanoseconds on the monotonic clock.
static inline long long check_now_ns(void) {
	struct timespec now;
	clock_gettime(CLOCK_MONOTONIC, &now);
	return (long long)now.tv_sec * 1000000000LL + now.tv_nsec;
}

// Sleeps for `ms` milliseconds.
static inline void check_sleep_ms(long ms) {
	struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
	while(nanosleep(&ts, &ts) < 0 && errno == EINTR) {
	}
}

#endif
