// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// AF_INET sockets on the kernel's TCP and UDP (ADR 0183), over the
// loopback interface. TCP: bind, listen, connect and accept, their
// addresses, data both ways, shutdown and the end of the stream, the
// options, a connection refused, non-blocking connect and accept with
// poll, a listener shared with a child, and a socket that listens
// unbound. UDP: sendto and recvfrom, datagrams kept whole, sendmsg and
// recvmsg over several buffers, connect, and MSG_DONTWAIT. The errors of
// addresses in use or not on the machine and of families not there. And
// the network capabilities: a child with none cannot make a socket
// (EACCES), one that may only connect out cannot bind a port or listen,
// and one that may only listen on ports 9000 to 9009 cannot connect out
// or bind another port. The runner gets Spark's network capability, and
// this test gets it through fork and exec.

#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <radix/radix.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/uio.h>

#include "check.h"

// 127.0.0.1 and `port`.
static struct sockaddr_in loopback(unsigned short port) {
	struct sockaddr_in in;
	memset(&in, 0, sizeof(in));
	in.sin_family = AF_INET;
	in.sin_port = htons(port);
	in.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
	return in;
}

// The port socket `fd` is bound to.
static unsigned short port_of(int fd) {
	struct sockaddr_in in;
	socklen_t len = sizeof(in);
	CHECK(getsockname(fd, (struct sockaddr *)&in, &len) == 0);
	CHECK(len == sizeof(in) && in.sin_family == AF_INET);
	return ntohs(in.sin_port);
}

// A TCP socket listening on 127.0.0.1, at a port the kernel picks.
static int listener(void) {
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	CHECK(fd >= 0);
	struct sockaddr_in at = loopback(0);
	CHECK(bind(fd, (struct sockaddr *)&at, sizeof(at)) == 0);
	CHECK(listen(fd, 4) == 0);
	return fd;
}

// A TCP socket connected to 127.0.0.1 `port`.
static int connected(unsigned short port) {
	int fd = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
	CHECK(fd >= 0);
	struct sockaddr_in to = loopback(port);
	CHECK(connect(fd, (struct sockaddr *)&to, sizeof(to)) == 0);
	return fd;
}

// The order the benchmarks use: the port read right after bind, and
// TCP_NODELAY set before the connection.
static void bench_order(void) {
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	CHECK(fd >= 0);
	struct sockaddr_in at = loopback(0);
	socklen_t length = sizeof(at);
	CHECK(bind(fd, (struct sockaddr *)&at, sizeof(at)) == 0);
	CHECK(getsockname(fd, (struct sockaddr *)&at, &length) == 0);
	unsigned short port = ntohs(at.sin_port);
	printf("inet: bound to port %u\n", port);
	CHECK(listen(fd, 16) == 0);
	CHECK(port_of(fd) == port);
	int client = socket(AF_INET, SOCK_STREAM, 0);
	int on = 1;
	CHECK(setsockopt(client, IPPROTO_TCP, TCP_NODELAY, &on, sizeof(on)) == 0);
	struct sockaddr_in to = loopback(port);
	CHECK(connect(client, (struct sockaddr *)&to, sizeof(to)) == 0);
	int accepted = accept(fd, NULL, NULL);
	CHECK(accepted >= 0);
	close(client);
	close(accepted);
	close(fd);
}

// More short connections in a row than the loopback interface has
// sockets: each ends at once, in TIME-WAIT, and gives its socket back.
static void short_connections(void) {
	int server = listener();
	unsigned short port = port_of(server);
	for(int i = 0; i < 300; i++) {
		int client = connected(port);
		int accepted = accept(server, NULL, NULL);
		CHECK(accepted >= 0);
		CHECK(close(client) == 0);
		CHECK(close(accepted) == 0);
	}
	CHECK(close(server) == 0);
}

static void tcp(void) {
	int server = listener();
	unsigned short port = port_of(server);
	CHECK(port != 0);
	int value = 0;
	socklen_t size = sizeof(value);
	CHECK(getsockopt(server, SOL_SOCKET, SO_ACCEPTCONN, &value, &size) == 0 && value == 1);
	int client = connected(port);
	struct sockaddr_in peer;
	socklen_t len = sizeof(peer);
	int accepted = accept(server, (struct sockaddr *)&peer, &len);
	CHECK(accepted >= 0);
	// The accepted connection came from the client's port, which the
	// kernel picked when it connected.
	CHECK(len == sizeof(peer) && peer.sin_family == AF_INET);
	CHECK(peer.sin_addr.s_addr == htonl(INADDR_LOOPBACK));
	unsigned short from = port_of(client);
	CHECK(ntohs(peer.sin_port) == from && from >= 49152);
	len = sizeof(peer);
	CHECK(getpeername(client, (struct sockaddr *)&peer, &len) == 0);
	CHECK(ntohs(peer.sin_port) == port);
	CHECK(port_of(accepted) == port);
	// Bytes both ways, with write and read, send and recv.
	CHECK(write(client, "ping", 4) == 4);
	char buffer[64] = {0};
	CHECK(read(accepted, buffer, sizeof(buffer)) == 4 && !memcmp(buffer, "ping", 4));
	CHECK(send(accepted, "pong!", 5, 0) == 5);
	CHECK(recv(client, buffer, 5, MSG_WAITALL) == 5 && !memcmp(buffer, "pong!", 5));
	// recvfrom on a stream says nothing of where the bytes came from.
	CHECK(send(accepted, "abc", 3, 0) == 3);
	struct sockaddr_in where;
	socklen_t where_len = sizeof(where);
	CHECK(recvfrom(client, buffer, sizeof(buffer), 0, (struct sockaddr *)&where, &where_len)
			== 3);
	CHECK(where_len == 0);
	// What getsockopt says, and the options it takes.
	CHECK(getsockopt(client, SOL_SOCKET, SO_TYPE, &value, &size) == 0 && value == SOCK_STREAM);
	CHECK(getsockopt(client, SOL_SOCKET, SO_DOMAIN, &value, &size) == 0 && value == AF_INET);
	CHECK(getsockopt(client, SOL_SOCKET, SO_PROTOCOL, &value, &size) == 0
			&& value == IPPROTO_TCP);
	CHECK(getsockopt(client, SOL_SOCKET, SO_ERROR, &value, &size) == 0 && value == 0);
	value = 1;
	CHECK(setsockopt(client, IPPROTO_TCP, TCP_NODELAY, &value, sizeof(value)) == 0);
	CHECK(getsockopt(client, IPPROTO_TCP, TCP_NODELAY, &value, &size) == 0 && value == 1);
	value = 1;
	CHECK(setsockopt(client, SOL_SOCKET, SO_KEEPALIVE, &value, sizeof(value)) == 0);
	CHECK(setsockopt(server, SOL_SOCKET, SO_REUSEADDR, &value, sizeof(value)) == 0);
	value = 32;
	CHECK(setsockopt(client, IPPROTO_IP, IP_TTL, &value, sizeof(value)) == 0);
	CHECK(getsockopt(client, IPPROTO_IP, IP_TTL, &value, &size) == 0 && value == 32);
	struct timeval tv = {2, 500000};
	CHECK(setsockopt(client, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) == 0);
	memset(&tv, 0, sizeof(tv));
	socklen_t tv_size = sizeof(tv);
	CHECK(getsockopt(client, SOL_SOCKET, SO_RCVTIMEO, &tv, &tv_size) == 0);
	CHECK(tv.tv_sec == 2 && tv.tv_usec == 500000);
	CHECK_ERROR(setsockopt(client, SOL_SOCKET, SO_PEERCRED, &value, sizeof(value)), ENOPROTOOPT);
	// FIONREAD counts what waits.
	CHECK(write(accepted, "12345", 5) == 5);
	check_sleep_ms(20);
	int readable = -1;
	CHECK(ioctl(client, FIONREAD, &readable) == 0 && readable == 5);
	CHECK(read(client, buffer, sizeof(buffer)) == 5);
	// The client stops sending: the other end reads the end, and can
	// still send back.
	CHECK(shutdown(client, SHUT_WR) == 0);
	CHECK(read(accepted, buffer, sizeof(buffer)) == 0);
	CHECK(write(accepted, "last", 4) == 4);
	CHECK(read(client, buffer, sizeof(buffer)) == 4 && !memcmp(buffer, "last", 4));
	close(accepted);
	CHECK(read(client, buffer, sizeof(buffer)) == 0);
	close(client);
	close(server);
	// Nothing listens there now.
	int refused = socket(AF_INET, SOCK_STREAM, 0);
	struct sockaddr_in to = loopback(port);
	CHECK_ERROR(connect(refused, (struct sockaddr *)&to, sizeof(to)), ECONNREFUSED);
	close(refused);
}

static void non_blocking(void) {
	int server = listener();
	CHECK(fcntl(server, F_SETFL, O_NONBLOCK) == 0);
	CHECK_ERROR(accept(server, NULL, NULL), EAGAIN);
	int client = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
	CHECK(client >= 0);
	CHECK((fcntl(client, F_GETFD) & FD_CLOEXEC) && (fcntl(client, F_GETFL) & O_NONBLOCK));
	struct sockaddr_in to = loopback(port_of(server));
	CHECK_ERROR(connect(client, (struct sockaddr *)&to, sizeof(to)), EINPROGRESS);
	// Writable once connected, with no error.
	struct pollfd p = {client, POLLOUT, 0};
	CHECK(poll(&p, 1, 5000) == 1 && (p.revents & POLLOUT));
	int value = -1;
	socklen_t size = sizeof(value);
	CHECK(getsockopt(client, SOL_SOCKET, SO_ERROR, &value, &size) == 0 && value == 0);
	CHECK_ERROR(connect(client, (struct sockaddr *)&to, sizeof(to)), EISCONN);
	// The listener is readable with a connection to take.
	struct pollfd q = {server, POLLIN, 0};
	CHECK(poll(&q, 1, 5000) == 1 && (q.revents & POLLIN));
	int accepted = accept4(server, NULL, NULL, SOCK_NONBLOCK);
	CHECK(accepted >= 0);
	char buffer[8];
	CHECK_ERROR(read(accepted, buffer, sizeof(buffer)), EAGAIN);
	CHECK_ERROR(recv(client, buffer, sizeof(buffer), MSG_DONTWAIT), EAGAIN);
	CHECK(send(client, "x", 1, 0) == 1);
	struct pollfd r = {accepted, POLLIN, 0};
	CHECK(poll(&r, 1, 5000) == 1 && (r.revents & POLLIN));
	CHECK(read(accepted, buffer, sizeof(buffer)) == 1);
	close(accepted);
	close(client);
	close(server);
}

static void unbound(void) {
	// A socket that listens unbound is bound to a port the kernel picks,
	// as on Linux.
	int server = socket(AF_INET, SOCK_STREAM, 0);
	CHECK(listen(server, 1) == 0);
	unsigned short port = port_of(server);
	CHECK(port >= 49152);
	int client = connected(port);
	int accepted = accept(server, NULL, NULL);
	CHECK(accepted >= 0);
	close(accepted);
	close(client);
	close(server);
}

static void shared(void) {
	// A child, which has the network capabilities through fork, connects
	// to the parent's listener.
	int server = listener();
	unsigned short port = port_of(server);
	pid_t pid = check_fork();
	if(pid == 0) {
		int client = connected(port);
		CHECK(write(client, "from child", 10) == 10);
		close(client);
		check_exit_child();
	}
	int accepted = accept(server, NULL, NULL);
	CHECK(accepted >= 0);
	char buffer[16] = {0};
	size_t got = 0;
	ssize_t n;
	while((n = read(accepted, buffer + got, sizeof(buffer) - got)) > 0)
		got += (size_t)n;
	CHECK(got == 10 && !memcmp(buffer, "from child", 10));
	CHECK_CHILD(pid);
	close(accepted);
	close(server);
}

static void udp(void) {
	int a = socket(AF_INET, SOCK_DGRAM, 0);
	int b = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
	CHECK(a >= 0 && b >= 0);
	struct sockaddr_in at = loopback(0);
	CHECK(bind(a, (struct sockaddr *)&at, sizeof(at)) == 0);
	CHECK(bind(b, (struct sockaddr *)&at, sizeof(at)) == 0);
	struct sockaddr_in to_a = loopback(port_of(a));
	struct sockaddr_in to_b = loopback(port_of(b));
	int value = 0;
	socklen_t size = sizeof(value);
	CHECK(getsockopt(a, SOL_SOCKET, SO_TYPE, &value, &size) == 0 && value == SOCK_DGRAM);
	CHECK(getsockopt(a, SOL_SOCKET, SO_PROTOCOL, &value, &size) == 0 && value == IPPROTO_UDP);
	// Two datagrams, each received whole and alone, with who sent it.
	CHECK(sendto(a, "first", 5, 0, (struct sockaddr *)&to_b, sizeof(to_b)) == 5);
	CHECK(sendto(a, "second!", 7, 0, (struct sockaddr *)&to_b, sizeof(to_b)) == 7);
	char buffer[64];
	struct sockaddr_in from;
	socklen_t from_len = sizeof(from);
	CHECK(recvfrom(b, buffer, sizeof(buffer), 0, (struct sockaddr *)&from, &from_len) == 5);
	CHECK(!memcmp(buffer, "first", 5) && from_len == sizeof(from));
	CHECK(ntohs(from.sin_port) == port_of(a) && from.sin_addr.s_addr == htonl(INADDR_LOOPBACK));
	CHECK(recv(b, buffer, sizeof(buffer), 0) == 7 && !memcmp(buffer, "second!", 7));
	CHECK_ERROR(recv(b, buffer, sizeof(buffer), MSG_DONTWAIT), EAGAIN);
	// sendmsg gathers its buffers into one datagram; recvmsg spreads one
	// over its buffers.
	struct iovec out[2] = {{(void *)"gat", 3}, {(void *)"hered", 5}};
	struct msghdr hdr;
	memset(&hdr, 0, sizeof(hdr));
	hdr.msg_name = &to_a;
	hdr.msg_namelen = sizeof(to_a);
	hdr.msg_iov = out;
	hdr.msg_iovlen = 2;
	CHECK(sendmsg(b, &hdr, 0) == 8);
	char first[4], second[8];
	struct iovec in[2] = {{first, sizeof(first)}, {second, sizeof(second)}};
	memset(&hdr, 0, sizeof(hdr));
	hdr.msg_name = &from;
	hdr.msg_namelen = sizeof(from);
	hdr.msg_iov = in;
	hdr.msg_iovlen = 2;
	CHECK(recvmsg(a, &hdr, 0) == 8);
	CHECK(!memcmp(first, "gath", 4) && !memcmp(second, "ered", 4));
	CHECK(hdr.msg_namelen == sizeof(from) && ntohs(from.sin_port) == port_of(b));
	// Connected, send and recv need no address.
	CHECK(connect(a, (struct sockaddr *)&to_b, sizeof(to_b)) == 0);
	struct sockaddr_in peer;
	socklen_t len = sizeof(peer);
	CHECK(getpeername(a, (struct sockaddr *)&peer, &len) == 0 && ntohs(peer.sin_port) == port_of(b));
	CHECK(send(a, "linked", 6, 0) == 6);
	CHECK(read(b, buffer, sizeof(buffer)) == 6 && !memcmp(buffer, "linked", 6));
	// A datagram socket is writable, and readable once one waits.
	struct pollfd p = {b, POLLIN | POLLOUT, 0};
	CHECK(poll(&p, 1, 0) == 1 && p.revents == POLLOUT);
	// The loopback interface's thread hands the datagram over after send
	// returns, so poll waits for it.
	CHECK(send(a, "!", 1, 0) == 1);
	p.events = POLLIN;
	CHECK(poll(&p, 1, 5000) == 1 && (p.revents & POLLIN));
	CHECK(recv(b, buffer, sizeof(buffer), 0) == 1);
	close(a);
	close(b);
}

static void errors(void) {
	int a = socket(AF_INET, SOCK_STREAM, 0);
	int b = socket(AF_INET, SOCK_STREAM, 0);
	struct sockaddr_in at = loopback(0);
	CHECK(bind(a, (struct sockaddr *)&at, sizeof(at)) == 0);
	// One socket to a port.
	at = loopback(port_of(a));
	CHECK_ERROR(bind(b, (struct sockaddr *)&at, sizeof(at)), EADDRINUSE);
	// An address the machine does not have.
	struct sockaddr_in away = loopback(0);
	away.sin_addr.s_addr = inet_addr("10.9.9.9");
	CHECK_ERROR(bind(b, (struct sockaddr *)&away, sizeof(away)), EADDRNOTAVAIL);
	// An address too short, and one of another family.
	CHECK_ERROR(bind(b, (struct sockaddr *)&at, sizeof(at) - 1), EINVAL);
	struct sockaddr_in other = at;
	other.sin_family = AF_UNIX;
	CHECK_ERROR(connect(b, (struct sockaddr *)&other, sizeof(other)), EAFNOSUPPORT);
	close(a);
	close(b);
	CHECK_ERROR(socket(AF_INET6, SOCK_STREAM, 0), EAFNOSUPPORT);
	CHECK_ERROR(socket(AF_INET, SOCK_STREAM, IPPROTO_UDP), EPROTONOSUPPORT);
	CHECK_ERROR(socket(AF_INET, SOCK_RAW, IPPROTO_ICMP), EPERM);
	// A datagram goes somewhere.
	int d = socket(AF_INET, SOCK_DGRAM, 0);
	CHECK_ERROR(send(d, "x", 1, 0), ENOTCONN);
	close(d);
}

// Deletes every network capability the process has, but the one in slot
// `keep` (0 for none).
static void drop_network(uint32_t keep) {
	for(uint32_t i = 0; i < RADIX_STARTUP_NETWORK_SLOTS; i++) {
		uint32_t slot = RADIX_STARTUP_SLOT_NETWORK + i;
		if(slot != keep)
			radix_capability_delete(slot);
	}
}

// Derives a network capability for `count` ports from `first` with
// `rights`, from the one in SLOT_NETWORK, into the last network slot,
// and keeps only that one.
static void only(uint32_t first, uint32_t count, uint64_t rights) {
	uint32_t slot = RADIX_STARTUP_SLOT_NETWORK + RADIX_STARTUP_NETWORK_SLOTS - 1;
	radix_capability_delete(slot);
	uint64_t kept = RADIX_RIGHTS_COPY | RADIX_RIGHTS_TRANSFER;
	CHECK(radix_network_restrict(RADIX_STARTUP_SLOT_NETWORK, slot, first, count, rights | kept)
			== 0);
	drop_network(slot);
}

static void capabilities(void) {
	// The runner was given the network, and so was this test.
	struct radix_network_ports_results ports;
	CHECK(radix_network_ports(RADIX_STARTUP_SLOT_NETWORK, &ports) == 0);
	CHECK(ports.first == 0 && ports.count == 65536);
	int server = listener();
	unsigned short port = port_of(server);
	// Without a network capability no socket can be made.
	pid_t pid = check_fork();
	if(pid == 0) {
		drop_network(0);
		CHECK_ERROR(socket(AF_INET, SOCK_STREAM, 0), EACCES);
		CHECK_ERROR(socket(AF_INET, SOCK_DGRAM, 0), EACCES);
		// Unix sockets need none.
		int fds[2];
		CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
		check_exit_child();
	}
	CHECK_CHILD(pid);
	// Allowed only to connect out: it connects, but cannot bind a port
	// of its own or listen.
	pid = check_fork();
	if(pid == 0) {
		only(0, 65536, RADIX_RIGHTS_SEND);
		int client = connected(port);
		CHECK(write(client, "out", 3) == 3);
		close(client);
		int fd = socket(AF_INET, SOCK_STREAM, 0);
		struct sockaddr_in at = loopback(9000);
		CHECK_ERROR(bind(fd, (struct sockaddr *)&at, sizeof(at)), EACCES);
		CHECK_ERROR(listen(fd, 1), EACCES);
		close(fd);
		check_exit_child();
	}
	int accepted = accept(server, NULL, NULL);
	CHECK(accepted >= 0);
	char buffer[8];
	CHECK(read(accepted, buffer, sizeof(buffer)) == 3);
	close(accepted);
	CHECK_CHILD(pid);
	// Allowed only to listen on ports 9000 to 9009: it binds and listens
	// there, but cannot connect out, bind another port or send a datagram.
	pid = check_fork();
	if(pid == 0) {
		only(9000, 10, RADIX_RIGHTS_RECEIVE);
		int fd = socket(AF_INET, SOCK_STREAM, 0);
		struct sockaddr_in at = loopback(9005);
		CHECK(bind(fd, (struct sockaddr *)&at, sizeof(at)) == 0);
		CHECK(listen(fd, 1) == 0);
		close(fd);
		int other = socket(AF_INET, SOCK_STREAM, 0);
		at = loopback(9010);
		CHECK_ERROR(bind(other, (struct sockaddr *)&at, sizeof(at)), EACCES);
		struct sockaddr_in to = loopback(port);
		CHECK_ERROR(connect(other, (struct sockaddr *)&to, sizeof(to)), EACCES);
		close(other);
		int d = socket(AF_INET, SOCK_DGRAM, 0);
		CHECK_ERROR(sendto(d, "x", 1, 0, (struct sockaddr *)&to, sizeof(to)), EACCES);
		close(d);
		check_exit_child();
	}
	CHECK_CHILD(pid);
	close(server);
}

int main(void) {
	check_start("inet");
	RUN(bench_order);
	RUN(short_connections);
	RUN(tcp);
	RUN(non_blocking);
	RUN(unbound);
	RUN(shared);
	RUN(udp);
	RUN(errors);
	RUN(capabilities);
	return check_end();
}
