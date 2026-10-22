// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// poll, ppoll, select and pselect: readiness, timeouts, the end of a
// pipe, bad descriptors, waking when another process writes, and the
// signal mask ppoll and pselect wait with.

#include <fcntl.h>
#include <poll.h>
#include <sys/select.h>

#include "check.h"

static void readiness(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	struct pollfd entries[3] = {
			{.fd = fds[0], .events = POLLIN},
			{.fd = fds[1], .events = POLLOUT},
			{.fd = -1, .events = POLLIN},
	};
	CHECK(poll(entries, 3, 0) == 1);
	CHECK(entries[0].revents == 0 && entries[1].revents == POLLOUT && entries[2].revents == 0);
	CHECK(write(fds[1], "a", 1) == 1);
	CHECK(poll(entries, 3, 0) == 2);
	CHECK(entries[0].revents == POLLIN);
	// The normal-data names are POLLIN's and POLLOUT's.
	struct pollfd normal[2] = {
			{.fd = fds[0], .events = POLLRDNORM},
			{.fd = fds[1], .events = POLLWRNORM},
	};
	CHECK(poll(normal, 2, 0) == 2);
	CHECK(normal[0].revents == POLLRDNORM && normal[1].revents == POLLWRNORM);
	// The end: the reader sees POLLHUP, with the data still to read.
	close(fds[1]);
	CHECK(poll(entries, 1, 0) == 1 && (entries[0].revents & POLLHUP));
	CHECK(entries[0].revents & POLLIN);
	char byte;
	CHECK(read(fds[0], &byte, 1) == 1);
	CHECK(poll(entries, 1, 0) == 1 && entries[0].revents == POLLHUP);
	close(fds[0]);
	// A writer whose reader has gone sees POLLERR.
	CHECK(pipe(fds) == 0);
	close(fds[0]);
	struct pollfd writer = {.fd = fds[1], .events = POLLOUT};
	CHECK(poll(&writer, 1, 0) == 1 && (writer.revents & POLLERR));
	close(fds[1]);
	// A descriptor that is not open.
	struct pollfd bad = {.fd = 900, .events = POLLIN};
	CHECK(poll(&bad, 1, 0) == 1 && bad.revents == POLLNVAL);
}

static void timeouts(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	struct pollfd entry = {.fd = fds[0], .events = POLLIN};
	long long start = check_now_ns();
	CHECK(poll(&entry, 1, 50) == 0);
	long long took = check_now_ns() - start;
	CHECK(took >= 45000000 && took < 2000000000);
	// No descriptors at all: a sleep.
	start = check_now_ns();
	CHECK(poll(NULL, 0, 30) == 0);
	CHECK(check_now_ns() - start >= 25000000);
	// Waking when another process writes.
	pid_t pid = check_fork();
	if(pid == 0) {
		check_sleep_ms(30);
		write(fds[1], "w", 1);
		_exit(0);
	}
	start = check_now_ns();
	CHECK(poll(&entry, 1, -1) == 1 && entry.revents == POLLIN);
	took = check_now_ns() - start;
	CHECK(took >= 20000000 && took < 2000000000);
	CHECK_CHILD(pid);
	close(fds[0]);
	close(fds[1]);
}

static void selects(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	fd_set read_set, write_set;
	FD_ZERO(&read_set);
	FD_ZERO(&write_set);
	FD_SET(fds[0], &read_set);
	FD_SET(fds[1], &write_set);
	struct timeval timeout = {0, 0};
	CHECK(select(fds[1] + 1, &read_set, &write_set, NULL, &timeout) == 1);
	CHECK(!FD_ISSET(fds[0], &read_set) && FD_ISSET(fds[1], &write_set));
	CHECK(write(fds[1], "s", 1) == 1);
	FD_SET(fds[0], &read_set);
	FD_SET(fds[1], &write_set);
	CHECK(select(fds[1] + 1, &read_set, &write_set, NULL, &timeout) == 2);
	CHECK(FD_ISSET(fds[0], &read_set) && FD_ISSET(fds[1], &write_set));
	// A timeout with nothing ready.
	char byte;
	CHECK(read(fds[0], &byte, 1) == 1);
	FD_ZERO(&read_set);
	FD_SET(fds[0], &read_set);
	timeout.tv_usec = 30000;
	long long start = check_now_ns();
	CHECK(select(fds[0] + 1, &read_set, NULL, NULL, &timeout) == 0);
	CHECK(check_now_ns() - start >= 25000000);
	CHECK(!FD_ISSET(fds[0], &read_set));
	// The end of the pipe is something to read.
	close(fds[1]);
	FD_SET(fds[0], &read_set);
	timeout.tv_usec = 0;
	CHECK(select(fds[0] + 1, &read_set, NULL, NULL, &timeout) == 1);
	close(fds[0]);
	// A descriptor that is not open.
	FD_ZERO(&read_set);
	FD_SET(900, &read_set);
	CHECK_ERROR(select(901, &read_set, NULL, NULL, &timeout), EBADF);
}

static volatile sig_atomic_t caught;

static void on_signal(int signal) {
	(void)signal;
	caught++;
}

// ppoll and pselect wait with the mask they are given, so a signal the
// program blocks can end the wait without a race.
static void masks(void) {
	struct sigaction action = {0};
	action.sa_handler = on_signal;
	sigemptyset(&action.sa_mask);
	sigaction(SIGUSR1, &action, NULL);
	sigset_t block, old, during;
	sigemptyset(&block);
	sigaddset(&block, SIGUSR1);
	sigprocmask(SIG_BLOCK, &block, &old);
	sigemptyset(&during);
	int fds[2];
	CHECK(pipe(fds) == 0);
	pid_t parent = getpid();
	for(int round = 0; round < 2; round++) {
		caught = 0;
		pid_t pid = check_fork();
		if(pid == 0) {
			check_sleep_ms(20);
			kill(parent, SIGUSR1);
			_exit(0);
		}
		if(round == 0) {
			struct pollfd entry = {.fd = fds[0], .events = POLLIN};
			struct timespec timeout = {5, 0};
			CHECK_ERROR(ppoll(&entry, 1, &timeout, &during), EINTR);
		} else {
			fd_set read_set;
			FD_ZERO(&read_set);
			FD_SET(fds[0], &read_set);
			struct timespec timeout = {5, 0};
			CHECK_ERROR(pselect(fds[0] + 1, &read_set, NULL, NULL, &timeout, &during), EINTR);
		}
		CHECK(caught == 1);
		sigset_t now;
		sigprocmask(SIG_BLOCK, NULL, &now);
		CHECK(sigismember(&now, SIGUSR1));
		CHECK_CHILD(pid);
	}
	sigprocmask(SIG_SETMASK, &old, NULL);
	signal(SIGUSR1, SIG_DFL);
	close(fds[0]);
	close(fds[1]);
}

// Files are always ready.
static void files(void) {
	int fd = open("/tmp/poll-file", O_RDWR | O_CREAT | O_TRUNC, 0644);
	CHECK(fd >= 0);
	struct pollfd entry = {.fd = fd, .events = POLLIN | POLLOUT};
	CHECK(poll(&entry, 1, 0) == 1 && entry.revents == (POLLIN | POLLOUT));
	close(fd);
	unlink("/tmp/poll-file");
}

int main(void) {
	check_start("poll");
	RUN(readiness);
	RUN(timeouts);
	RUN(selects);
	RUN(masks);
	RUN(files);
	return check_end();
}
