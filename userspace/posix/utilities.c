// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The ported utilities as a script uses them: each run through the links
// in /bin, with text on its standard input, and what it writes and its
// exit status checked. uutils' core utilities (ports/uutils-coreutils),
// find and xargs (ports/uutils-findutils), and their manual pages.

#include <sys/stat.h>

#include "check.h"

// What a program wrote to its standard output, NUL-terminated.
static char output[8192];

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

// Text: the filters of a pipeline.
static void text(void) {
	OUTPUT("", "hello world\n", "/bin/echo", "hello", "world");
	OUTPUT("abc\n", "abc\n", "/bin/cat");
	OUTPUT("b\na\nc\n", "a\nb\nc\n", "/bin/sort");
	OUTPUT("10\n9\n100\n", "9\n10\n100\n", "/bin/sort", "-n");
	OUTPUT("a\na\nb\n", "a\nb\n", "/bin/uniq");
	OUTPUT("1\n2\n3\n", "3\n", "/bin/wc", "-l");
	OUTPUT("dukovo\n", "DUKOVO\n", "/bin/tr", "a-z", "A-Z");
	OUTPUT("1\n2\n3\n", "1\n2\n", "/bin/head", "-n", "2");
	OUTPUT("1\n2\n3\n", "3\n", "/bin/tail", "-n", "1");
	OUTPUT("a:b:c\n", "b\n", "/bin/cut", "-d:", "-f2");
	OUTPUT("1\n2\n", "2\n1\n", "/bin/tac");
	OUTPUT("a\tb\n", "a       b\n", "/bin/expand");
	OUTPUT("", "1\n2\n3\n", "/bin/seq", "3");
	OUTPUT("", "00042", "/bin/printf", "%05d", "42");
	OUTPUT("AB", " 41 42\n", "/bin/od", "-An", "-tx1");
	OUTPUT("hi", "aGk=\n", "/bin/base64");
	OUTPUT("aGk=\n", "hi", "/bin/base64", "-d");
	OUTPUT("abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  -\n",
			"/bin/sha256sum");
	OUTPUT("abc", "900150983cd24fb0d6963f7d28e17f72  -\n", "/bin/md5sum");
	OUTPUT("", "5\n", "/bin/expr", "2", "+", "3");
	OUTPUT("", "12: 2 2 3\n", "/bin/factor", "12");
	OUTPUT("", "c\n", "/bin/basename", "/a/b/c.txt", ".txt");
	OUTPUT("", "/a/b\n", "/bin/dirname", "/a/b/c.txt");
	OUTPUT("", "1970\n", "/bin/date", "-u", "-d", "@0", "+%Y");
	OUTPUT("", "Dukovo\n", "/bin/uname", "-s");
}

// Exit statuses: test and its other name, true and false, timeout.
static void statuses(void) {
	STATUS(0, "/bin/true");
	STATUS(1, "/bin/false");
	STATUS(0, "/bin/test", "-d", "/tmp");
	STATUS(0, "/bin/test", "-L", "/bin");
	STATUS(1, "/bin/test", "-f", "/nowhere");
	STATUS(0, "/bin/[", "1", "-lt", "2", "]");
	STATUS(1, "/bin/[", "b", "=", "a", "]");
	STATUS(0, "/bin/sleep", "0.05");
	STATUS(124, "/bin/timeout", "0.2", "/bin/sleep", "5");
	OUTPUT("", "bar\n", "/bin/env", "FOO=bar", "/bin/printenv", "FOO");
}

// Files: making, linking, copying, moving, listing and removing them.
static void files(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/utilities/a/b");
	STATUS(0, "/bin/touch", "/tmp/utilities/a/b/file");
	OUTPUT("text\n", "text\n", "/bin/tee", "/tmp/utilities/a/text");
	OUTPUT("", "text\n", "/bin/cat", "/tmp/utilities/a/text");
	STATUS(0, "/bin/cp", "/tmp/utilities/a/text", "/tmp/utilities/copy");
	STATUS(0, "/bin/mv", "/tmp/utilities/copy", "/tmp/utilities/moved");
	STATUS(0, "/bin/ln", "-s", "a/b", "/tmp/utilities/link");
	OUTPUT("", "a/b\n", "/bin/readlink", "/tmp/utilities/link");
	OUTPUT("", "/tmp/utilities/a/b\n", "/bin/realpath", "/tmp/utilities/link");
	OUTPUT("", "symbolic link\n", "/bin/stat", "-c", "%F", "/tmp/utilities/link");
	OUTPUT("", "5\n", "/bin/stat", "-c", "%s", "/tmp/utilities/moved");
	OUTPUT("", "a\nlink\nmoved\n", "/bin/ls", "/tmp/utilities");
	CONTAINS("-> a/b", "/bin/ls", "-l", "/tmp/utilities");
	STATUS(0, "/bin/cp", "-r", "/tmp/utilities/a", "/tmp/utilities/tree");
	OUTPUT("", "/tmp/utilities/tree/b/file\n", "/bin/ls", "-d", "/tmp/utilities/tree/b/file");
	CONTAINS("/tmp/utilities\n", "/bin/du", "-s", "/tmp/utilities");
	STATUS(0, "/bin/rm", "-r", "/tmp/utilities");
	STATUS(2, "/bin/ls", "/tmp/utilities");
	// Spark's links into /system, and what ls and realpath make of them.
	OUTPUT("", "system\n", "/bin/readlink", "/usr");
	OUTPUT("", "/system/bin\n", "/bin/realpath", "/usr/bin");
	CONTAINS("-> system/bin", "/bin/ls", "-ld", "/bin");
	CONTAINS("coreutils\n", "/bin/ls", "/system/bin");
}

// find and xargs.
static void finding(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/find/a/b");
	STATUS(0, "/bin/touch", "/tmp/find/a/one.txt", "/tmp/find/a/b/two.txt", "/tmp/find/c");
	STATUS(0, "/bin/ln", "-s", "a", "/tmp/find/l");
	OUTPUT("", "/tmp/find/a/b/two.txt\n/tmp/find/a/one.txt\n", "/bin/find", "/tmp/find",
			"-sorted", "-name", "*.txt");
	OUTPUT("", "/tmp/find/l\n", "/bin/find", "/tmp/find", "-type", "l");
	OUTPUT("", "/tmp/find/a\n/tmp/find/c\n/tmp/find/l\n", "/bin/find", "/tmp/find", "-sorted",
			"-mindepth", "1", "-maxdepth", "1");
	OUTPUT("", "/tmp/find/l/b\n", "/bin/find", "-L", "/tmp/find/l", "-type", "d", "-name", "b");
	OUTPUT("", "c\n", "/bin/find", "/tmp/find", "-name", "c", "-printf", "%f\\n");
	OUTPUT("", "/tmp/find/c\n", "/bin/find", "/tmp/find", "-name", "c", "-exec", "/bin/echo",
			"{}", ";");
	OUTPUT("", "/proc\n", "/bin/find", "/proc", "-maxdepth", "0", "-fstype", "proc");
	OUTPUT("", "/tmp\n", "/bin/find", "/tmp", "-maxdepth", "0", "-fstype", "tmpfs");
	STATUS(0, "/bin/find", "/tmp/find", "-name", "*.txt", "-delete");
	OUTPUT("", "", "/bin/find", "/tmp/find", "-name", "*.txt");
	STATUS(1, "/bin/find", "/nowhere");
	OUTPUT("1 2 3", "1\n2\n3\n", "/bin/xargs", "-n", "1", "/bin/echo");
	OUTPUT("a b\0c\0", "a b c\n", "/bin/xargs", "-0", "/bin/echo");
	OUTPUT("x\ny\n", "<x>\n<y>\n", "/bin/xargs", "-I", "{}", "/bin/echo", "<{}>");
	OUTPUT("", "", "/bin/xargs", "-r", "/bin/echo", "never");
	STATUS(0, "/bin/rm", "-r", "/tmp/find");
}

// Every utility has its manual page.
static void manuals(void) {
	const char *const pages[] = {"ls", "cat", "sort", "test", "[", "find", "xargs"};
	for(size_t i = 0; i < sizeof(pages) / sizeof(pages[0]); i++) {
		char path[64];
		snprintf(path, sizeof(path), "/usr/share/man/man1/%s.1", pages[i]);
		struct stat st;
		CHECK(stat(path, &st) == 0 && S_ISREG(st.st_mode) && st.st_size > 0);
	}
}

int main(void) {
	check_start("utilities");
	RUN(text);
	RUN(statuses);
	RUN(files);
	RUN(finding);
	RUN(manuals);
	return check_end();
}
