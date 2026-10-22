// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// exec: the program, its arguments and environment, what a process keeps
// across it (its number, parent, working directory, descriptors without
// close-on-exec, ignored signals, its mask and pending signals), scripts,
// and how it fails.

#include <fcntl.h>
#include <limits.h>

#include "check.h"

#define HELPER "/system/tests/posix/exec-helper"

static void on_signal(int signal) {
	(void)signal;
}

// Runs exec-helper with `argv` in a child and checks that it passed.
static void run(char *const argv[]) {
	pid_t pid = check_fork();
	if(pid == 0) {
		execv(HELPER, argv);
		_exit(126);
	}
	CHECK_CHILD(pid);
}

static void arguments(void) {
	char *argv[] = {HELPER, "args", "one", "two words", "", NULL};
	run(argv);
	pid_t pid = check_fork();
	if(pid == 0) {
		char *env[] = {"DUKOVO_TEST=yes", "OTHER=1", NULL};
		char *argv[] = {HELPER, "env", NULL};
		execve(HELPER, argv, env);
		_exit(126);
	}
	CHECK_CHILD(pid);
	// execvp searches PATH.
	pid = check_fork();
	if(pid == 0) {
		setenv("PATH", "/nowhere:/system/tests/posix", 1);
		execlp("exec-helper", "exec-helper", "exit", "0", (char *)NULL);
		_exit(126);
	}
	CHECK_CHILD(pid);
	// The exit code comes back through wait.
	pid = check_fork();
	if(pid == 0) {
		execl(HELPER, HELPER, "exit", "42", (char *)NULL);
		_exit(126);
	}
	int status = check_wait(pid);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 42);
}

static void keeps(void) {
	// Its number, parent and working directory.
	pid_t pid = check_fork();
	if(pid == 0) {
		char me[16], parent[16];
		snprintf(me, sizeof(me), "%d", (int)getpid());
		snprintf(parent, sizeof(parent), "%d", (int)getppid());
		CHECK(chdir("/tmp") == 0);
		execl(HELPER, HELPER, "same", me, parent, "/tmp", (char *)NULL);
		_exit(126);
	}
	CHECK_CHILD(pid);
	// Descriptors, but not those with close-on-exec.
	int kept[2], closed[2];
	CHECK(pipe(kept) == 0);
	CHECK(pipe2(closed, O_CLOEXEC) == 0);
	CHECK(fcntl(closed[0], F_GETFD) == FD_CLOEXEC);
	int later = dup(kept[1]);
	CHECK(fcntl(later, F_SETFD, FD_CLOEXEC) == 0);
	pid = check_fork();
	if(pid == 0) {
		char a[16], b[16];
		snprintf(a, sizeof(a), "%d", kept[1]);
		snprintf(b, sizeof(b), "%d", later);
		execl(HELPER, HELPER, "fds", a, b, (char *)NULL);
		_exit(126);
	}
	close(kept[1]);
	close(later);
	CHECK_CHILD(pid);
	char buffer[8] = {0};
	CHECK(read(kept[0], buffer, sizeof(buffer)) == 4 && !strcmp(buffer, "kept"));
	close(kept[0]);
	close(closed[0]);
	close(closed[1]);
	// Ignored signals stay ignored, handled ones go back to the default,
	// and the mask and pending signals stay.
	pid = check_fork();
	if(pid == 0) {
		signal(SIGUSR1, on_signal);
		struct sigaction action = {0};
		action.sa_handler = SIG_IGN;
		sigaction(SIGUSR2, &action, NULL);
		sigset_t mask;
		sigemptyset(&mask);
		sigaddset(&mask, SIGINT);
		sigprocmask(SIG_SETMASK, &mask, NULL);
		raise(SIGINT);
		execl(HELPER, HELPER, "signals", (char *)NULL);
		_exit(126);
	}
	CHECK_CHILD(pid);
}

static void scripts(void) {
	int fd = open("/tmp/script", O_WRONLY | O_CREAT | O_TRUNC, 0755);
	CHECK(fd >= 0);
	const char *text = "#!" HELPER " script\nnot run\n";
	CHECK(write(fd, text, strlen(text)) == (ssize_t)strlen(text));
	close(fd);
	char *argv[] = {"/tmp/script", "extra", NULL};
	pid_t pid = check_fork();
	if(pid == 0) {
		execv("/tmp/script", argv);
		_exit(126);
	}
	CHECK_CHILD(pid);
	unlink("/tmp/script");
}

static void failures(void) {
	char *argv[] = {"x", NULL};
	CHECK_ERROR(execv("/system/tests/posix/nothing", argv), ENOENT);
	CHECK_ERROR(execv("/system/tests", argv), EACCES);
	CHECK_ERROR(execv("/system/tests/posix/exec-helper/x", argv), ENOTDIR);
	int fd = open("/tmp/text", O_WRONLY | O_CREAT | O_TRUNC, 0755);
	CHECK(write(fd, "just text\n", 10) == 10);
	close(fd);
	CHECK_ERROR(execv("/tmp/text", argv), ENOEXEC);
	unlink("/tmp/text");
	// Too much to pass.
	size_t size = 300 * 1024;
	char *huge = malloc(size);
	memset(huge, 'a', size - 1);
	huge[size - 1] = 0;
	char *big[] = {HELPER, huge, NULL};
	CHECK_ERROR(execv(HELPER, big), E2BIG);
	free(huge);
	// The process goes on after a failed exec.
	CHECK(getpid() > 0);
}

int main(void) {
	check_start("exec");
	RUN(arguments);
	RUN(keeps);
	RUN(scripts);
	RUN(failures);
	return check_end();
}
