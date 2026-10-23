// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Scheduling and limits: nice levels as nice and setpriority set them,
// the sched_ calls, resource limits as setrlimit sets them and fork and
// exec keep them, the file descriptor and stack limits, and the file
// mode mask.

#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <sys/resource.h>
#include <sys/stat.h>

#include "check.h"

// The program, to run again (argv[0]).
static const char *self;

// Uses `depth` MiB of stack; returns `depth`.
__attribute__((noinline)) static int deep(int depth) {
	volatile char block[1 << 20];
	block[0] = 1;
	block[sizeof(block) - 1] = 1;
	int below = depth > 1 ? deep(depth - 1) : 0;
	return below + block[0] + block[sizeof(block) - 1] - 1;
}

// Run again by `limits` after exec: checks what exec kept, and that the
// stack is as large as RLIMIT_STACK says.
static int after_exec(void) {
	check_start("limits");
	struct rlimit limit;
	CHECK(getrlimit(RLIMIT_STACK, &limit) == 0 && limit.rlim_cur == 16 << 20);
	CHECK(getrlimit(RLIMIT_CORE, &limit) == 0 && limit.rlim_max == 0);
	CHECK(umask(0) == 027);
	// 12 MiB of stack: more than the 8 MiB a program gets by default.
	CHECK(deep(12) == 12);
	return check_end();
}

// Runs `f` with an argument in a thread and returns what it returned.
static void *in_thread(void *(*f)(void *), void *argument) {
	pthread_t thread;
	void *result = NULL;
	CHECK(pthread_create(&thread, NULL, f, argument) == 0);
	CHECK(pthread_join(thread, &result) == 0);
	return result;
}

static void *thread_nice(void *argument) {
	(void)argument;
	int nice_was = getpriority(PRIO_PROCESS, 0);
	// Its own, as on Linux: the first thread's stays.
	setpriority(PRIO_PROCESS, 0, 10);
	return (void *)(intptr_t)nice_was;
}

static void priorities(void) {
	errno = 0;
	CHECK(getpriority(PRIO_PROCESS, 0) == 0 && errno == 0);
	CHECK(getpriority(PRIO_PROCESS, getpid()) == 0);
	// In a child in a process group of its own, so that this process
	// keeps its own.
	pid_t child = check_fork();
	if(child == 0) {
		CHECK(setpgid(0, 0) == 0);
		CHECK(nice(3) == 3);
		CHECK(getpriority(PRIO_PROCESS, 0) == 3);
		// A new thread starts with its maker's level.
		CHECK((intptr_t)in_thread(thread_nice, NULL) == 3);
		CHECK(getpriority(PRIO_PROCESS, 0) == 3);
		// More of the CPU needs RLIMIT_NICE, which is 0 as on Linux.
		CHECK_ERROR(setpriority(PRIO_PROCESS, 0, 1), EACCES);
		errno = 0;
		CHECK(nice(-1) == -1 && errno == EPERM);
		CHECK(setpriority(PRIO_PROCESS, 0, 100) == 0);
		CHECK(getpriority(PRIO_PROCESS, 0) == 19);
		// A process group's, and one of its children's.
		CHECK(setpriority(PRIO_PGRP, 0, 19) == 0 && getpriority(PRIO_PGRP, 0) == 19);
		pid_t grandchild = check_fork();
		if(grandchild == 0) {
			CHECK(getpriority(PRIO_PROCESS, 0) == 19);
			check_exit_child();
		}
		CHECK_CHILD(grandchild);
		check_exit_child();
	}
	CHECK_CHILD(child);
	CHECK(getpriority(PRIO_PROCESS, 0) == 0);
	// Every process is user 0's until there are users (Phase 13).
	errno = 0;
	CHECK(getpriority(PRIO_USER, 0) <= 0 && errno == 0);
	CHECK_ERROR(getpriority(PRIO_USER, 1000), ESRCH);
	CHECK_ERROR(getpriority(PRIO_PROCESS, 99999), ESRCH);
	CHECK_ERROR(getpriority(42, 0), EINVAL);
}

static void policies(void) {
	struct sched_param param = {0};
	CHECK(sched_getscheduler(0) == SCHED_OTHER);
	CHECK(sched_getparam(getpid(), &param) == 0 && param.sched_priority == 0);
	int policy = -1;
	CHECK(pthread_getschedparam(pthread_self(), &policy, &param) == 0);
	CHECK(policy == SCHED_OTHER && param.sched_priority == 0);
	CHECK(sched_get_priority_min(SCHED_FIFO) == 1 && sched_get_priority_max(SCHED_FIFO) == 99);
	CHECK(sched_get_priority_min(SCHED_RR) == 1 && sched_get_priority_max(SCHED_RR) == 99);
	CHECK(sched_get_priority_min(SCHED_OTHER) == 0 && sched_get_priority_max(SCHED_OTHER) == 0);
	CHECK_ERROR(sched_get_priority_max(42), EINVAL);
	CHECK(sched_yield() == 0);
	pid_t child = check_fork();
	if(child == 0) {
		CHECK(sched_setscheduler(0, SCHED_BATCH, &param) == 0);
		CHECK(sched_getscheduler(0) == SCHED_BATCH);
		CHECK(sched_setscheduler(getpid(), SCHED_OTHER, &param) == 0);
		// Real-time scheduling needs RLIMIT_RTPRIO (0, as on Linux).
		param.sched_priority = 10;
		CHECK_ERROR(sched_setscheduler(0, SCHED_FIFO, &param), EPERM);
		CHECK(pthread_setschedparam(pthread_self(), SCHED_RR, &param) == EPERM);
		CHECK_ERROR(sched_setscheduler(0, SCHED_OTHER, &param), EINVAL);
		param.sched_priority = 100;
		CHECK_ERROR(sched_setscheduler(0, SCHED_FIFO, &param), EINVAL);
		param.sched_priority = 0;
		CHECK_ERROR(sched_setscheduler(0, 4, &param), EINVAL);
		// Idle is the smallest share; leaving it needs RLIMIT_NICE.
		CHECK(sched_setscheduler(0, SCHED_IDLE, &param) == 0);
		CHECK(sched_getscheduler(0) == SCHED_IDLE);
		CHECK_ERROR(sched_setscheduler(0, SCHED_OTHER, &param), EPERM);
		check_exit_child();
	}
	CHECK_CHILD(child);
	CHECK(sched_getscheduler(0) == SCHED_OTHER);
	CHECK_ERROR(sched_getscheduler(99999), ESRCH);
}

static void limits(void) {
	struct rlimit limit;
	CHECK(getrlimit(RLIMIT_NOFILE, &limit) == 0);
	CHECK(limit.rlim_cur == 1024 && limit.rlim_max == 4096);
	CHECK(sysconf(_SC_OPEN_MAX) == 1024);
	CHECK(getrlimit(RLIMIT_STACK, &limit) == 0 && limit.rlim_cur == 8 << 20);
	CHECK(limit.rlim_max == RLIM_INFINITY);
	CHECK(getrlimit(RLIMIT_CPU, &limit) == 0 && limit.rlim_cur == RLIM_INFINITY);
	CHECK(getrlimit(RLIMIT_NICE, &limit) == 0 && limit.rlim_max == 0);
	CHECK(getrlimit(RLIMIT_RTPRIO, &limit) == 0 && limit.rlim_max == 0);
	CHECK_ERROR(getrlimit(99, &limit), EINVAL);
	pid_t child = check_fork();
	if(child == 0) {
		// Descriptors stop at the soft limit.
		struct rlimit files = {16, 1024};
		CHECK(setrlimit(RLIMIT_NOFILE, &files) == 0);
		CHECK(sysconf(_SC_OPEN_MAX) == 16);
		int opened = 0;
		while(opened < 64 && open("/", O_RDONLY | O_DIRECTORY) >= 0)
			opened++;
		CHECK(errno == EMFILE && opened > 0 && opened < 16);
		CHECK_ERROR(dup2(0, 20), EBADF);
		CHECK_ERROR(fcntl(0, F_DUPFD, 20), EINVAL);
		files.rlim_cur = 1024;
		CHECK(setrlimit(RLIMIT_NOFILE, &files) == 0);
		CHECK(dup2(0, 20) == 20);
		// A hard limit only goes down; the soft one stays below it.
		files.rlim_max = 2048;
		CHECK_ERROR(setrlimit(RLIMIT_NOFILE, &files), EPERM);
		files.rlim_cur = 2000;
		files.rlim_max = 1024;
		CHECK_ERROR(setrlimit(RLIMIT_NOFILE, &files), EINVAL);
		struct rlimit core = {0, 0};
		CHECK(setrlimit(RLIMIT_CORE, &core) == 0);
		core.rlim_max = RLIM_INFINITY;
		CHECK_ERROR(setrlimit(RLIMIT_CORE, &core), EPERM);
		// A child has them, and exec keeps them, with a stack that size.
		struct rlimit stack = {16 << 20, RLIM_INFINITY};
		CHECK(setrlimit(RLIMIT_STACK, &stack) == 0);
		CHECK(umask(027) == 022);
		pid_t grandchild = check_fork();
		if(grandchild == 0) {
			CHECK(getrlimit(RLIMIT_CORE, &core) == 0 && core.rlim_max == 0);
			CHECK(umask(027) == 027);
			execl(self, self, "after-exec", (char *)NULL);
			CHECK(!"exec");
			check_exit_child();
		}
		CHECK_CHILD(grandchild);
		check_exit_child();
	}
	CHECK_CHILD(child);
	CHECK(getrlimit(RLIMIT_CORE, &limit) == 0 && limit.rlim_max == RLIM_INFINITY);
}

static void file_mask(void) {
	mode_t old = umask(077);
	CHECK(old == 022);
	CHECK(umask(old) == 077);
	CHECK(umask(01777) == 022);
	CHECK(umask(old) == 0777);
}

int main(int argc, char **argv) {
	self = argv[0];
	if(argc > 1 && !strcmp(argv[1], "after-exec"))
		return after_exec();
	check_start("limits");
	RUN(priorities);
	RUN(policies);
	RUN(limits);
	RUN(file_mask);
	return check_end();
}
