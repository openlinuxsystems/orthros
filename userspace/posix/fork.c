// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// fork and wait: the child is a copy whose writes the parent does not
// see, and the parent waits for its children in every way waitpid has.

#include <fcntl.h>
#include <stdint.h>
#include <sys/mman.h>

#include "check.h"

static int global = 1;

// The child gets a copy of the parent's memory: what it writes, the
// parent does not see.
static void copies(void) {
	int local = 2;
	int *heap = malloc(sizeof(int));
	*heap = 3;
	size_t big = 4 << 20;
	unsigned char *pattern = malloc(big);
	CHECK(pattern != NULL);
	for(size_t i = 0; i < big; i++)
		pattern[i] = (unsigned char)(i * 7);
	pid_t parent = getpid();
	pid_t pid = check_fork();
	if(pid == 0) {
		CHECK(getppid() == parent);
		CHECK(getpid() != parent);
		CHECK(global == 1 && local == 2 && *heap == 3);
		size_t wrong = 0;
		for(size_t i = 0; i < big; i++)
			wrong += pattern[i] != (unsigned char)(i * 7);
		CHECK(wrong == 0);
		global = 10;
		local = 20;
		*heap = 30;
		memset(pattern, 0xee, big);
		CHECK(global == 10 && local == 20 && *heap == 30 && pattern[big - 1] == 0xee);
		check_exit_child();
	}
	CHECK(pid > 0);
	CHECK_CHILD(pid);
	CHECK(global == 1 && local == 2 && *heap == 3);
	size_t wrong = 0;
	for(size_t i = 0; i < big; i++)
		wrong += pattern[i] != (unsigned char)(i * 7);
	CHECK(wrong == 0);
	free(pattern);
	free(heap);
}

// Shared memory stays shared: a MAP_SHARED anonymous mapping is the same
// memory in both.
static void shares(void) {
	volatile int *shared =
			mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	CHECK(shared != MAP_FAILED);
	if(shared == MAP_FAILED)
		return;
	*shared = 1;
	pid_t pid = check_fork();
	if(pid == 0) {
		*shared = 2;
		check_exit_child();
	}
	CHECK_CHILD(pid);
	CHECK(*shared == 2);
	munmap((void *)shared, 4096);
}

// Exit codes and wait statuses.
static void statuses(void) {
	for(int code = 0; code < 256; code += 51) {
		pid_t pid = check_fork();
		if(pid == 0)
			_exit(code);
		int status = check_wait(pid);
		CHECK(WIFEXITED(status) && WEXITSTATUS(status) == code);
	}
	// exit() runs the atexit handlers and flushes stdio; _exit does not.
	int fds[2];
	CHECK(pipe(fds) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		close(fds[0]);
		dup2(fds[1], STDOUT_FILENO);
		setvbuf(stdout, NULL, _IOFBF, 4096);
		fputs("flushed at exit", stdout);
		exit(7);
	}
	close(fds[1]);
	char buffer[64] = {0};
	ssize_t n = 0, got;
	while((got = read(fds[0], buffer + n, sizeof(buffer) - 1 - n)) > 0)
		n += got;
	close(fds[0]);
	CHECK(strcmp(buffer, "flushed at exit") == 0);
	int status = check_wait(pid);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 7);
}

// waitpid for any child, for none, and without waiting.
static void waits(void) {
	enum { CHILDREN = 8 };
	pid_t pids[CHILDREN];
	for(int i = 0; i < CHILDREN; i++) {
		pids[i] = check_fork();
		if(pids[i] == 0)
			_exit(100 + i);
	}
	int seen = 0;
	for(int i = 0; i < CHILDREN; i++) {
		int status;
		pid_t got = waitpid(-1, &status, 0);
		for(int j = 0; j < CHILDREN; j++) {
			if(got == pids[j] && WIFEXITED(status) && WEXITSTATUS(status) == 100 + j)
				seen |= 1 << j;
		}
	}
	CHECK(seen == (1 << CHILDREN) - 1);
	CHECK_ERROR(waitpid(-1, NULL, 0), ECHILD);
	CHECK_ERROR(waitpid(-1, NULL, WNOHANG), ECHILD);
	CHECK_ERROR(wait(NULL), ECHILD);
	// A child still running: WNOHANG returns 0 until it ends.
	int fds[2];
	CHECK(pipe(fds) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		close(fds[1]);
		char byte;
		_exit(read(fds[0], &byte, 1) == 1 && byte == 'x' ? 0 : 1);
	}
	close(fds[0]);
	int status;
	CHECK(waitpid(pid, &status, WNOHANG) == 0);
	CHECK(write(fds[1], "x", 1) == 1);
	close(fds[1]);
	CHECK_CHILD(pid);
	// A child that is not ours.
	CHECK_ERROR(waitpid(getpid(), NULL, 0), ECHILD);
	// waitid sees it too, and WNOWAIT leaves it to be waited for again.
	pid = check_fork();
	if(pid == 0)
		_exit(5);
	siginfo_t info;
	memset(&info, 0, sizeof(info));
	CHECK(waitid(P_PID, pid, &info, WEXITED | WNOWAIT) == 0);
	CHECK(info.si_pid == pid && info.si_code == CLD_EXITED && info.si_status == 5);
	CHECK(waitpid(pid, &status, 0) == pid && WEXITSTATUS(status) == 5);
}

// Descriptors are copied, and a copy shares its description's offset.
static void descriptors(void) {
	int fd = open("/tmp/fork-offset", O_RDWR | O_CREAT | O_TRUNC, 0644);
	CHECK(fd >= 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		CHECK(write(fd, "child", 5) == 5);
		check_exit_child();
	}
	CHECK_CHILD(pid);
	CHECK(lseek(fd, 0, SEEK_CUR) == 5);
	CHECK(write(fd, "parent", 6) == 6);
	char buffer[16] = {0};
	CHECK(pread(fd, buffer, sizeof(buffer) - 1, 0) == 11);
	CHECK(strcmp(buffer, "childparent") == 0);
	close(fd);
	unlink("/tmp/fork-offset");
}

// A child whose parent ends loses its parent, and its process group and
// session stay.
static void orphans(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		pid_t middle = getpid();
		pid_t grandchild = fork();
		if(grandchild == 0) {
			close(fds[0]);
			// Until its parent has gone.
			while(getppid() == middle)
				check_sleep_ms(5);
			pid_t ids[3] = {getppid(), getpgrp(), getsid(0)};
			write(fds[1], ids, sizeof(ids));
			_exit(0);
		}
		_exit(grandchild > 0 ? 0 : 1);
	}
	close(fds[1]);
	CHECK_CHILD(pid);
	pid_t ids[3];
	CHECK(read(fds[0], ids, sizeof(ids)) == sizeof(ids));
	CHECK(ids[0] != pid);
	CHECK(ids[1] == getpgrp() && ids[2] == getsid(0));
	close(fds[0]);
}

// Many forks one after another: each child's memory comes back.
static void many(void) {
	for(int i = 0; i < 200; i++) {
		pid_t pid = fork();
		if(pid == 0)
			_exit(i & 0x7f);
		if(pid < 0) {
			CHECK(pid > 0);
			return;
		}
		int status = check_wait(pid);
		if(!WIFEXITED(status) || WEXITSTATUS(status) != (i & 0x7f)) {
			CHECK(WIFEXITED(status) && WEXITSTATUS(status) == (i & 0x7f));
			return;
		}
	}
}

int main(void) {
	check_start("fork");
	RUN(copies);
	RUN(shares);
	RUN(statuses);
	RUN(waits);
	RUN(descriptors);
	RUN(orphans);
	RUN(many);
	return check_end();
}
