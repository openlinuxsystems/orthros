// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Sockets on Radix (ADRs 0085 and 0091): AF_UNIX stream sockets. The
// pairs radix-process(2)'s `socket_pair` makes, connected to each other,
// and those its `socket` makes, which bind names through
// radix-directory(2)'s `bind_socket`, connect through `connect_socket`,
// and listen and accept with radix-file(2)'s `listen` and `accept`. Each
// is a file description, so read, write, poll, dup, fcntl and close treat
// it as they treat a pipe; send and receive are radix-file(2)'s `send`
// and `receive`, which take MSG_DONTWAIT and MSG_NOSIGNAL, or with
// SCM_RIGHTS descriptors, `send_files` and `receive_files`; shutdown is
// its `shutdown`.
//
// Other families come with networking (Phase 12).

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <mlibc/all-sysdeps.hpp>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/un.h>

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

// The slot of socket descriptor `fd` in `slot`, or EBADF.
int slot_of(int fd, uint32_t *slot) {
	*slot = radix::slot_of(fd);
	return *slot == radix::NO_SLOT ? EBADF : 0;
}

// The flags send takes: the others that Linux takes for a stream socket
// (MSG_OOB, MSG_EOR) are for what these sockets do not have.
constexpr int SEND_FLAGS = MSG_DONTWAIT | MSG_NOSIGNAL | MSG_MORE;

// The flags receive takes. MSG_PEEK is not taken yet.
constexpr int RECEIVE_FLAGS = MSG_DONTWAIT | MSG_WAITALL | MSG_CMSG_CLOEXEC;

// The kernel's SocketFlags for send's `flags`.
uint32_t send_flags(int flags) {
	uint32_t kernel = 0;
	if(flags & MSG_DONTWAIT)
		kernel |= RADIX_SOCKET_FLAGS_DONT_WAIT;
	if(flags & MSG_NOSIGNAL)
		kernel |= RADIX_SOCKET_FLAGS_NO_SIGNAL;
	return kernel;
}

// Sends `size` bytes from `data` on `fd`, as send(2) with `flags`, with
// the `count` files whose slots `slots` holds riding on the first byte.
int send_bytes(int fd, const void *data, size_t size, int flags, const uint32_t *slots,
		size_t count, size_t *sent) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	if(flags & ~SEND_FLAGS)
		return EOPNOTSUPP;
	uint64_t status;
	if(count) {
		struct radix_file_send_files_results r;
		status = radix_file_send_files(slot, data, size, slots, count * sizeof(uint32_t),
				send_flags(flags), &r);
		*sent = r.count;
	} else {
		struct radix_file_send_results r;
		status = radix_file_send(slot, data, size, send_flags(flags), &r);
		*sent = r.count;
	}
	if(status)
		return socket_errno(status);
	return 0;
}

// What a receive with room for descriptors got: where they go, how many
// fit, how many came, and whether some were lost for want of room.
struct Received {
	uint32_t *slots;
	size_t room;
	size_t count;
	bool lost;
};

// Receives up to `size` bytes on `fd` into `buffer`, waiting for some
// unless `dont_wait`: 0 bytes once the other end has stopped sending.
// With `files`, the descriptors that came with the bytes go there, closed
// by exec if `close_on_exec`.
int receive_bytes(int fd, void *buffer, size_t size, bool dont_wait, Received *files,
		bool close_on_exec, size_t *got) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	uint32_t flags = dont_wait ? RADIX_SOCKET_FLAGS_DONT_WAIT : 0;
	uint64_t status;
	if(files) {
		if(close_on_exec)
			flags |= RADIX_SOCKET_FLAGS_CLOSE_ON_EXEC;
		struct radix_file_receive_files_results r;
		status = radix_file_receive_files(slot, buffer, size, files->slots + files->count,
				(files->room - files->count) * sizeof(uint32_t), flags, &r);
		if(!status) {
			*got = r.count;
			files->count += r.files;
			files->lost = files->lost || r.lost;
		}
	} else {
		struct radix_file_receive_results r;
		status = radix_file_receive(slot, buffer, size, flags, &r);
		*got = r.count;
	}
	if(status)
		return socket_errno(status);
	return 0;
}

// Receives into the buffers of `iov`, as recvmsg(2) with `flags`: what
// waits now, after waiting for some unless MSG_DONTWAIT; with MSG_WAITALL,
// until the buffers are full, the other end stops sending, or an error
// comes after some bytes (which are then what it returns). With `files`,
// the descriptors that come with the bytes go there, and the receive ends
// with the bytes they came with, as on Linux.
int receive_vector(int fd, const struct iovec *iov, size_t count, int flags, Received *files,
		ssize_t *length) {
	if(flags & ~RECEIVE_FLAGS)
		return EOPNOTSUPP;
	bool dont_wait = flags & MSG_DONTWAIT;
	bool wait_all = (flags & MSG_WAITALL) && !dont_wait;
	bool close_on_exec = flags & MSG_CMSG_CLOEXEC;
	size_t total = 0;
	for(size_t i = 0; i < count; i++) {
		char *base = static_cast<char *>(iov[i].iov_base);
		size_t at = 0;
		while(at < iov[i].iov_len) {
			// Once some bytes are in, only MSG_WAITALL waits for more.
			size_t got = 0;
			bool now = dont_wait || (total && !wait_all);
			int e = receive_bytes(fd, base + at, iov[i].iov_len - at, now, files,
					close_on_exec, &got);
			if(e && !total)
				return e;
			at += got;
			total += got;
			// An error after some bytes, the end, descriptors, or a short
			// receive without MSG_WAITALL: what came is the answer.
			bool came = files && (files->count || files->lost);
			if(e || !got || came || (!wait_all && at < iov[i].iov_len)) {
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

// The kernel's PipeFlags for SOCK_NONBLOCK and SOCK_CLOEXEC in `flags`.
uint32_t description_flags(int flags) {
	uint32_t kernel = 0;
	if(flags & SOCK_NONBLOCK)
		kernel |= RADIX_PIPE_FLAGS_NON_BLOCKING;
	if(flags & SOCK_CLOEXEC)
		kernel |= RADIX_PIPE_FLAGS_CLOSE_ON_EXEC;
	return kernel;
}

// The path of `addr`, an AF_UNIX address of `length` bytes, in `out`
// (with room for sun_path and a NUL), and its length in `len`. Linux's
// abstract names, which start with a NUL, are not taken.
int unix_path(const struct sockaddr *addr, socklen_t length, char *out, size_t *len) {
	constexpr size_t start = offsetof(struct sockaddr_un, sun_path);
	if(!addr || length < start || addr->sa_family != AF_UNIX)
		return EINVAL;
	auto un = reinterpret_cast<const struct sockaddr_un *>(addr);
	size_t most = length - start;
	if(most > sizeof(un->sun_path))
		most = sizeof(un->sun_path);
	size_t n = strnlen(un->sun_path, most);
	if(!n)
		return most ? EOPNOTSUPP : EINVAL;
	if(n > RADIX_FILE_SOCKET_NAME_MAX)
		return ENAMETOOLONG;
	memcpy(out, un->sun_path, n);
	out[n] = 0;
	*len = n;
	return 0;
}

// The address of socket `fd`, or with `peer` of the socket at the other
// end of its connection, as getsockname and getpeername give it: the
// family, and the name the socket was bound to, if it has one, with its
// NUL.
int address(int fd, bool peer, struct sockaddr *addr, socklen_t max, socklen_t *actual) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	struct sockaddr_un un;
	memset(&un, 0, sizeof(un));
	un.sun_family = AF_UNIX;
	struct radix_file_socket_name_results r;
	if(uint64_t status = radix_file_socket_name(slot, peer, un.sun_path,
			sizeof(un.sun_path) - 1, &r); status)
		return socket_errno(status);
	size_t len = offsetof(struct sockaddr_un, sun_path);
	if(r.len)
		len += (r.len < sizeof(un.sun_path) ? r.len : sizeof(un.sun_path) - 1) + 1;
	if(addr)
		memcpy(addr, &un, max < len ? max : len);
	*actual = static_cast<socklen_t>(len);
	return 0;
}

// The address recvfrom and recvmsg give: the name of the socket at the
// other end, or nothing (a length of 0) if it has none, as on Linux.
int sender(int fd, struct sockaddr *addr, socklen_t max, socklen_t *actual) {
	if(int e = address(fd, true, addr, max, actual); e)
		return e;
	if(*actual <= offsetof(struct sockaddr_un, sun_path))
		*actual = 0;
	return 0;
}

// The slots of the descriptors in `hdr`'s SCM_RIGHTS messages, in `slots`
// (room for RADIX_FILE_SEND_FILES_MAX), and how many in `count`.
int rights_of(const struct msghdr *hdr, uint32_t *slots, size_t *count) {
	*count = 0;
	if(!hdr->msg_control || !hdr->msg_controllen)
		return 0;
	for(auto cmsg = CMSG_FIRSTHDR(hdr); cmsg; cmsg = CMSG_NXTHDR(hdr, cmsg)) {
		// Credentials and the like are not sent.
		if(cmsg->cmsg_level != SOL_SOCKET || cmsg->cmsg_type != SCM_RIGHTS
				|| cmsg->cmsg_len < CMSG_LEN(0))
			return EINVAL;
		size_t n = (cmsg->cmsg_len - CMSG_LEN(0)) / sizeof(int);
		if(*count + n > RADIX_FILE_SEND_FILES_MAX)
			return EINVAL;
		const unsigned char *data = CMSG_DATA(cmsg);
		for(size_t i = 0; i < n; i++) {
			int fd;
			memcpy(&fd, data + i * sizeof(int), sizeof(int));
			uint32_t slot = radix::slot_of(fd);
			if(slot == radix::NO_SLOT)
				return EBADF;
			slots[(*count)++] = slot;
		}
	}
	return 0;
}

// Fills `hdr`'s control buffer with one SCM_RIGHTS message holding the
// descriptors `files` got, if any came; sets MSG_CTRUNC if some were lost.
// The length it gives is at most the buffer's, as Linux's is when the
// buffer has no room for the last message's padding.
void give_rights(struct msghdr *hdr, const Received &files) {
	size_t room = hdr->msg_controllen;
	hdr->msg_controllen = 0;
	if(files.lost)
		hdr->msg_flags |= MSG_CTRUNC;
	if(!files.count)
		return;
	auto cmsg = reinterpret_cast<struct cmsghdr *>(hdr->msg_control);
	cmsg->cmsg_level = SOL_SOCKET;
	cmsg->cmsg_type = SCM_RIGHTS;
	cmsg->cmsg_len = CMSG_LEN(files.count * sizeof(int));
	unsigned char *data = CMSG_DATA(cmsg);
	for(size_t i = 0; i < files.count; i++) {
		int fd = static_cast<int>(files.slots[i] - radix::FD_BASE);
		// A new descriptor has no directory path until it is given one.
		radix::forget_dir_path(fd);
		memcpy(data + i * sizeof(int), &fd, sizeof(int));
	}
	size_t used = CMSG_SPACE(files.count * sizeof(int));
	hdr->msg_controllen = used < room ? used : room;
}

} // namespace

namespace mlibc {

int Sysdeps<Socket>::operator()(int family, int type_and_flags, int protocol, int *fd) {
	if(family != AF_UNIX)
		return EAFNOSUPPORT;
	int type = type_and_flags & ~(SOCK_NONBLOCK | SOCK_CLOEXEC);
	switch(type) {
	case SOCK_STREAM: break;
	// Linux's AF_UNIX has these, which Radix does not yet.
	case SOCK_DGRAM:
	case SOCK_SEQPACKET: return ESOCKTNOSUPPORT;
	default: return EINVAL;
	}
	if(protocol != 0 && protocol != PF_UNIX)
		return EPROTONOSUPPORT;
	uint32_t flags = description_flags(type_and_flags);
	return radix::place(0, fd, [&](uint32_t slot) {
		return radix_process_socket(radix::SLOT_PROCESS, slot, flags);
	});
}

int Sysdeps<Socketpair>::operator()(int domain, int type_and_flags, int proto, int *fds) {
	if(domain != AF_UNIX)
		return EAFNOSUPPORT;
	int type = type_and_flags & ~(SOCK_NONBLOCK | SOCK_CLOEXEC);
	switch(type) {
	case SOCK_STREAM: break;
	case SOCK_DGRAM:
	case SOCK_SEQPACKET: return ESOCKTNOSUPPORT;
	default: return EINVAL;
	}
	if(proto != 0 && proto != PF_UNIX)
		return EPROTONOSUPPORT;
	return make_pair(description_flags(type_and_flags), fds);
}

int Sysdeps<Bind>::operator()(int fd, const struct sockaddr *addr, socklen_t length) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	char name[sizeof(sockaddr_un::sun_path) + 1];
	size_t len;
	if(int e = unix_path(addr, length, name, &len); e)
		return e;
	radix::Path path;
	if(int e = radix::resolve(AT_FDCWD, name, &path); e)
		return e;
	uint64_t status = radix_directory_bind_socket(path.dir, path.text, path.len, slot, name, len);
	// A file of that name is there already.
	if(RADIX_STATUS_DOMAIN(status) == RADIX_FILE_ERROR_DOMAIN
			&& RADIX_STATUS_CODE(status) == RADIX_FILE_EXISTS)
		return EADDRINUSE;
	if(is(status, RADIX_KERNEL_INVALID_ARGUMENT))
		return EINVAL;
	return status ? socket_errno(status) : 0;
}

int Sysdeps<Listen>::operator()(int fd, int backlog) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	// As on Linux, a backlog out of range is the most there may be.
	uint32_t most = backlog < 0 || static_cast<uint64_t>(backlog) > RADIX_FILE_BACKLOG_MAX
			? static_cast<uint32_t>(RADIX_FILE_BACKLOG_MAX) : static_cast<uint32_t>(backlog);
	uint64_t status = radix_file_listen(slot, most);
	return status ? socket_errno(status) : 0;
}

int Sysdeps<Connect>::operator()(int fd, const struct sockaddr *addr, socklen_t length) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	char name[sizeof(sockaddr_un::sun_path) + 1];
	size_t len;
	if(int e = unix_path(addr, length, name, &len); e)
		return e;
	radix::Path path;
	if(int e = radix::resolve(AT_FDCWD, name, &path); e)
		return e;
	uint64_t status = radix_directory_connect_socket(path.dir, path.text, path.len, slot);
	// Another thread's connect is under way.
	if(is(status, RADIX_KERNEL_BUSY))
		return EALREADY;
	return status ? socket_errno(status) : 0;
}

int Sysdeps<Accept>::operator()(int fd, int *newfd, struct sockaddr *addr, socklen_t *length,
		int flags) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	if(flags & ~(SOCK_NONBLOCK | SOCK_CLOEXEC))
		return EINVAL;
	uint32_t kernel = description_flags(flags);
	for(;;) {
		uint32_t to;
		if(int e = radix::free_slot(0, &to); e)
			return e;
		uint64_t status = radix_file_accept(slot, to, kernel);
		// Another thread took the slot first.
		if(is(status, RADIX_KERNEL_OCCUPIED))
			continue;
		if(status)
			return socket_errno(status);
		*newfd = static_cast<int>(to - radix::FD_BASE);
		radix::forget_dir_path(*newfd);
		break;
	}
	if(addr && length) {
		socklen_t actual;
		if(address(*newfd, true, addr, *length, &actual))
			actual = 0;
		*length = actual;
	}
	return 0;
}

int Sysdeps<Sendto>::operator()(int fd, const void *buffer, size_t size, int flags,
		const struct sockaddr *sock_addr, socklen_t addr_length, ssize_t *length) {
	// A stream socket sends to the socket it is connected to: an address
	// is one too many, as on Linux.
	if(sock_addr && addr_length) {
		if(int e = check_socket(fd); e)
			return e;
		return EISCONN;
	}
	size_t sent;
	if(int e = send_bytes(fd, buffer, size, flags, nullptr, 0, &sent); e)
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
	uint32_t slots[RADIX_FILE_SEND_FILES_MAX];
	size_t files;
	if(int e = rights_of(hdr, slots, &files); e)
		return e;
	size_t sum = 0;
	for(size_t i = 0; i < hdr->msg_iovlen; i++)
		sum += hdr->msg_iov[i].iov_len;
	// Descriptors go with a byte, and a stream with no bytes sends none,
	// as on Linux.
	if(!sum) {
		if(int e = check_socket(fd); e)
			return e;
		*length = 0;
		return 0;
	}
	size_t sent;
	// Up to PIPE_BUF bytes go in one send, which the other end gets whole,
	// never mixed with another sender's bytes, as writev does on a pipe.
	if(hdr->msg_iovlen > 1 && sum <= PIPE_BUF) {
		char gathered[PIPE_BUF];
		size_t at = 0;
		for(size_t i = 0; i < hdr->msg_iovlen; i++) {
			memcpy(gathered + at, hdr->msg_iov[i].iov_base, hdr->msg_iov[i].iov_len);
			at += hdr->msg_iov[i].iov_len;
		}
		if(int e = send_bytes(fd, gathered, sum, flags, slots, files, &sent); e)
			return e;
		*length = static_cast<ssize_t>(sent);
		return 0;
	}
	size_t total = 0;
	for(size_t i = 0; i < hdr->msg_iovlen; i++) {
		if(!hdr->msg_iov[i].iov_len)
			continue;
		// The descriptors go with the first bytes.
		if(int e = send_bytes(fd, hdr->msg_iov[i].iov_base, hdr->msg_iov[i].iov_len, flags,
				total ? nullptr : slots, total ? 0 : files, &sent); e) {
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
	struct iovec iov = {buffer, size};
	if(int e = receive_vector(fd, &iov, 1, flags, nullptr, length); e)
		return e;
	// The sender is the socket at the other end.
	if(addr_length) {
		socklen_t actual;
		if(sender(fd, sock_addr, *addr_length, &actual))
			actual = 0;
		*addr_length = actual;
	}
	return 0;
}

int Sysdeps<MsgRecv>::operator()(int fd, struct msghdr *hdr, int flags, ssize_t *length) {
	if(hdr->msg_iovlen > IOV_MAX)
		return EMSGSIZE;
	// Room for as many descriptors as the control buffer holds: those
	// with nowhere to go are closed, and MSG_CTRUNC says so.
	uint32_t slots[RADIX_FILE_SEND_FILES_MAX];
	Received files = {slots, 0, 0, false};
	if(hdr->msg_control && hdr->msg_controllen >= CMSG_LEN(sizeof(int))) {
		files.room = (hdr->msg_controllen - CMSG_LEN(0)) / sizeof(int);
		if(files.room > RADIX_FILE_SEND_FILES_MAX)
			files.room = RADIX_FILE_SEND_FILES_MAX;
	}
	if(int e = receive_vector(fd, hdr->msg_iov, hdr->msg_iovlen, flags, &files, length); e)
		return e;
	hdr->msg_flags = 0;
	if(hdr->msg_name && hdr->msg_namelen) {
		socklen_t actual;
		if(sender(fd, static_cast<struct sockaddr *>(hdr->msg_name), hdr->msg_namelen,
				&actual))
			actual = 0;
		hdr->msg_namelen = actual;
	} else {
		hdr->msg_namelen = 0;
	}
	give_rights(hdr, files);
	return 0;
}

int Sysdeps<Shutdown>::operator()(int sockfd, int how) {
	uint32_t slot;
	if(int e = slot_of(sockfd, &slot); e)
		return e;
	if(how != SHUT_RD && how != SHUT_WR && how != SHUT_RDWR)
		return EINVAL;
	uint64_t status = radix_file_shutdown(slot, how != SHUT_WR, how != SHUT_RD);
	return status ? socket_errno(status) : 0;
}

int Sysdeps<GetSockopt>::operator()(int fd, int layer, int number, void *__restrict buffer,
		socklen_t *__restrict size) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	struct radix_file_socket_state_results state;
	if(uint64_t status = radix_file_socket_state(slot, &state); status)
		return socket_errno(status);
	if(layer != SOL_SOCKET)
		return ENOPROTOOPT;
	if(number == SO_PEERCRED) {
		// Every process runs as root until Phase 13 brings users.
		struct ucred cred;
		cred.pid = state.connected ? static_cast<pid_t>(state.peer) : 0;
		cred.uid = state.connected ? 0 : static_cast<uid_t>(-1);
		cred.gid = state.connected ? 0 : static_cast<gid_t>(-1);
		socklen_t len = *size < sizeof(cred) ? *size : sizeof(cred);
		memcpy(buffer, &cred, len);
		*size = len;
		return 0;
	}
	int value;
	switch(number) {
	case SO_TYPE: value = SOCK_STREAM; break;
	case SO_DOMAIN: value = AF_UNIX; break;
	case SO_PROTOCOL: value = 0; break;
	// Errors come back from each call, so none waits here.
	case SO_ERROR: value = 0; break;
	case SO_ACCEPTCONN: value = state.listening; break;
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
	// a size as a hint, and a local socket has no address to reuse and no
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
	return address(fd, false, addr_ptr, max_addr_length, actual_length);
}

int Sysdeps<Peername>::operator()(int fd, struct sockaddr *addr_ptr, socklen_t max_addr_length,
		socklen_t *actual_length) {
	return address(fd, true, addr_ptr, max_addr_length, actual_length);
}

} // namespace mlibc
