// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Sockets on Radix (ADR 0085): the pairs radix-process(2)'s `socket_pair`
// makes, which are AF_UNIX stream sockets without names, connected to
// each other. Each end is a file description, so read, write, poll, dup,
// fcntl and close treat it as they treat a pipe; send and receive are
// radix-file(2)'s `send` and `receive`, which take MSG_DONTWAIT and
// MSG_NOSIGNAL, and shutdown is its `shutdown`.
//
// Named sockets (bind, listen, connect, accept) and passing descriptors
// come later in Phase 9; other families with networking (Phase 12).

#include <errno.h>
#include <limits.h>
#include <mlibc/all-sysdeps.hpp>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>

#include "radix.hpp"

namespace {

// Whether `status` is the kernel error `code`.
bool is(uint64_t status, uint16_t code) {
	return RADIX_STATUS_DOMAIN(status) == RADIX_KERNEL_ERROR_DOMAIN
			&& RADIX_STATUS_CODE(status) == code;
}

// The errno of a socket call that failed with `status`: a descriptor that
// is not a file (a directory) is not a socket either, and one not open
// for the call's direction is a bad one for it, as for read and write.
int socket_errno(uint64_t status) {
	if(is(status, RADIX_KERNEL_NO_SUCH_METHOD) || is(status, RADIX_KERNEL_WRONG_KIND))
		return ENOTSOCK;
	if(is(status, RADIX_KERNEL_MISSING_RIGHTS))
		return EBADF;
	return radix::to_errno(status);
}

// 0 if `fd` is a socket; else ENOTSOCK, or EBADF if it is no descriptor.
int check_socket(int fd) {
	struct stat st;
	if(int e = mlibc::sysdep<Stat>(mlibc::fsfd_target::fd, fd, "", 0, &st); e)
		return e;
	return S_ISSOCK(st.st_mode) ? 0 : ENOTSOCK;
}

// The flags send takes: the others that Linux takes for a stream socket
// (MSG_OOB, MSG_EOR) are for what a pair does not have.
constexpr int SEND_FLAGS = MSG_DONTWAIT | MSG_NOSIGNAL | MSG_MORE;

// The flags receive takes. MSG_PEEK is not taken yet.
constexpr int RECEIVE_FLAGS = MSG_DONTWAIT | MSG_WAITALL | MSG_CMSG_CLOEXEC;

// Sends `size` bytes from `data` on `fd`, as send(2) with `flags`.
int send_bytes(int fd, const void *data, size_t size, int flags, size_t *sent) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	if(flags & ~SEND_FLAGS)
		return EOPNOTSUPP;
	uint32_t kernel = 0;
	if(flags & MSG_DONTWAIT)
		kernel |= RADIX_SOCKET_FLAGS_DONT_WAIT;
	if(flags & MSG_NOSIGNAL)
		kernel |= RADIX_SOCKET_FLAGS_NO_SIGNAL;
	struct radix_file_send_results r;
	if(uint64_t status = radix_file_send(slot, data, size, kernel, &r); status)
		return socket_errno(status);
	*sent = r.count;
	return 0;
}

// Receives up to `size` bytes on `fd` into `buffer`, waiting for some
// unless `dont_wait`: 0 bytes once the other end has stopped sending.
int receive_bytes(int fd, void *buffer, size_t size, bool dont_wait, size_t *got) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_file_receive_results r;
	uint64_t status = radix_file_receive(slot, buffer, size,
			dont_wait ? RADIX_SOCKET_FLAGS_DONT_WAIT : 0, &r);
	if(status)
		return socket_errno(status);
	*got = r.count;
	return 0;
}

// Receives into the buffers of `iov`, as recvmsg(2) with `flags`: what
// waits now, after waiting for some unless MSG_DONTWAIT; with MSG_WAITALL,
// until the buffers are full, the other end stops sending, or an error
// comes after some bytes (which are then what it returns).
int receive_vector(int fd, const struct iovec *iov, size_t count, int flags, ssize_t *length) {
	if(flags & ~RECEIVE_FLAGS)
		return EOPNOTSUPP;
	bool dont_wait = flags & MSG_DONTWAIT;
	bool wait_all = (flags & MSG_WAITALL) && !dont_wait;
	size_t total = 0;
	for(size_t i = 0; i < count; i++) {
		char *base = static_cast<char *>(iov[i].iov_base);
		size_t at = 0;
		while(at < iov[i].iov_len) {
			// Once some bytes are in, only MSG_WAITALL waits for more.
			size_t got = 0;
			bool now = dont_wait || (total && !wait_all);
			int e = receive_bytes(fd, base + at, iov[i].iov_len - at, now, &got);
			if(e && !total)
				return e;
			at += got;
			total += got;
			// An error after some bytes, the end, or a short receive
			// without MSG_WAITALL: what came is the answer.
			if(e || !got || (!wait_all && at < iov[i].iov_len)) {
				*length = static_cast<ssize_t>(total);
				return 0;
			}
		}
	}
	*length = static_cast<ssize_t>(total);
	return 0;
}

// The two ends of a pair in the lowest free descriptors.
int make_pair(uint32_t flags, int *fds) {
	for(;;) {
		uint32_t first;
		uint32_t second;
		if(int e = radix::free_slot(0, &first); e)
			return e;
		int next = static_cast<int>(first - radix::FD_BASE) + 1;
		if(next >= radix::MAX_FDS)
			return EMFILE;
		if(int e = radix::free_slot(next, &second); e)
			return e;
		uint64_t status = radix_process_socket_pair(radix::SLOT_PROCESS, first, second, flags);
		// Another thread took a slot first.
		if(is(status, RADIX_KERNEL_OCCUPIED))
			continue;
		if(status)
			return radix::to_errno(status);
		fds[0] = static_cast<int>(first - radix::FD_BASE);
		fds[1] = static_cast<int>(second - radix::FD_BASE);
		return 0;
	}
}

// The address of an end, or of its peer: neither has a name, so it is
// only the family, as Linux gives for a pair.
int unnamed(int fd, struct sockaddr *addr, socklen_t max, socklen_t *actual) {
	if(int e = check_socket(fd); e)
		return e;
	sa_family_t family = AF_UNIX;
	if(addr)
		memcpy(addr, &family, max < sizeof(family) ? max : sizeof(family));
	*actual = sizeof(family);
	return 0;
}

} // namespace

namespace mlibc {

int Sysdeps<Socket>::operator()(int family, int type, int protocol, int *fd) {
	(void)family;
	(void)type;
	(void)protocol;
	(void)fd;
	// Only pairs so far: no family makes a socket on its own.
	return EAFNOSUPPORT;
}

int Sysdeps<Socketpair>::operator()(int domain, int type_and_flags, int proto, int *fds) {
	if(domain != AF_UNIX)
		return EAFNOSUPPORT;
	int type = type_and_flags & ~(SOCK_NONBLOCK | SOCK_CLOEXEC);
	switch(type) {
	case SOCK_STREAM: break;
	// Linux's AF_UNIX has these, which Radix does not yet.
	case SOCK_DGRAM:
	case SOCK_SEQPACKET: return ESOCKTNOSUPPORT;
	default: return EINVAL;
	}
	if(proto != 0 && proto != PF_UNIX)
		return EPROTONOSUPPORT;
	uint32_t flags = 0;
	if(type_and_flags & SOCK_NONBLOCK)
		flags |= RADIX_PIPE_FLAGS_NON_BLOCKING;
	if(type_and_flags & SOCK_CLOEXEC)
		flags |= RADIX_PIPE_FLAGS_CLOSE_ON_EXEC;
	return make_pair(flags, fds);
}

int Sysdeps<Sendto>::operator()(int fd, const void *buffer, size_t size, int flags,
		const struct sockaddr *sock_addr, socklen_t addr_length, ssize_t *length) {
	// A pair is connected: an address is one too many, as on Linux.
	if(sock_addr && addr_length) {
		if(int e = check_socket(fd); e)
			return e;
		return EISCONN;
	}
	size_t sent;
	if(int e = send_bytes(fd, buffer, size, flags, &sent); e)
		return e;
	*length = static_cast<ssize_t>(sent);
	return 0;
}

int Sysdeps<MsgSend>::operator()(int fd, const struct msghdr *hdr, int flags, ssize_t *length) {
	if(hdr->msg_name && hdr->msg_namelen) {
		if(int e = check_socket(fd); e)
			return e;
		return EISCONN;
	}
	// Passing descriptors comes with named sockets.
	if(hdr->msg_control && hdr->msg_controllen)
		return EOPNOTSUPP;
	size_t sent;
	if(hdr->msg_iovlen <= 1) {
		const struct iovec *iov = hdr->msg_iovlen ? hdr->msg_iov : nullptr;
		if(int e = send_bytes(fd, iov ? iov->iov_base : nullptr, iov ? iov->iov_len : 0, flags,
				&sent); e)
			return e;
		*length = static_cast<ssize_t>(sent);
		return 0;
	}
	size_t sum = 0;
	for(size_t i = 0; i < hdr->msg_iovlen; i++)
		sum += hdr->msg_iov[i].iov_len;
	// Up to PIPE_BUF bytes go in one send, which the other end gets whole,
	// never mixed with another sender's bytes, as writev does on a pipe.
	if(sum <= PIPE_BUF) {
		char gathered[PIPE_BUF];
		size_t at = 0;
		for(size_t i = 0; i < hdr->msg_iovlen; i++) {
			memcpy(gathered + at, hdr->msg_iov[i].iov_base, hdr->msg_iov[i].iov_len);
			at += hdr->msg_iov[i].iov_len;
		}
		if(int e = send_bytes(fd, gathered, sum, flags, &sent); e)
			return e;
		*length = static_cast<ssize_t>(sent);
		return 0;
	}
	size_t total = 0;
	for(size_t i = 0; i < hdr->msg_iovlen; i++) {
		if(!hdr->msg_iov[i].iov_len)
			continue;
		if(int e = send_bytes(fd, hdr->msg_iov[i].iov_base, hdr->msg_iov[i].iov_len, flags, &sent);
				e) {
			if(total)
				break;
			return e;
		}
		total += sent;
		if(sent < hdr->msg_iov[i].iov_len)
			break;
	}
	*length = static_cast<ssize_t>(total);
	return 0;
}

int Sysdeps<Recvfrom>::operator()(int fd, void *buffer, size_t size, int flags,
		struct sockaddr *sock_addr, socklen_t *addr_length, ssize_t *length) {
	(void)sock_addr;
	struct iovec iov = {buffer, size};
	if(int e = receive_vector(fd, &iov, 1, flags, length); e)
		return e;
	// The other end has no name, so there is no address to give.
	if(addr_length)
		*addr_length = 0;
	return 0;
}

int Sysdeps<MsgRecv>::operator()(int fd, struct msghdr *hdr, int flags, ssize_t *length) {
	if(hdr->msg_iovlen > IOV_MAX)
		return EMSGSIZE;
	if(int e = receive_vector(fd, hdr->msg_iov, hdr->msg_iovlen, flags, length); e)
		return e;
	hdr->msg_namelen = 0;
	hdr->msg_controllen = 0;
	hdr->msg_flags = 0;
	return 0;
}

int Sysdeps<Shutdown>::operator()(int sockfd, int how) {
	uint32_t slot = radix::slot_of(sockfd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	if(how != SHUT_RD && how != SHUT_WR && how != SHUT_RDWR)
		return EINVAL;
	return socket_errno(radix_file_shutdown(slot, how != SHUT_WR, how != SHUT_RD));
}

int Sysdeps<GetSockopt>::operator()(int fd, int layer, int number, void *__restrict buffer,
		socklen_t *__restrict size) {
	if(int e = check_socket(fd); e)
		return e;
	if(layer != SOL_SOCKET)
		return ENOPROTOOPT;
	int value;
	switch(number) {
	case SO_TYPE: value = SOCK_STREAM; break;
	case SO_DOMAIN: value = AF_UNIX; break;
	case SO_PROTOCOL: value = 0; break;
	// Errors come back from each call, so none waits here.
	case SO_ERROR: value = 0; break;
	case SO_ACCEPTCONN: value = 0; break;
	// Each way has a pipe's buffer (radix-posix(7)).
	case SO_RCVBUF:
	case SO_SNDBUF: value = 65536; break;
	default: return ENOPROTOOPT;
	}
	if(*size < sizeof(int))
		return EINVAL;
	memcpy(buffer, &value, sizeof(int));
	*size = sizeof(int);
	return 0;
}

int Sysdeps<SetSockopt>::operator()(int fd, int layer, int number, const void *buffer,
		socklen_t size) {
	(void)buffer;
	if(int e = check_socket(fd); e)
		return e;
	if(layer != SOL_SOCKET)
		return ENOPROTOOPT;
	switch(number) {
	// Taken, and nothing to do: the buffers have one size, as Linux takes
	// a size as a hint, and a pair has no address to reuse and no
	// connection to keep alive.
	case SO_RCVBUF:
	case SO_SNDBUF:
	case SO_REUSEADDR:
	case SO_KEEPALIVE:
		if(size < sizeof(int))
			return EINVAL;
		return 0;
	default: return ENOPROTOOPT;
	}
}

int Sysdeps<Sockname>::operator()(int fd, struct sockaddr *addr_ptr, socklen_t max_addr_length,
		socklen_t *actual_length) {
	return unnamed(fd, addr_ptr, max_addr_length, actual_length);
}

int Sysdeps<Peername>::operator()(int fd, struct sockaddr *addr_ptr, socklen_t max_addr_length,
		socklen_t *actual_length) {
	return unnamed(fd, addr_ptr, max_addr_length, actual_length);
}

} // namespace mlibc
