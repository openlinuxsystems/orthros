// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The runner of the C test programs (the `posix` boot test): Spark
// starts it on a pseudo-terminal; it makes a session of its own with
// that terminal as the controlling one, then runs each test (those its
// arguments name, or all of them) as a shell runs a job: in a process
// group of its own, in the foreground, with fork and exec. It prints
// "posix: <test> passed" or "FAILED" for each, and exits with 0 only if
// every test passed (radix-posix(7)).

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

// Every test, in the order they run.
static const char *const TESTS[] = {
		"hello", "fork", "exec", "signal", "pipe", "poll", "tty", "job", "time", "files",
		"thread", "spawn",
};

// Runs test `name` as a foreground job; returns whether it passed.
static int run(const char *name) {
	char path[256];
	snprintf(path, sizeof(path), DIRECTORY "%s", name);
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
		char *argv[] = {path, NULL};
		execv(path, argv);
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
	int count = argc > 1 ? argc - 1 : (int)(sizeof(TESTS) / sizeof(TESTS[0]));
	int passed = 0;
	for(int i = 0; i < count; i++)
		passed += run(argc > 1 ? argv[i + 1] : TESTS[i]);
	printf("posix: %d of %d tests passed\n", passed, count);
	return passed == count ? 0 : 1;
}
