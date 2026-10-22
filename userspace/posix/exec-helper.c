// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the exec test runs: it checks what it was given, as its first
// argument says, and exits with how many checks failed.

#include <fcntl.h>
#include <limits.h>

#include "check.h"

extern char **environ;

int main(int argc, char **argv) {
	check_start("exec-helper");
	const char *mode = argc > 1 ? argv[1] : "";
	if(!strcmp(mode, "args")) {
		// exec-helper args one "two words" ""
		CHECK(argc == 5);
		CHECK(argc == 5 && !strcmp(argv[2], "one") && !strcmp(argv[3], "two words")
				&& !strcmp(argv[4], ""));
		CHECK(argv[argc] == NULL);
	} else if(!strcmp(mode, "env")) {
		CHECK(getenv("DUKOVO_TEST") && !strcmp(getenv("DUKOVO_TEST"), "yes"));
		int count = 0;
		while(environ[count])
			count++;
		CHECK(count == 2);
	} else if(!strcmp(mode, "fds")) {
		// exec-helper fds <kept> <closed>
		int kept = atoi(argv[2]), closed = atoi(argv[3]);
		CHECK(fcntl(kept, F_GETFD) == 0);
		CHECK_ERROR(fcntl(closed, F_GETFD), EBADF);
		CHECK(write(kept, "kept", 4) == 4);
	} else if(!strcmp(mode, "signals")) {
		struct sigaction action;
		CHECK(sigaction(SIGUSR1, NULL, &action) == 0 && action.sa_handler == SIG_DFL);
		CHECK(sigaction(SIGUSR2, NULL, &action) == 0 && action.sa_handler == SIG_IGN);
		sigset_t mask;
		CHECK(sigprocmask(SIG_BLOCK, NULL, &mask) == 0);
		CHECK(sigismember(&mask, SIGINT) && !sigismember(&mask, SIGUSR1));
		// What was pending stays pending.
		sigset_t pending;
		CHECK(sigpending(&pending) == 0 && sigismember(&pending, SIGINT));
	} else if(!strcmp(mode, "same")) {
		// exec-helper same <pid> <parent> <cwd>
		CHECK(getpid() == atoi(argv[2]));
		CHECK(getppid() == atoi(argv[3]));
		char cwd[PATH_MAX];
		CHECK(getcwd(cwd, sizeof(cwd)) && !strcmp(cwd, argv[4]));
	} else if(!strcmp(mode, "script")) {
		// From "#!/system/tests/posix/exec-helper script" in <path>, run
		// as <path> extra.
		CHECK(argc == 4);
		CHECK(argc == 4 && !strcmp(argv[2], "/tmp/script") && !strcmp(argv[3], "extra"));
	} else if(!strcmp(mode, "spawned")) {
		// From posix_spawn with a group, a mask and default signals.
		CHECK(getpgrp() == getpid());
		sigset_t mask;
		CHECK(sigprocmask(SIG_BLOCK, NULL, &mask) == 0 && sigismember(&mask, SIGINT));
		struct sigaction action;
		CHECK(sigaction(SIGUSR2, NULL, &action) == 0 && action.sa_handler == SIG_DFL);
	} else if(!strcmp(mode, "exit")) {
		return atoi(argv[2]);
	} else {
		CHECK(!"a mode it knows");
	}
	fflush(stdout);
	return check_failures;
}
