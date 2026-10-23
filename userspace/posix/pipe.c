// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Pipes: data in order, the end of the stream, SIGPIPE and EPIPE, a pipe
// that fills, non-blocking ends, atomic writes, and descriptors made with
// dup, dup2, dup3 and fcntl.

#include <fcntl.h>
#include <limits.h>
#include <sys/ioctl.h>

#include "check.h"

static void basics(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	CHECK(fds[0] >= 0 && fds[1] >= 0 && fds[0] != fds[1]);
	CHECK(write(fds[1], "abc", 3) == 3);
	CHECK(write(fds[1], "def", 3) == 3);
	char buffer[16] = {0};
	CHECK(read(fds[0], buffer, 2) == 2 && !memcmp(buffer, "ab", 2));
	CHECK(read(fds[0], buffer, sizeof(buffer)) == 4 && !memcmp(buffer, "cdef", 4));
	// The reading end cannot be written, nor the writing end read.
	CHECK_ERROR(write(fds[0], "x", 1), EBADF);
	CHECK_ERROR(read(fds[1], buffer, 1), EBADF);
	CHECK_ERROR(lseek(fds[0], 0, SEEK_SET), ESPIPE);
	int readable = -1;
	CHECK(write(fds[1], "12345", 5) == 5);
	CHECK(ioctl(fds[0], FIONREAD, &readable) == 0 && readable == 5);
	CHECK(read(fds[0], buffer, sizeof(buffer)) == 5);
	// The end of the stream, once no writer is left.
	close(fds[1]);
	CHECK(read(fds[0], buffer, sizeof(buffer)) == 0);
	close(fds[0]);
}

static void broken(void) {
	// Writing with no reader left: SIGPIPE, which ends the process.
	int fds[2];
	CHECK(pipe(fds) == 0);
	close(fds[0]);
	pid_t pid = check_fork();
	if(pid == 0) {
		write(fds[1], "x", 1);
		_exit(0);
	}
	int status = check_wait(pid);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGPIPE);
	// With SIGPIPE ignored, EPIPE.
	signal(SIGPIPE, SIG_IGN);
	CHECK_ERROR(write(fds[1], "x", 1), EPIPE);
	signal(SIGPIPE, SIG_DFL);
	close(fds[1]);
}

// A megabyte through a pipe, which fills and waits for the reader.
static void streams(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	enum { TOTAL = 1 << 20 };
	pid_t pid = check_fork();
	if(pid == 0) {
		close(fds[0]);
		unsigned char chunk[3001];
		size_t sent = 0;
		while(sent < TOTAL) {
			size_t n = TOTAL - sent < sizeof(chunk) ? TOTAL - sent : sizeof(chunk);
			for(size_t i = 0; i < n; i++)
				chunk[i] = (unsigned char)((sent + i) % 251);
			ssize_t wrote = write(fds[1], chunk, n);
			if(wrote <= 0)
				_exit(1);
			sent += (size_t)wrote;
		}
		_exit(0);
	}
	close(fds[1]);
	unsigned char buffer[4096];
	size_t got = 0, wrong = 0;
	ssize_t n;
	while((n = read(fds[0], buffer, sizeof(buffer))) > 0) {
		for(ssize_t i = 0; i < n; i++)
			wrong += buffer[i] != (unsigned char)((got + (size_t)i) % 251);
		got += (size_t)n;
	}
	CHECK(n == 0 && got == TOTAL && wrong == 0);
	CHECK_CHILD(pid);
	close(fds[0]);
}

static void nonblocking(void) {
	int fds[2];
	CHECK(pipe2(fds, O_NONBLOCK) == 0);
	CHECK(fcntl(fds[0], F_GETFL) & O_NONBLOCK);
	char buffer[4096];
	CHECK_ERROR(read(fds[0], buffer, 1), EAGAIN);
	// Fill it up: then a write would wait.
	memset(buffer, 'z', sizeof(buffer));
	size_t room = 0;
	ssize_t n;
	while((n = write(fds[1], buffer, sizeof(buffer))) > 0)
		room += (size_t)n;
	CHECK(n == -1 && errno == EAGAIN);
	CHECK(room >= 4096);
	// A write of PIPE_BUF or less goes in whole or not at all.
	CHECK(read(fds[0], buffer, 100) == 100);
	CHECK_ERROR(write(fds[1], buffer, PIPE_BUF), EAGAIN);
	// Back to blocking, with fcntl.
	int flags = fcntl(fds[0], F_GETFL);
	CHECK(fcntl(fds[0], F_SETFL, flags & ~O_NONBLOCK) == 0);
	CHECK(!(fcntl(fds[0], F_GETFL) & O_NONBLOCK));
	CHECK((fcntl(fds[0], F_GETFL) & O_ACCMODE) == O_RDONLY);
	CHECK((fcntl(fds[1], F_GETFL) & O_ACCMODE) == O_WRONLY);
	close(fds[0]);
	close(fds[1]);
}

// Writes of PIPE_BUF bytes or fewer from two writers never interleave.
static void atomic(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	enum { MESSAGES = 300, SIZE = 512 };
	pid_t writers[2];
	for(int w = 0; w < 2; w++) {
		writers[w] = check_fork();
		if(writers[w] == 0) {
			close(fds[0]);
			char message[SIZE];
			memset(message, 'a' + w, sizeof(message));
			for(int i = 0; i < MESSAGES; i++) {
				if(write(fds[1], message, sizeof(message)) != SIZE)
					_exit(1);
			}
			_exit(0);
		}
	}
	close(fds[1]);
	char message[SIZE];
	int counts[2] = {0, 0}, torn = 0;
	for(;;) {
		size_t got = 0;
		ssize_t n = 0;
		while(got < SIZE && (n = read(fds[0], message + got, SIZE - got)) > 0)
			got += (size_t)n;
		if(got == 0)
			break;
		int whole = got == SIZE;
		for(size_t i = 1; i < got && whole; i++)
			whole = message[i] == message[0];
		if(whole && (message[0] == 'a' || message[0] == 'b'))
			counts[message[0] - 'a']++;
		else
			torn++;
	}
	CHECK(torn == 0 && counts[0] == MESSAGES && counts[1] == MESSAGES);
	CHECK_CHILD(writers[0]);
	CHECK_CHILD(writers[1]);
	close(fds[0]);
}

static void duplicates(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	int copy = dup(fds[1]);
	CHECK(copy >= 0);
	close(fds[1]);
	// The copy keeps the pipe open.
	CHECK(write(copy, "d", 1) == 1);
	CHECK(dup2(copy, 50) == 50);
	CHECK(write(50, "e", 1) == 1);
	CHECK(dup2(50, 50) == 50);
	CHECK_ERROR(dup2(50, -1), EBADF);
	CHECK_ERROR(dup(1000), EBADF);
	CHECK(dup3(copy, 51, O_CLOEXEC) == 51 && fcntl(51, F_GETFD) == FD_CLOEXEC);
	CHECK_ERROR(dup3(51, 51, 0), EINVAL);
	// F_DUPFD takes the lowest free descriptor from its argument on.
	int high = fcntl(copy, F_DUPFD, 60);
	CHECK(high == 60);
	int high_cloexec = fcntl(copy, F_DUPFD_CLOEXEC, 60);
	CHECK(high_cloexec == 61 && fcntl(high_cloexec, F_GETFD) == FD_CLOEXEC);
	char buffer[4] = {0};
	CHECK(read(fds[0], buffer, sizeof(buffer)) == 2 && !memcmp(buffer, "de", 2));
	close(copy);
	close(50);
	close(51);
	close(high);
	close(high_cloexec);
	// The lowest free descriptor comes first.
	int lowest = dup(fds[0]);
	CHECK(lowest == fds[1]);
	close(lowest);
	close(fds[0]);
	CHECK_ERROR(close(fds[0]), EBADF);
}

// A pipeline of three processes, as a shell makes one.
static void pipeline(void) {
	int first[2], second[2];
	CHECK(pipe(first) == 0 && pipe(second) == 0);
	pid_t producer = check_fork();
	if(producer == 0) {
		dup2(first[1], STDOUT_FILENO);
		close(first[0]);
		close(first[1]);
		close(second[0]);
		close(second[1]);
		for(int i = 0; i < 100; i++)
			printf("line %d\n", i);
		exit(0);
	}
	pid_t filter = check_fork();
	if(filter == 0) {
		dup2(first[0], STDIN_FILENO);
		dup2(second[1], STDOUT_FILENO);
		close(first[0]);
		close(first[1]);
		close(second[0]);
		close(second[1]);
		char line[64];
		int count = 0;
		while(fgets(line, sizeof(line), stdin))
			count++;
		printf("%d\n", count);
		exit(0);
	}
	close(first[0]);
	close(first[1]);
	close(second[1]);
	char buffer[16] = {0};
	ssize_t got = 0, n;
	while((n = read(second[0], buffer + got, sizeof(buffer) - 1 - (size_t)got)) > 0)
		got += n;
	CHECK(!strcmp(buffer, "100\n"));
	close(second[0]);
	CHECK_CHILD(producer);
	CHECK_CHILD(filter);
}

int main(void) {
	check_start("pipe");
	RUN(basics);
	RUN(broken);
	RUN(streams);
	RUN(nonblocking);
	RUN(atomic);
	RUN(duplicates);
	RUN(pipeline);
	return check_end();
}
