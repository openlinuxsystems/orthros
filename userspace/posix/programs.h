// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Running programs as a script does, for the tests of the utilities and
// the administration tools: each with text on its standard input, and
// what it writes and its exit status checked.

#ifndef POSIX_PROGRAMS_H
#define POSIX_PROGRAMS_H

#include "check.h"

// What a program wrote to its standard output, NUL-terminated.
static char output[16384];

// Runs `argv` with `input` on its standard input, collects what it
// writes to its standard output in `output`, and returns its exit
// status, or 128 and the signal that ended it.
static int run_program(const char *const argv[], const char *input, size_t input_len) {
	int in[2], out[2];
	if(pipe(in) || pipe(out))
		return -1;
	pid_t pid = fork();
	if(pid < 0)
		return -1;
	if(!pid) {
		dup2(in[0], STDIN_FILENO);
		dup2(out[1], STDOUT_FILENO);
		close(in[0]);
		close(in[1]);
		close(out[0]);
		close(out[1]);
		execv(argv[0], (char *const *)argv);
		_exit(127);
	}
	close(in[0]);
	close(out[1]);
	// The inputs are smaller than a pipe holds, so this never waits on
	// the program's output.
	if(write(in[1], input, input_len) != (ssize_t)input_len)
		check_failed(__FILE__, __LINE__, "writing the input", errno);
	close(in[1]);
	size_t len = 0;
	ssize_t got;
	while(len < sizeof(output) - 1 && (got = read(out[0], output + len, sizeof(output) - 1 - len)) > 0)
		len += got;
	output[len] = 0;
	close(out[0]);
	int status;
	if(waitpid(pid, &status, 0) != pid)
		return -1;
	return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}

// Checks that `argv` given `input` exits with `status` and writes
// `expected`, or something that holds `expected` when `part` is set.
static void expect(const char *file, int line, int status, const char *input, size_t input_len,
		const char *expected, int part, const char *const argv[]) {
	int got = run_program(argv, input, input_len);
	int matched = part ? strstr(output, expected) != NULL : !strcmp(output, expected);
	if(got != status || !matched) {
		char what[512];
		snprintf(what, sizeof(what), "%s exited with %d and wrote \"%.300s\"", argv[0], got, output);
		check_failed(file, line, what, 0);
	}
}

// The program and arguments that follow, given `input`, exit with 0 and
// write exactly `expected` (OUTPUT), or something holding it (CONTAINS),
// or exit with `status` (STATUS).
#define RUN_WITH(status, input, expected, part, ...) \
	expect(__FILE__, __LINE__, status, input, sizeof(input) - 1, expected, part, \
			(const char *const[]){__VA_ARGS__, NULL})
#define OUTPUT(input, expected, ...) RUN_WITH(0, input, expected, 0, __VA_ARGS__)
#define CONTAINS(expected, ...) RUN_WITH(0, "", expected, 1, __VA_ARGS__)
#define STATUS(status, ...) RUN_WITH(status, "", "", 1, __VA_ARGS__)

#endif
