// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Socket pairs (socketpair(AF_UNIX, SOCK_STREAM)): data both ways with
// read, write, send, recv, sendmsg and recvmsg; what fstat, getsockopt,
// getsockname and getpeername say; shutdown each way; the end of the
// stream, SIGPIPE, EPIPE and MSG_NOSIGNAL; non-blocking ends and
// MSG_DONTWAIT; MSG_WAITALL; poll; and pairs shared with a child.

#include <fcntl.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/un.h>

#include "check.h"

static void basics(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	CHECK(fds[0] >= 0 && fds[1] >= 0 && fds[0] != fds[1]);
	// Each end reads what the other writes.
	CHECK(write(fds[0], "ping", 4) == 4);
	CHECK(write(fds[1], "pong!", 5) == 5);
	char buffer[16] = {0};
	CHECK(read(fds[1], buffer, sizeof(buffer)) == 4 && !memcmp(buffer, "ping", 4));
	CHECK(recv(fds[0], buffer, 2, 0) == 2 && !memcmp(buffer, "po", 2));
	CHECK(recv(fds[0], buffer, sizeof(buffer), 0) == 3 && !memcmp(buffer, "ng!", 3));
	CHECK(send(fds[1], "abc", 3, 0) == 3);
	int readable = -1;
	CHECK(ioctl(fds[0], FIONREAD, &readable) == 0 && readable == 3);
	// recvfrom gives no address: the other end has no name.
	struct sockaddr_un from;
	socklen_t from_len = sizeof(from);
	CHECK(recvfrom(fds[0], buffer, sizeof(buffer), 0, (struct sockaddr *)&from, &from_len) == 3);
	CHECK(from_len == 0);
	// A pair is connected, so an address is one too many.
	struct sockaddr_un to = {.sun_family = AF_UNIX, .sun_path = "/tmp/nowhere"};
	CHECK_ERROR(sendto(fds[0], "x", 1, 0, (struct sockaddr *)&to, sizeof(to)), EISCONN);
	// Neither end can seek.
	CHECK_ERROR(lseek(fds[0], 0, SEEK_SET), ESPIPE);
	close(fds[0]);
	close(fds[1]);
}

static void described(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	struct stat st;
	CHECK(fstat(fds[0], &st) == 0 && S_ISSOCK(st.st_mode));
	int value = -1;
	socklen_t size = sizeof(value);
	CHECK(getsockopt(fds[0], SOL_SOCKET, SO_TYPE, &value, &size) == 0);
	CHECK(value == SOCK_STREAM && size == sizeof(int));
	CHECK(getsockopt(fds[0], SOL_SOCKET, SO_DOMAIN, &value, &size) == 0 && value == AF_UNIX);
	CHECK(getsockopt(fds[0], SOL_SOCKET, SO_ERROR, &value, &size) == 0 && value == 0);
	CHECK(getsockopt(fds[0], SOL_SOCKET, SO_SNDBUF, &value, &size) == 0 && value > 0);
	value = 1 << 16;
	CHECK(setsockopt(fds[0], SOL_SOCKET, SO_RCVBUF, &value, sizeof(value)) == 0);
	// Both ends are unnamed: only the family.
	struct sockaddr_un name;
	socklen_t len = sizeof(name);
	memset(&name, 0xff, sizeof(name));
	CHECK(getsockname(fds[0], (struct sockaddr *)&name, &len) == 0);
	CHECK(len == sizeof(sa_family_t) && name.sun_family == AF_UNIX);
	len = sizeof(name);
	CHECK(getpeername(fds[1], (struct sockaddr *)&name, &len) == 0);
	CHECK(len == sizeof(sa_family_t) && name.sun_family == AF_UNIX);
	// What is not a socket says so.
	int pipe_fds[2];
	CHECK(pipe(pipe_fds) == 0);
	CHECK_ERROR(send(pipe_fds[1], "x", 1, 0), ENOTSOCK);
	CHECK_ERROR(recv(pipe_fds[0], &value, 1, MSG_DONTWAIT), ENOTSOCK);
	CHECK_ERROR(shutdown(pipe_fds[0], SHUT_RD), ENOTSOCK);
	size = sizeof(value);
	CHECK_ERROR(getsockopt(pipe_fds[0], SOL_SOCKET, SO_TYPE, &value, &size), ENOTSOCK);
	len = sizeof(name);
	CHECK_ERROR(getsockname(pipe_fds[0], (struct sockaddr *)&name, &len), ENOTSOCK);
	close(pipe_fds[0]);
	close(pipe_fds[1]);
	CHECK_ERROR(send(-1, "x", 1, 0), EBADF);
	CHECK_ERROR(shutdown(fds[0], 7), EINVAL);
	// Only AF_UNIX makes pairs, of streams so far.
	CHECK_ERROR(socketpair(AF_UNIX, SOCK_STREAM, 99, fds), EPROTONOSUPPORT);
	close(fds[0]);
	close(fds[1]);
}

// sendmsg gathers its buffers and recvmsg scatters into its own.
static void vectors(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	struct iovec out[3] = {{"one ", 4}, {"", 0}, {"two three", 9}};
	struct msghdr message = {.msg_iov = out, .msg_iovlen = 3};
	CHECK(sendmsg(fds[0], &message, 0) == 13);
	char first[5] = {0};
	char second[16] = {0};
	struct iovec in[2] = {{first, 4}, {second, sizeof(second)}};
	struct msghdr received = {.msg_iov = in, .msg_iovlen = 2};
	received.msg_flags = -1;
	CHECK(recvmsg(fds[1], &received, 0) == 13);
	CHECK(!memcmp(first, "one ", 4) && !memcmp(second, "two three", 9));
	CHECK(received.msg_flags == 0 && received.msg_controllen == 0);
	close(fds[0]);
	close(fds[1]);
}

static void shutting(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	CHECK(write(fds[0], "last", 4) == 4);
	// Shut for writing: the other end reads what was sent, then the end,
	// and can still write back.
	CHECK(shutdown(fds[0], SHUT_WR) == 0);
	char buffer[8];
	CHECK(read(fds[1], buffer, sizeof(buffer)) == 4);
	CHECK(read(fds[1], buffer, sizeof(buffer)) == 0);
	struct pollfd p = {.fd = fds[1], .events = POLLIN};
	CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLIN));
	CHECK(write(fds[1], "back", 4) == 4);
	CHECK(read(fds[0], buffer, sizeof(buffer)) == 4 && !memcmp(buffer, "back", 4));
	// Sending on an end shut for writing fails, without SIGPIPE when asked.
	CHECK_ERROR(send(fds[0], "x", 1, MSG_NOSIGNAL), EPIPE);
	// Shutting it again does no harm.
	CHECK(shutdown(fds[0], SHUT_WR) == 0);
	// Shut for reading: it reads the end, and the other end can no longer
	// send to it.
	CHECK(shutdown(fds[0], SHUT_RD) == 0);
	CHECK(read(fds[0], buffer, sizeof(buffer)) == 0);
	CHECK_ERROR(send(fds[1], "x", 1, MSG_NOSIGNAL), EPIPE);
	// Both ways done: hung up.
	p.fd = fds[0];
	p.events = POLLIN | POLLOUT;
	CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLHUP));
	close(fds[0]);
	close(fds[1]);
}

static void closing(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	CHECK(write(fds[0], "bye", 3) == 3);
	close(fds[0]);
	char buffer[8];
	CHECK(read(fds[1], buffer, sizeof(buffer)) == 3);
	CHECK(read(fds[1], buffer, sizeof(buffer)) == 0);
	struct pollfd p = {.fd = fds[1], .events = POLLIN};
	CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLHUP) && (p.revents & POLLIN));
	// Writing to an end no one reads: SIGPIPE, which ends the process...
	pid_t pid = check_fork();
	if(pid == 0) {
		write(fds[1], "x", 1);
		_exit(0);
	}
	int status = check_wait(pid);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGPIPE);
	// ...but not with MSG_NOSIGNAL, even with SIGPIPE's default action.
	pid = check_fork();
	if(pid == 0) {
		CHECK_ERROR(send(fds[1], "x", 1, MSG_NOSIGNAL), EPIPE);
		check_exit_child();
	}
	CHECK_CHILD(pid);
	// With SIGPIPE ignored, EPIPE.
	signal(SIGPIPE, SIG_IGN);
	CHECK_ERROR(write(fds[1], "x", 1), EPIPE);
	signal(SIGPIPE, SIG_DFL);
	close(fds[1]);
}

static void non_blocking(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0, fds) == 0);
	CHECK(fcntl(fds[0], F_GETFL) & O_NONBLOCK);
	CHECK(fcntl(fds[1], F_GETFD) & FD_CLOEXEC);
	char byte;
	CHECK_ERROR(read(fds[0], &byte, 1), EAGAIN);
	CHECK_ERROR(recv(fds[0], &byte, 1, 0), EAGAIN);
	// Fill one way: it takes a pipe's worth, then no more.
	static char block[4096];
	size_t sent = 0;
	for(;;) {
		ssize_t n = send(fds[0], block, sizeof(block), 0);
		if(n < 0) {
			CHECK(errno == EAGAIN);
			break;
		}
		sent += (size_t)n;
		if(sent > (8 << 20))
			break;
	}
	CHECK(sent >= 4096 && sent <= (1 << 20));
	struct pollfd p = {.fd = fds[0], .events = POLLOUT};
	CHECK(poll(&p, 1, 0) == 0);
	// The other way is still free.
	CHECK(send(fds[1], "y", 1, 0) == 1);
	CHECK(recv(fds[0], &byte, 1, 0) == 1 && byte == 'y');
	close(fds[0]);
	close(fds[1]);
	// MSG_DONTWAIT on a blocking end.
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	CHECK(!(fcntl(fds[0], F_GETFL) & O_NONBLOCK));
	CHECK(!(fcntl(fds[0], F_GETFD) & FD_CLOEXEC));
	CHECK_ERROR(recv(fds[0], &byte, 1, MSG_DONTWAIT), EAGAIN);
	p.fd = fds[1];
	p.events = POLLIN | POLLOUT;
	CHECK(poll(&p, 1, 0) == 1 && p.revents == POLLOUT);
	close(fds[0]);
	close(fds[1]);
}

// A child and its parent, over one pair: requests and answers, then a
// megabyte one way, which fills the socket and waits for the reader, and
// MSG_WAITALL for bytes that come in pieces.
static void shared(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	enum { TOTAL = 1 << 20, ROUNDS = 100 };
	pid_t pid = check_fork();
	if(pid == 0) {
		close(fds[0]);
		for(int i = 0; i < ROUNDS; i++) {
			int n;
			CHECK(recv(fds[1], &n, sizeof(n), MSG_WAITALL) == sizeof(n) && n == i);
			n = i * 2;
			CHECK(send(fds[1], &n, sizeof(n), 0) == sizeof(n));
		}
		static char data[TOTAL];
		for(int i = 0; i < TOTAL; i++)
			data[i] = (char)(i * 7);
		CHECK(write(fds[1], data, TOTAL) == TOTAL);
		// Ten bytes, one at a time.
		for(int i = 0; i < 10; i++) {
			check_sleep_ms(5);
			CHECK(send(fds[1], &"0123456789"[i], 1, 0) == 1);
		}
		close(fds[1]);
		check_exit_child();
	}
	close(fds[1]);
	for(int i = 0; i < ROUNDS; i++) {
		CHECK(send(fds[0], &i, sizeof(i), 0) == sizeof(i));
		int n = -1;
		CHECK(recv(fds[0], &n, sizeof(n), MSG_WAITALL) == sizeof(n) && n == i * 2);
	}
	static char got[TOTAL];
	size_t have = 0;
	while(have < TOTAL) {
		ssize_t n = read(fds[0], got + have, TOTAL - have);
		if(n <= 0)
			break;
		have += (size_t)n;
	}
	CHECK(have == TOTAL);
	int wrong = 0;
	for(int i = 0; i < TOTAL; i++)
		wrong += got[i] != (char)(i * 7);
	CHECK(wrong == 0);
	char ten[10] = {0};
	CHECK(recv(fds[0], ten, sizeof(ten), MSG_WAITALL) == 10 && !memcmp(ten, "0123456789", 10));
	CHECK(recv(fds[0], ten, sizeof(ten), MSG_WAITALL) == 0);
	CHECK_CHILD(pid);
	close(fds[0]);
}

// A descriptor made with dup is the same socket: shutting either down
// shuts both, and the socket lasts until the last one closes.
static void duplicated(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	int copy = dup(fds[0]);
	CHECK(copy >= 0);
	close(fds[0]);
	CHECK(write(fds[1], "via", 3) == 3);
	char buffer[8];
	CHECK(read(copy, buffer, sizeof(buffer)) == 3 && !memcmp(buffer, "via", 3));
	int again = dup(copy);
	CHECK(again >= 0);
	CHECK(shutdown(again, SHUT_WR) == 0);
	CHECK_ERROR(send(copy, "x", 1, MSG_NOSIGNAL), EPIPE);
	CHECK(read(fds[1], buffer, sizeof(buffer)) == 0);
	close(copy);
	close(again);
	close(fds[1]);
}

int main(void) {
	check_start("socket");
	RUN(basics);
	RUN(described);
	RUN(vectors);
	RUN(shutting);
	RUN(closing);
	RUN(non_blocking);
	RUN(shared);
	RUN(duplicated);
	return check_end();
}
