// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// posix_spawn: a program started with file actions and attributes, and
// system() and popen(), which use it or fork.

#include <fcntl.h>
#include <spawn.h>

#include "check.h"

extern char **environ;

#define HELPER "/system/tests/posix/exec-helper"

static void spawns(void) {
	pid_t pid;
	char *argv[] = {HELPER, "exit", "3", NULL};
	CHECK(posix_spawn(&pid, HELPER, NULL, NULL, argv, environ) == 0);
	int status = check_wait(pid);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 3);
	// posix_spawnp searches PATH.
	char *argvp[] = {"exec-helper", "exit", "4", NULL};
	setenv("PATH", "/system/tests/posix", 1);
	CHECK(posix_spawnp(&pid, "exec-helper", NULL, NULL, argvp, environ) == 0);
	status = check_wait(pid);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 4);
	// A program that is not there.
	char *none[] = {"none", NULL};
	int failed = posix_spawn(&pid, "/system/tests/posix/none", NULL, NULL, none, environ);
	if(failed == 0) {
		status = check_wait(pid);
		CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 127);
	} else {
		CHECK(failed == ENOENT);
	}
}

// File actions: its standard output is a pipe, and a descriptor closed.
static void actions(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	posix_spawn_file_actions_t actions;
	CHECK(posix_spawn_file_actions_init(&actions) == 0);
	CHECK(posix_spawn_file_actions_adddup2(&actions, fds[1], STDOUT_FILENO) == 0);
	CHECK(posix_spawn_file_actions_addclose(&actions, fds[0]) == 0);
	CHECK(posix_spawn_file_actions_addclose(&actions, fds[1]) == 0);
	pid_t pid;
	char *argv[] = {"/system/tests/posix/hello", NULL};
	CHECK(posix_spawn(&pid, argv[0], &actions, NULL, argv, environ) == 0);
	posix_spawn_file_actions_destroy(&actions);
	close(fds[1]);
	char buffer[256] = {0};
	ssize_t got = 0, n;
	while((n = read(fds[0], buffer + got, sizeof(buffer) - 1 - (size_t)got)) > 0)
		got += n;
	CHECK(!strncmp(buffer, "hello: /system/tests/posix/hello in /", 37));
	CHECK_CHILD(pid);
	close(fds[0]);
}

// Attributes: a process group, a signal mask and default signals.
static void attributes(void) {
	posix_spawnattr_t attr;
	CHECK(posix_spawnattr_init(&attr) == 0);
	CHECK(posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETPGROUP | POSIX_SPAWN_SETSIGMASK
				| POSIX_SPAWN_SETSIGDEF) == 0);
	CHECK(posix_spawnattr_setpgroup(&attr, 0) == 0);
	sigset_t mask, defaults;
	sigemptyset(&mask);
	sigaddset(&mask, SIGINT);
	CHECK(posix_spawnattr_setsigmask(&attr, &mask) == 0);
	sigemptyset(&defaults);
	sigaddset(&defaults, SIGUSR2);
	CHECK(posix_spawnattr_setsigdefault(&attr, &defaults) == 0);
	// SIGUSR2, ignored here, is back to its default in the child.
	signal(SIGUSR2, SIG_IGN);
	pid_t pid;
	char *argv[] = {HELPER, "spawned", NULL};
	CHECK(posix_spawn(&pid, HELPER, NULL, &attr, argv, environ) == 0);
	CHECK(getpgid(pid) == pid || errno == ESRCH);
	CHECK_CHILD(pid);
	signal(SIGUSR2, SIG_DFL);
	posix_spawnattr_destroy(&attr);
}

// popen and system run their command with /bin/sh, which comes with
// Nukesh in Phase 9: until then there is nothing to check.
static void shells(void) {
	if(access("/bin/sh", X_OK) != 0)
		return;
	CHECK(system(NULL) != 0);
	// popen reads a program's output.
	FILE *pipe = popen("/system/tests/posix/hello", "r");
	CHECK(pipe != NULL);
	if(pipe) {
		char line[256] = {0};
		CHECK(fgets(line, sizeof(line), pipe) != NULL);
		CHECK(!strncmp(line, "hello: ", 7));
		CHECK(pclose(pipe) == 0);
	}
	int status = system("exit 3");
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 3);
}

int main(void) {
	check_start("spawn");
	RUN(spawns);
	RUN(actions);
	RUN(attributes);
	RUN(shells);
	return check_end();
}
