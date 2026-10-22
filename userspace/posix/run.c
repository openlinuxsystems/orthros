// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The runner of the C test programs (the `posix` boot test): Spark
// starts it on a pseudo-terminal; it makes a session of its own with
// that terminal as the controlling one, then runs each test (those its
// arguments name, or all of them) as a shell runs a job: in a process
// group of its own, in the foreground, with fork and exec. The tests are
// the programs of this directory, the test of Rust's standard library
// (std-test, userspace/programs), the C library's own tests (libc) and
// Lua's test suite (lua); libc:<name> runs only the C library's test
// <name>, with its output as it goes. It prints "posix: <test> passed" or
// "FAILED" for each, and exits with 0 only if every test passed
// (radix-posix(7)).

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

// Where the tests are.
#define DIRECTORY "/system/tests/posix/"

// A test.
struct test {
	const char *name;
	// The directory it runs in; NULL for the runner's.
	const char *dir;
	// Its program and arguments; NULL for the program DIRECTORY<name>.
	const char *const *argv;
};

// Lua's own suite, as its all.lua runs it for users (_U): without the
// tests that need a shell or C modules, or take long (ports/lua).
static const char *const LUA[] = {"/system/tests/lua/lua", "-e", "_U=true", "all.lua", NULL};

// The C library's own tests, in one program (ports/mlibc-tests).
static const char *const LIBC[] = {"/system/tests/libc/run", NULL};

// Every test, in the order they run.
static const struct test TESTS[] = {
		{"hello", NULL, NULL},
		{"fork", NULL, NULL},
		{"exec", NULL, NULL},
		{"signal", NULL, NULL},
		{"pipe", NULL, NULL},
		{"poll", NULL, NULL},
		{"tty", NULL, NULL},
		{"job", NULL, NULL},
		{"time", NULL, NULL},
		{"files", NULL, NULL},
		{"thread", NULL, NULL},
		{"spawn", NULL, NULL},
		{"limits", NULL, NULL},
		{"format", NULL, NULL},
		{"std-test", NULL, NULL},
		{"libc", NULL, LIBC},
		{"lua", "/system/tests/lua/testes", LUA},
};

enum { COUNT = sizeof(TESTS) / sizeof(TESTS[0]) };

// Runs `test` as a foreground job; returns whether it passed.
static int run(const struct test *test) {
	const char *name = test->name;
	char path[256];
	snprintf(path, sizeof(path), DIRECTORY "%s", name);
	char *alone[] = {path, NULL};
	char *const *argv = test->argv ? (char *const *)test->argv : alone;
	fflush(stdout);
	pid_t pid = fork();
	if(pid < 0) {
		printf("posix: %s: fork failed: %s\n", name, strerror(errno));
		return 0;
	}
	if(pid == 0) {
		// As a shell's child: its own group, in the foreground, with the
		// signals the shell ignores back to their defaults.
		setpgid(0, 0);
		tcsetpgrp(STDIN_FILENO, getpid());
		signal(SIGTTOU, SIG_DFL);
		signal(SIGTTIN, SIG_DFL);
		signal(SIGTSTP, SIG_DFL);
		if(test->dir && chdir(test->dir) < 0) {
			printf("posix: %s: cannot go to %s: %s\n", name, test->dir, strerror(errno));
			_exit(127);
		}
		execv(argv[0], argv);
		printf("posix: %s: exec failed: %s\n", name, strerror(errno));
		_exit(127);
	}
	// Both set the group, so that it is set whichever runs first.
	setpgid(pid, pid);
	tcsetpgrp(STDIN_FILENO, pid);
	int status;
	pid_t got;
	do {
		got = waitpid(pid, &status, WUNTRACED);
	} while(got < 0 && errno == EINTR);
	tcsetpgrp(STDIN_FILENO, getpgrp());
	if(got != pid) {
		printf("posix: %s: waitpid failed: %s\n", name, strerror(errno));
		return 0;
	}
	if(WIFEXITED(status) && WEXITSTATUS(status) == 0) {
		printf("posix: %s passed\n", name);
		return 1;
	}
	if(WIFEXITED(status))
		printf("posix: %s FAILED: it exited with %d\n", name, WEXITSTATUS(status));
	else if(WIFSIGNALED(status))
		printf("posix: %s FAILED: signal %d (%s) ended it\n", name, WTERMSIG(status),
				strsignal(WTERMSIG(status)));
	else if(WIFSTOPPED(status)) {
		printf("posix: %s FAILED: signal %d stopped it\n", name, WSTOPSIG(status));
		kill(pid, SIGKILL);
		waitpid(pid, &status, 0);
	}
	return 0;
}

int main(int argc, char **argv) {
	setvbuf(stdout, NULL, _IOLBF, 0);
	// A session of its own, whose controlling terminal is the one Spark
	// gave it, and the job control signals a shell ignores.
	if(setsid() < 0 || ioctl(STDIN_FILENO, TIOCSCTTY, 0) < 0) {
		printf("posix: cannot take the terminal: %s\n", strerror(errno));
		return 1;
	}
	signal(SIGTTOU, SIG_IGN);
	signal(SIGTTIN, SIG_IGN);
	signal(SIGTSTP, SIG_IGN);
	int count = argc > 1 ? argc - 1 : COUNT;
	int passed = 0;
	for(int i = 0; i < count; i++) {
		const struct test *test = &TESTS[i];
		const char *one[] = {LIBC[0], NULL, NULL};
		struct test libc_one = {NULL, NULL, one};
		if(argc > 1 && !strncmp(argv[i + 1], "libc:", 5)) {
			libc_one.name = argv[i + 1];
			one[1] = argv[i + 1] + 5;
			test = &libc_one;
		} else if(argc > 1) {
			test = NULL;
			for(int j = 0; j < COUNT && !test; j++)
				if(!strcmp(TESTS[j].name, argv[i + 1]))
					test = &TESTS[j];
		}
		if(test)
			passed += run(test);
		else
			printf("posix: %s FAILED: there is no such test\n", argv[i + 1]);
	}
	printf("posix: %d of %d tests passed\n", passed, count);
	return passed == count ? 0 : 1;
}
