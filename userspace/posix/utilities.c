// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The ported utilities as a script uses them: each run through the links
// in /bin, with text on its standard input, and what it writes and its
// exit status checked. uutils' core utilities (ports/uutils-coreutils),
// find and xargs (ports/uutils-findutils), GNU grep, sed, diffutils and
// patch, awk, bc, less, ncurses' tools and man (ports/<name>), and their
// manual pages.

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

// grep, sed and awk.
static void searching(void) {
	OUTPUT("a\nb\nab\n", "a\nab\n", "/bin/grep", "a");
	OUTPUT("A\nb\n", "A\n", "/bin/grep", "-i", "a");
	OUTPUT("a\nb\nab\n", "b\n", "/bin/grep", "-v", "a");
	OUTPUT("a\nb\nab\n", "2\n", "/bin/grep", "-c", "a");
	OUTPUT("x1\nfoo22\n", "2:foo22\n", "/bin/grep", "-n", "-E", "o{2}[0-9]{2}$");
	OUTPUT("a.b\naxb\n", "a.b\n", "/bin/grep", "-F", "a.b");
	OUTPUT("cat category\n", "cat\n", "/bin/grep", "-o", "-w", "cat");
	STATUS(1, "/bin/grep", "x");
	STATUS(2, "/bin/grep", "x", "/nowhere");
	// egrep is a script, which /bin/sh runs.
	OUTPUT("a\nb\nc\n", "a\nc\n", "/bin/egrep", "a|c");
	STATUS(0, "/bin/mkdir", "-p", "/tmp/search/d");
	OUTPUT("needle\n", "needle\n", "/bin/tee", "/tmp/search/d/file");
	OUTPUT("", "/tmp/search/d/file:needle\n", "/bin/grep", "-r", "needle", "/tmp/search");

	OUTPUT("hello\n", "hallo\n", "/bin/sed", "s/e/a/");
	OUTPUT("1\n2\n3\n", "2\n", "/bin/sed", "-n", "2p");
	OUTPUT("a b\n", "b a\n", "/bin/sed", "-E", "s/(a) (b)/\\2 \\1/");
	OUTPUT("a\nb\nc\n", "a\nc\n", "/bin/sed", "/b/d");
	OUTPUT("x\n", "x\ny\n", "/bin/sed", "a\\\ny");
	STATUS(0, "/bin/sed", "-i", "s/needle/thread/", "/tmp/search/d/file");
	OUTPUT("", "thread\n", "/bin/cat", "/tmp/search/d/file");

	OUTPUT("a 1\nb 2\n", "3\n", "/bin/awk", "{ s += $2 } END { print s }");
	OUTPUT("a:b\n", "b\n", "/bin/awk", "-F:", "{ print $2 }");
	OUTPUT("", "3.14\n", "/bin/awk", "BEGIN { printf \"%.2f\\n\", 3.14159 }");
	OUTPUT("dukovo os\n", "DUKOVO 6\n", "/bin/awk", "{ print toupper($1), length($1) }");
	OUTPUT("x=1\ny=2\n", "y\n", "/bin/awk", "-F=", "$2 > 1 { print $1 }");
	OUTPUT("", "3\n", "/bin/awk", "BEGIN { n = split(\"a,b,c\", parts, \",\"); print n }");
	STATUS(2, "/bin/awk", "BEGIN { exit 2 }");
	STATUS(0, "/bin/rm", "-r", "/tmp/search");
}

// diff, cmp, diff3, sdiff and patch, on files and trees.
static void comparing(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/compare/a", "/tmp/compare/b");
	OUTPUT("one\ntwo\nthree\n", "one\ntwo\nthree\n", "/bin/tee", "/tmp/compare/a/f");
	OUTPUT("one\n2\nthree\n", "one\n2\nthree\n", "/bin/tee", "/tmp/compare/b/f");
	OUTPUT("one\n  two\nthree\n", "one\n  two\nthree\n", "/bin/tee", "/tmp/compare/spaced");
	STATUS(0, "/bin/diff", "/tmp/compare/a/f", "/tmp/compare/a/f");
	RUN_WITH(1, "", "2c2\n< two\n---\n> 2\n", 0, "/bin/diff", "/tmp/compare/a/f",
			"/tmp/compare/b/f");
	RUN_WITH(1, "", "@@ -1,3 +1,3 @@\n one\n-two\n+2\n three\n", 1, "/bin/diff", "-u",
			"/tmp/compare/a/f", "/tmp/compare/b/f");
	STATUS(0, "/bin/diff", "-w", "/tmp/compare/a/f", "/tmp/compare/spaced");
	RUN_WITH(1, "", "diff -r /tmp/compare/a/f /tmp/compare/b/f\n", 1, "/bin/diff", "-r",
			"/tmp/compare/a", "/tmp/compare/b");
	STATUS(0, "/bin/cmp", "-s", "/tmp/compare/a/f", "/tmp/compare/a/f");
	STATUS(1, "/bin/cmp", "-s", "/tmp/compare/a/f", "/tmp/compare/b/f");
	STATUS(0, "/bin/diff3", "/tmp/compare/a/f", "/tmp/compare/a/f", "/tmp/compare/a/f");
	STATUS(1, "/bin/sdiff", "-s", "/tmp/compare/a/f", "/tmp/compare/b/f");

	// A patch made by diff, applied, and taken back.
	STATUS(1, "/bin/sh", "-c",
			"diff -u /tmp/compare/a/f /tmp/compare/b/f >/tmp/compare/change.diff");
	STATUS(0, "/bin/patch", "-s", "/tmp/compare/a/f", "/tmp/compare/change.diff");
	STATUS(0, "/bin/cmp", "/tmp/compare/a/f", "/tmp/compare/b/f");
	STATUS(0, "/bin/patch", "-s", "-R", "/tmp/compare/a/f", "/tmp/compare/change.diff");
	OUTPUT("", "one\ntwo\nthree\n", "/bin/cat", "/tmp/compare/a/f");
	// git's patches name files with a/ and b/, and rename them.
	OUTPUT("diff --git a/f b/g\nsimilarity index 100%\nrename from f\nrename to g\n", "",
			"/bin/patch", "-s", "-p1", "-d", "/tmp/compare/b");
	OUTPUT("", "g\n", "/bin/ls", "/tmp/compare/b");
	STATUS(0, "/bin/rm", "-r", "/tmp/compare");
}

// bc, dc, less, the terminal descriptions and man.
static void others(void) {
	OUTPUT("2^100\n", "1267650600228229401496703205376\n", "/bin/bc");
	OUTPUT("scale=10; sqrt(2)\n", "1.4142135623\n", "/bin/bc");
	OUTPUT("define f(n) { if (n < 2) return (n); return (f(n - 1) + f(n - 2)); }\nf(20)\n",
			"6765\n", "/bin/bc");
	OUTPUT("2 3 + p\n", "5\n", "/bin/dc");
	// less shows its input unchanged when its output is not a terminal.
	OUTPUT("line\n", "line\n", "/bin/less");
	OUTPUT("", "80\n", "/bin/tput", "-T", "xterm", "cols");
	// xterm's description also clears the lines scrolled off the screen.
	OUTPUT("", "\033[H\033[2J\033[3J", "/bin/tput", "-T", "xterm", "clear");
	STATUS(0, "/bin/tput", "-T", "tmux-256color", "colors");
	CONTAINS("cols#80,", "/bin/infocmp", "-1", "vt100");
	STATUS(3, "/bin/tput", "-T", "no-such-terminal", "cols");
	OUTPUT("", "/system/share/man/man1/grep.1\n", "/bin/man", "-w", "grep");
	// The pages are longer than what this collects, so a script reads
	// their start, where their NAME section is.
	CONTAINS("print lines that match patterns", "/bin/sh", "-c", "man grep | head -n 8");
	CONTAINS("stream editor", "/bin/sh", "-c", "man 1 sed | head -n 8");
	CONTAINS("terminal capability database", "/bin/sh", "-c", "man 5 terminfo | head -n 8");
}

// Every utility has its manual page.
static void manuals(void) {
	const char *const pages[] = {"ls", "cat", "sort", "test", "[", "find", "xargs", "grep", "sed",
			"awk", "diff", "cmp", "diff3", "sdiff", "patch", "bc", "dc", "less", "man", "mandoc",
			"apropos", "tput", "tset", "clear"};
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
	RUN(searching);
	RUN(comparing);
	RUN(others);
	RUN(manuals);
	return check_end();
}
