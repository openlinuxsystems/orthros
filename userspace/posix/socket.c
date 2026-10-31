// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// AF_UNIX stream sockets. Pairs (socketpair): data both ways with read,
// write, send, recv, sendmsg and recvmsg; what fstat, getsockopt,
// getsockname and getpeername say; shutdown each way; the end of the
// stream, SIGPIPE, EPIPE and MSG_NOSIGNAL; non-blocking ends and
// MSG_DONTWAIT; MSG_WAITALL; poll; and pairs shared with a child. Named
// sockets: bind, listen, connect and accept, their names and errors, the
// backlog, and connections from another process. And descriptors passed
// with SCM_RIGHTS: pipes, files and directories, between processes too,
// MSG_CTRUNC, MSG_CMSG_CLOEXEC, the bytes they ride on, and the limits.

#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <stddef.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/un.h>

#include "check.h"

// The name the named sockets' tests bind.
#define NAME "/tmp/socket-test"

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
	// More vectors than IOV_MAX, or a negative count, which Linux reads
	// as a huge unsigned one, are too many.
	static struct iovec many[IOV_MAX + 1];
	char byte = 0;
	for(int i = 0; i <= IOV_MAX; i++)
		many[i] = (struct iovec){&byte, 1};
	struct msghdr too_many = {.msg_iov = many, .msg_iovlen = IOV_MAX + 1};
	CHECK_ERROR(sendmsg(fds[0], &too_many, 0), EMSGSIZE);
	CHECK_ERROR(recvmsg(fds[1], &too_many, MSG_DONTWAIT), EMSGSIZE);
	struct msghdr negative = {.msg_iov = many};
	negative.msg_iovlen = -1;
	CHECK_ERROR(sendmsg(fds[0], &negative, 0), EMSGSIZE);
	CHECK_ERROR(recvmsg(fds[1], &negative, MSG_DONTWAIT), EMSGSIZE);
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

// An AF_UNIX address for `path`, and its length.
static socklen_t address_of(const char *path, struct sockaddr_un *addr) {
	memset(addr, 0, sizeof(*addr));
	addr->sun_family = AF_UNIX;
	strcpy(addr->sun_path, path);
	return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1);
}

// Whether `fd`'s name, or with `peer` its peer's, is `path` (NULL for
// none: just the family).
static int named_as(int fd, int peer, const char *path) {
	struct sockaddr_un name;
	memset(&name, 0xff, sizeof(name));
	socklen_t len = sizeof(name);
	int got = peer ? getpeername(fd, (struct sockaddr *)&name, &len)
			: getsockname(fd, (struct sockaddr *)&name, &len);
	if(got != 0 || name.sun_family != AF_UNIX)
		return 0;
	if(!path)
		return len == sizeof(sa_family_t);
	return len == offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1
			&& !strcmp(name.sun_path, path);
}

// A socket bound to NAME and listening, with `backlog`.
static int listening(int backlog) {
	int s = socket(AF_UNIX, SOCK_STREAM, 0);
	CHECK(s >= 0);
	struct sockaddr_un addr;
	socklen_t len = address_of(NAME, &addr);
	CHECK(bind(s, (struct sockaddr *)&addr, len) == 0);
	CHECK(listen(s, backlog) == 0);
	return s;
}

// A new socket connected to NAME: 0 or the errno connect failed with in
// `error`, and the socket, or -1 if it failed.
static int connected(int flags, int *error) {
	int c = socket(AF_UNIX, SOCK_STREAM | flags, 0);
	CHECK(c >= 0);
	struct sockaddr_un addr;
	socklen_t len = address_of(NAME, &addr);
	*error = connect(c, (struct sockaddr *)&addr, len) == 0 ? 0 : errno;
	if(*error) {
		close(c);
		return -1;
	}
	return c;
}

static void named(void) {
	int s = socket(AF_UNIX, SOCK_STREAM, 0);
	CHECK(s >= 0);
	struct stat st;
	CHECK(fstat(s, &st) == 0 && S_ISSOCK(st.st_mode));
	// Neither named nor connected yet.
	CHECK(named_as(s, 0, NULL));
	struct sockaddr_un name;
	socklen_t len = sizeof(name);
	CHECK_ERROR(getpeername(s, (struct sockaddr *)&name, &len), ENOTCONN);
	CHECK_ERROR(send(s, "x", 1, MSG_NOSIGNAL), ENOTCONN);
	// Listening needs a name.
	CHECK_ERROR(listen(s, 1), EINVAL);
	unlink(NAME);
	struct sockaddr_un addr;
	socklen_t addr_len = address_of(NAME, &addr);
	CHECK_ERROR(connect(s, (struct sockaddr *)&addr, addr_len), ENOENT);
	// The name is a file of the socket kind, which opens to nothing.
	CHECK(bind(s, (struct sockaddr *)&addr, addr_len) == 0);
	CHECK(stat(NAME, &st) == 0 && S_ISSOCK(st.st_mode));
	CHECK_ERROR(open(NAME, O_RDWR), ENXIO);
	CHECK(named_as(s, 0, NAME));
	// One name to a socket, and one socket to a name.
	struct sockaddr_un other;
	socklen_t other_len = address_of("/tmp/socket-other", &other);
	CHECK_ERROR(bind(s, (struct sockaddr *)&other, other_len), EINVAL);
	int second = socket(AF_UNIX, SOCK_STREAM, 0);
	CHECK_ERROR(bind(second, (struct sockaddr *)&addr, addr_len), EADDRINUSE);
	CHECK_ERROR(bind(second, (struct sockaddr *)&other, 1), EINVAL);
	// Linux's abstract names, which start with a NUL, are not taken.
	int third = socket(AF_UNIX, SOCK_STREAM, 0);
	struct sockaddr_un abstract = {.sun_family = AF_UNIX, .sun_path = "\0abstract"};
	CHECK_ERROR(bind(third, (struct sockaddr *)&abstract, sizeof(abstract)), EOPNOTSUPP);
	close(third);
	CHECK_ERROR(bind(second, (struct sockaddr *)&other, 0), EINVAL);
	// Named but not listening: no one takes a connection.
	CHECK_ERROR(connect(second, (struct sockaddr *)&addr, addr_len), ECONNREFUSED);
	// Nor does a file that is not a socket's name.
	int file = open("/tmp/socket-file", O_WRONLY | O_CREAT | O_TRUNC, 0644);
	CHECK(file >= 0);
	close(file);
	struct sockaddr_un plain;
	socklen_t plain_len = address_of("/tmp/socket-file", &plain);
	CHECK_ERROR(connect(second, (struct sockaddr *)&plain, plain_len), ECONNREFUSED);
	unlink("/tmp/socket-file");
	// Listening, it is readable while a connection waits.
	CHECK(listen(s, 4) == 0);
	int value = 0;
	socklen_t size = sizeof(value);
	CHECK(getsockopt(s, SOL_SOCKET, SO_ACCEPTCONN, &value, &size) == 0 && value == 1);
	struct pollfd p = {.fd = s, .events = POLLIN};
	CHECK(poll(&p, 1, 0) == 0);
	CHECK(connect(second, (struct sockaddr *)&addr, addr_len) == 0);
	CHECK(poll(&p, 1, 0) == 1 && p.revents == POLLIN);
	CHECK(named_as(second, 0, NULL) && named_as(second, 1, NAME));
	size = sizeof(value);
	CHECK(getsockopt(second, SOL_SOCKET, SO_ACCEPTCONN, &value, &size) == 0 && value == 0);
	// The socket accept makes has the listening socket's name, and its
	// peer has none.
	memset(&name, 0xff, sizeof(name));
	len = sizeof(name);
	int a = accept(s, (struct sockaddr *)&name, &len);
	CHECK(a >= 0);
	CHECK(len == sizeof(sa_family_t) && name.sun_family == AF_UNIX);
	CHECK(named_as(a, 0, NAME) && named_as(a, 1, NULL));
	CHECK(poll(&p, 1, 0) == 0);
	CHECK(write(second, "hello", 5) == 5);
	char buffer[8] = {0};
	CHECK(read(a, buffer, sizeof(buffer)) == 5 && !memcmp(buffer, "hello", 5));
	CHECK(send(a, "back", 4, 0) == 4);
	CHECK(recv(second, buffer, sizeof(buffer), 0) == 4 && !memcmp(buffer, "back", 4));
	// recvfrom names the sender.
	CHECK(send(a, "!", 1, 0) == 1);
	struct sockaddr_un from;
	socklen_t from_len = sizeof(from);
	CHECK(recvfrom(second, buffer, sizeof(buffer), 0, (struct sockaddr *)&from, &from_len) == 1);
	CHECK(from_len == addr_len && !strcmp(from.sun_path, NAME));
	// The other end is this process.
	struct ucred cred;
	size = sizeof(cred);
	CHECK(getsockopt(a, SOL_SOCKET, SO_PEERCRED, &cred, &size) == 0);
	CHECK(size == sizeof(cred) && cred.pid == getpid());
	CHECK(cred.uid == getuid() && cred.gid == getgid());
	// Connected and listening sockets stay so.
	CHECK_ERROR(connect(second, (struct sockaddr *)&addr, addr_len), EISCONN);
	CHECK_ERROR(listen(second, 1), EINVAL);
	CHECK_ERROR(connect(s, (struct sockaddr *)&addr, addr_len), EINVAL);
	// Nothing waits: a non-blocking accept says so.
	CHECK(fcntl(s, F_SETFL, O_NONBLOCK) == 0);
	CHECK_ERROR(accept(s, NULL, NULL), EAGAIN);
	CHECK_ERROR(accept(a, NULL, NULL), EINVAL);
	// Once the listening socket is gone, connections are refused, though
	// the name stays until it is removed.
	close(s);
	int error;
	CHECK(connected(0, &error) < 0 && error == ECONNREFUSED);
	CHECK(stat(NAME, &st) == 0 && S_ISSOCK(st.st_mode));
	CHECK(unlink(NAME) == 0);
	// A connection outlives the listening socket.
	CHECK(write(a, "on", 2) == 2);
	CHECK(read(second, buffer, sizeof(buffer)) == 2);
	close(a);
	CHECK(read(second, buffer, sizeof(buffer)) == 0);
	close(second);
	// A relative name is the socket's name as given.
	char cwd[256];
	CHECK(getcwd(cwd, sizeof(cwd)) != NULL);
	CHECK(chdir("/tmp") == 0);
	s = socket(AF_UNIX, SOCK_STREAM, 0);
	addr_len = address_of("socket-here", &addr);
	CHECK(bind(s, (struct sockaddr *)&addr, addr_len) == 0);
	CHECK(named_as(s, 0, "socket-here"));
	CHECK(stat("/tmp/socket-here", &st) == 0 && S_ISSOCK(st.st_mode));
	close(s);
	CHECK(unlink("socket-here") == 0);
	CHECK(chdir(cwd) == 0);
}

// Connections wait in the listening socket's backlog until accept takes
// them: a non-blocking connect to a full one fails, and a blocking one
// waits, here until a child takes one; and the other end of each is the
// process that connected.
static void backlog(void) {
	unlink(NAME);
	int s = listening(0);
	int error;
	int first = connected(0, &error);
	CHECK(first >= 0);
	CHECK(connected(SOCK_NONBLOCK, &error) < 0 && error == EAGAIN);
	pid_t pid = check_fork();
	if(pid == 0) {
		check_sleep_ms(50);
		int a = accept(s, NULL, NULL);
		CHECK(a >= 0);
		struct ucred cred;
		socklen_t size = sizeof(cred);
		CHECK(getsockopt(a, SOL_SOCKET, SO_PEERCRED, &cred, &size) == 0);
		CHECK(cred.pid == getppid());
		CHECK(write(a, "hi", 2) == 2);
		check_exit_child();
	}
	long long start = check_now_ns();
	int second = connected(0, &error);
	CHECK(second >= 0 && check_now_ns() - start >= 40 * 1000000LL);
	char buffer[4];
	CHECK(read(first, buffer, sizeof(buffer)) == 2 && !memcmp(buffer, "hi", 2));
	CHECK_CHILD(pid);
	// The child's end went with it.
	CHECK(read(first, buffer, sizeof(buffer)) == 0);
	int a = accept4(s, NULL, NULL, SOCK_CLOEXEC);
	CHECK(a >= 0 && (fcntl(a, F_GETFD) & FD_CLOEXEC));
	close(a);
	close(first);
	close(second);
	// A child connects, and the accepted socket's peer is the child.
	pid = check_fork();
	if(pid == 0) {
		int c = connected(0, &error);
		CHECK(c >= 0);
		CHECK(write(c, "child", 5) == 5);
		char byte;
		CHECK(read(c, &byte, 1) == 1 && byte == 'k');
		check_exit_child();
	}
	a = accept(s, NULL, NULL);
	CHECK(a >= 0);
	struct ucred cred;
	socklen_t size = sizeof(cred);
	CHECK(getsockopt(a, SOL_SOCKET, SO_PEERCRED, &cred, &size) == 0 && cred.pid == pid);
	CHECK(read(a, buffer, sizeof(buffer)) == 4);
	CHECK(write(a, "k", 1) == 1);
	CHECK_CHILD(pid);
	close(a);
	close(s);
	CHECK(unlink(NAME) == 0);
}

// Sends one byte, `byte`, with the `count` descriptors of `fds`.
static ssize_t send_fds(int socket, char byte, const int *fds, int count) {
	union {
		struct cmsghdr header;
		char room[CMSG_SPACE(253 * sizeof(int))];
	} control;
	struct iovec iov = {&byte, 1};
	struct msghdr message = {.msg_iov = &iov, .msg_iovlen = 1};
	message.msg_control = control.room;
	message.msg_controllen = CMSG_SPACE(count * sizeof(int));
	struct cmsghdr *cmsg = CMSG_FIRSTHDR(&message);
	cmsg->cmsg_level = SOL_SOCKET;
	cmsg->cmsg_type = SCM_RIGHTS;
	cmsg->cmsg_len = CMSG_LEN(count * sizeof(int));
	memcpy(CMSG_DATA(cmsg), fds, count * sizeof(int));
	return sendmsg(socket, &message, 0);
}

// Receives into `buffer` with room for `room` descriptors (at most 8),
// which go in `fds`; returns the bytes, and the descriptors' count in
// `count` and the message's flags in `flags`. The control buffer is just
// long enough: CMSG_SPACE's padding would hold one more.
static ssize_t receive_fds(int socket, char *buffer, size_t size, int *fds, int room, int *count,
		int receive_flags, int *flags) {
	union {
		struct cmsghdr header;
		char room[CMSG_SPACE(8 * sizeof(int))];
	} control;
	struct iovec iov = {buffer, size};
	struct msghdr message = {.msg_iov = &iov, .msg_iovlen = 1};
	message.msg_control = room ? control.room : NULL;
	message.msg_controllen = room ? CMSG_LEN(room * sizeof(int)) : 0;
	ssize_t got = recvmsg(socket, &message, receive_flags);
	*count = 0;
	*flags = message.msg_flags;
	if(got < 0)
		return got;
	for(struct cmsghdr *cmsg = CMSG_FIRSTHDR(&message); cmsg; cmsg = CMSG_NXTHDR(&message, cmsg)) {
		if(cmsg->cmsg_level != SOL_SOCKET || cmsg->cmsg_type != SCM_RIGHTS)
			continue;
		int n = (int)((cmsg->cmsg_len - CMSG_LEN(0)) / sizeof(int));
		memcpy(fds + *count, CMSG_DATA(cmsg), n * sizeof(int));
		*count += n;
	}
	return got;
}

static void passing(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	// A pipe's write end: what goes in it comes out of the read end.
	int pipe_fds[2];
	CHECK(pipe(pipe_fds) == 0);
	CHECK(send_fds(fds[0], 'p', &pipe_fds[1], 1) == 1);
	CHECK(close(pipe_fds[1]) == 0);
	char buffer[16];
	int got[8];
	int count;
	int flags;
	CHECK(receive_fds(fds[1], buffer, sizeof(buffer), got, 1, &count, 0, &flags) == 1);
	CHECK(buffer[0] == 'p' && count == 1 && flags == 0);
	CHECK(!(fcntl(got[0], F_GETFD) & FD_CLOEXEC));
	CHECK(write(got[0], "through", 7) == 7);
	CHECK(read(pipe_fds[0], buffer, sizeof(buffer)) == 7 && !memcmp(buffer, "through", 7));
	close(got[0]);
	CHECK(read(pipe_fds[0], buffer, sizeof(buffer)) == 0);
	close(pipe_fds[0]);
	// A file and a directory at once, the file's description shared: a
	// read through one moves the other's offset.
	int file = open("/tmp/socket-passed", O_RDWR | O_CREAT | O_TRUNC, 0644);
	CHECK(file >= 0 && write(file, "contents", 8) == 8 && lseek(file, 0, SEEK_SET) == 0);
	int dir = open("/tmp", O_RDONLY | O_DIRECTORY);
	CHECK(dir >= 0);
	int both[2] = {file, dir};
	CHECK(send_fds(fds[0], 'f', both, 2) == 1);
	CHECK(receive_fds(fds[1], buffer, sizeof(buffer), got, 2, &count, MSG_CMSG_CLOEXEC, &flags)
			== 1);
	CHECK(count == 2 && !(flags & MSG_CTRUNC));
	CHECK(fcntl(got[0], F_GETFD) & FD_CLOEXEC);
	CHECK(read(got[0], buffer, 3) == 3 && !memcmp(buffer, "con", 3));
	CHECK(lseek(file, 0, SEEK_CUR) == 3);
	struct stat st;
	CHECK(fstat(got[1], &st) == 0 && S_ISDIR(st.st_mode));
	CHECK(fstatat(got[1], "socket-passed", &st, 0) == 0 && st.st_size == 8);
	close(got[0]);
	close(got[1]);
	close(dir);
	// Room for one of two: the other is closed, and MSG_CTRUNC says so.
	CHECK(pipe(pipe_fds) == 0);
	int two[2] = {file, pipe_fds[1]};
	CHECK(send_fds(fds[0], 't', two, 2) == 1);
	close(pipe_fds[1]);
	CHECK(receive_fds(fds[1], buffer, sizeof(buffer), got, 1, &count, 0, &flags) == 1);
	CHECK(count == 1 && (flags & MSG_CTRUNC));
	close(got[0]);
	CHECK(read(pipe_fds[0], buffer, sizeof(buffer)) == 0);
	close(pipe_fds[0]);
	// A plain read closes them too.
	CHECK(pipe(pipe_fds) == 0);
	CHECK(send_fds(fds[0], 'r', &pipe_fds[1], 1) == 1);
	close(pipe_fds[1]);
	CHECK(read(fds[1], buffer, sizeof(buffer)) == 1 && buffer[0] == 'r');
	CHECK(read(pipe_fds[0], buffer, sizeof(buffer)) == 0);
	close(pipe_fds[0]);
	// The descriptors come with the bytes they were sent with: a read
	// ends before them, and the one that starts with them gets them
	// (where Linux's reads on, ADR 0091).
	CHECK(send(fds[0], "ab", 2, 0) == 2);
	CHECK(send_fds(fds[0], 'c', &file, 1) == 1);
	CHECK(send(fds[0], "de", 2, 0) == 2);
	CHECK(recv(fds[1], buffer, sizeof(buffer), 0) == 2 && !memcmp(buffer, "ab", 2));
	CHECK(receive_fds(fds[1], buffer, sizeof(buffer), got, 1, &count, 0, &flags) == 3);
	CHECK(!memcmp(buffer, "cde", 3) && count == 1);
	close(got[0]);
	// Without bytes, nothing is sent.
	struct msghdr empty = {0};
	union {
		struct cmsghdr header;
		char room[CMSG_SPACE(sizeof(int))];
	} control;
	empty.msg_control = control.room;
	empty.msg_controllen = sizeof(control.room);
	struct cmsghdr *cmsg = CMSG_FIRSTHDR(&empty);
	cmsg->cmsg_level = SOL_SOCKET;
	cmsg->cmsg_type = SCM_RIGHTS;
	cmsg->cmsg_len = CMSG_LEN(sizeof(int));
	memcpy(CMSG_DATA(cmsg), &file, sizeof(int));
	CHECK(sendmsg(fds[0], &empty, 0) == 0);
	CHECK_ERROR(recv(fds[1], buffer, sizeof(buffer), MSG_DONTWAIT), EAGAIN);
	// Sockets are not sent (ADR 0091), nor are descriptors that are not.
	CHECK_ERROR(send_fds(fds[0], 's', &fds[1], 1), EOPNOTSUPP);
	int bad = 1000;
	CHECK_ERROR(send_fds(fds[0], 'b', &bad, 1), EBADF);
	CHECK_ERROR(recv(fds[1], buffer, sizeof(buffer), MSG_DONTWAIT), EAGAIN);
	// At most 253 in a message, as on Linux, and 1024 on their way each
	// way at once (ADR 0091).
	int many[253];
	for(int i = 0; i < 253; i++)
		many[i] = file;
	int sent = 0;
	while(sent < 8 && send_fds(fds[0], 'm', many, 253) == 1)
		sent++;
	CHECK(sent == 4 && errno == ETOOMANYREFS);
	CHECK(read(fds[1], buffer, sizeof(buffer)) == 1);
	CHECK(send_fds(fds[0], 'm', many, 253) == 1);
	for(int i = 0; i < 4; i++)
		CHECK(read(fds[1], buffer, sizeof(buffer)) == 1);
	CHECK(send_fds(fds[0], 'm', many, 1) == 1 && read(fds[1], buffer, 1) == 1);
	close(file);
	unlink("/tmp/socket-passed");
	close(fds[0]);
	close(fds[1]);
}

// A child opens a file and sends it to its parent, which reads it after
// the child has gone; and the parent sends the child a pipe to answer on.
static void between(void) {
	int fds[2];
	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		close(fds[0]);
		int file = open("/tmp/socket-child", O_RDWR | O_CREAT | O_TRUNC, 0644);
		CHECK(file >= 0 && write(file, "from child", 10) == 10);
		CHECK(send_fds(fds[1], 'c', &file, 1) == 1);
		close(file);
		char byte;
		int got[1];
		int count;
		int flags;
		CHECK(receive_fds(fds[1], &byte, 1, got, 1, &count, 0, &flags) == 1 && count == 1);
		CHECK(byte == 'a' && write(got[0], "answer", 6) == 6);
		check_exit_child();
	}
	close(fds[1]);
	char buffer[16] = {0};
	int got[1];
	int count;
	int flags;
	CHECK(receive_fds(fds[0], buffer, 1, got, 1, &count, 0, &flags) == 1 && count == 1);
	int pipe_fds[2];
	CHECK(pipe(pipe_fds) == 0);
	CHECK(send_fds(fds[0], 'a', &pipe_fds[1], 1) == 1);
	close(pipe_fds[1]);
	CHECK(read(pipe_fds[0], buffer, sizeof(buffer)) == 6 && !memcmp(buffer, "answer", 6));
	CHECK_CHILD(pid);
	CHECK(read(pipe_fds[0], buffer, sizeof(buffer)) == 0);
	CHECK(pread(got[0], buffer, sizeof(buffer), 0) == 10 && !memcmp(buffer, "from child", 10));
	close(got[0]);
	close(pipe_fds[0]);
	close(fds[0]);
	unlink("/tmp/socket-child");
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
	RUN(named);
	RUN(backlog);
	RUN(passing);
	RUN(between);
	return check_end();
}
