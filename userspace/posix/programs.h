// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Running programs as a script does, for the tests of the utilities and
// the administration tools: each with text on its standard input, and
// what it writes and its exit status checked. Each program of
// /system/commands that runs is noted, for the `commands` test.

#ifndef POSIX_PROGRAMS_H
#define POSIX_PROGRAMS_H

#include <fcntl.h>
#include <signal.h>

#include "check.h"

// What a program wrote to its standard output, NUL-terminated.
static char output[16384];

// Notes the program that `len` bytes of `word` name, as `name`,
// `/bin/name` or `/system/commands/name`, if /system/commands has it; returns
// whether it does.
static int note_command(const char *word, size_t len) {
	static const char *const DIRS[] = {"/bin/", "/system/commands/"};
	for(size_t i = 0; i < 2; i++) {
		size_t n = strlen(DIRS[i]);
		if(len > n && !strncmp(word, DIRS[i], n)) {
			word += n;
			len -= n;
			break;
		}
	}
	if(!len || len > 64 || memchr(word, '/', len))
		return 0;
	char path[96], line[72];
	snprintf(path, sizeof(path), "/system/commands/%.*s", (int)len, word);
	if(access(path, X_OK) != 0)
		return 0;
	int fd = open(COMMANDS_RUN, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
	if(fd < 0) {
		check_failed(__FILE__, __LINE__, "opening " COMMANDS_RUN, errno);
		return 1;
	}
	int n = snprintf(line, sizeof(line), "%.*s\n", (int)len, word);
	if(write(fd, line, n) != n)
		check_failed(__FILE__, __LINE__, "noting a command", errno);
	close(fd);
	return 1;
}

// Whether `len` bytes of `word` are one of `words`.
static int one_of(const char *word, size_t len, const char *const words[]) {
	for(size_t i = 0; words[i]; i++)
		if(strlen(words[i]) == len && !strncmp(word, words[i], len))
			return 1;
	return 0;
}

// Notes the programs that shell script `script` runs: the words where a
// command goes (the first, and those after `|`, `;`, `&`, `(`, `{`, `$(`,
// a backquote, a newline or a word such as `then`), and those after a
// program that runs another, such as `xargs`; not an assignment, a
// redirection's file or a builtin, which runs no program.
static void note_script(const char *script) {
	static const char *const STARTS[] = {"if", "then", "else", "elif", "do", "while", "until",
			"!", "exec", "command", "time", NULL};
	static const char *const RUNNERS[] = {"env", "nice", "nohup", "timeout", "xargs", "taskset",
			"ionice", "rtrace", "watch", NULL};
	static const char *const BUILTINS[] = {"[", "echo", "false", "kill", "printf", "pwd", "test",
			"true", NULL};
	int command = 1, runner = 0, file = 0;
	const char *p = script;
	while(*p) {
		if(*p == ' ' || *p == '\t') {
			p++;
		} else if(*p == '<' || *p == '>') {
			// A redirection: its file follows.
			while(*p == '<' || *p == '>' || *p == '&')
				p++;
			file = 1;
		} else if(strchr("|;&(){}`\n", *p) || (*p == '$' && p[1] == '(')) {
			p += *p == '$' ? 2 : 1;
			command = 1;
			runner = 0;
		} else {
			const char *word = p;
			while(*p && !strchr(" \t|;&(){}`\n<>", *p))
				p++;
			size_t len = (size_t)(p - word);
			if(file) {
				file = 0;
			} else if(*p == '<' || *p == '>') {
				// The descriptor of a redirection, as in `2>`.
			} else if(runner) {
				runner = !note_command(word, len);
			} else if(command) {
				if(one_of(word, len, STARTS) || memchr(word, '=', len))
					continue;
				if(!one_of(word, len, BUILTINS))
					note_command(word, len);
				runner = one_of(word, len, RUNNERS);
				command = 0;
			}
		}
	}
}

// Notes the programs that `argv` runs: its own, those it names by their
// path, as `timeout` and `xargs` are given them, and those of a script
// it is given with `-c`.
static void note_commands(const char *const argv[]) {
	for(size_t i = 0; argv[i]; i++) {
		if(i == 0 || !strncmp(argv[i], "/bin/", 5) || !strncmp(argv[i], "/system/commands/", 17))
			note_command(argv[i], strlen(argv[i]));
		else if(!strcmp(argv[i - 1], "-c"))
			note_script(argv[i]);
	}
}

// Runs `argv` with `input` on its standard input, collects what it
// writes to its standard output in `output`, and returns its exit
// status, or 128 and the signal that ended it.
static int run_program(const char *const argv[], const char *input, size_t input_len) {
	note_commands(argv);
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
	// the program's output. A program may end without reading its input,
	// as one that refuses its arguments does, and that is no failure:
	// with two processors it can end before the input is written.
	void (*was)(int) = signal(SIGPIPE, SIG_IGN);
	ssize_t wrote = write(in[1], input, input_len);
	int error = errno;
	signal(SIGPIPE, was);
	if(wrote != (ssize_t)input_len && !(wrote < 0 && error == EPIPE))
		check_failed(__FILE__, __LINE__, "writing the input", error);
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
