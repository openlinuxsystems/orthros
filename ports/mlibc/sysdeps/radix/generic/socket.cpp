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
// AF_INET stream and datagram sockets are the kernel's TCP and UDP
// (ADR 0183), made, bound, connected and sent from through the network
// capabilities in the NETWORK_SLOTS slots from SLOT_NETWORK
// (radix-network(2)). Each such call tries them in turn until one allows
// it; when none does, it fails with EACCES, as Linux's calls do when a
// policy forbids them. A program given none cannot use the network.
// AF_INET6 comes with IPv6 (Phase 12c).

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <mlibc/all-sysdeps.hpp>
#include <mlibc/threads.hpp>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/time.h>
#include <stdlib.h>

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
// the `count` files whose slots `slots` holds riding on the first byte,
// made with `syscall`.
int send_bytes(radix_syscall_fn *syscall, int fd, const void *data, size_t size, int flags,
		const uint32_t *slots, size_t count, size_t *sent) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	if(flags & ~SEND_FLAGS)
		return EOPNOTSUPP;
	uint64_t status;
	if(count) {
		struct radix_file_send_files_results r;
		status = radix_file_send_files_with(syscall, slot, data, size, slots,
				count * sizeof(uint32_t), send_flags(flags), &r);
		*sent = r.count;
	} else {
		struct radix_file_send_results r;
		status = radix_file_send_with(syscall, slot, data, size, send_flags(flags), &r);
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
// by exec if `close_on_exec`. Made with `syscall`.
int receive_bytes(radix_syscall_fn *syscall, int fd, void *buffer, size_t size, bool dont_wait,
		Received *files, bool close_on_exec, size_t *got) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	uint32_t flags = dont_wait ? RADIX_SOCKET_FLAGS_DONT_WAIT : 0;
	uint64_t status;
	if(files) {
		if(close_on_exec)
			flags |= RADIX_SOCKET_FLAGS_CLOSE_ON_EXEC;
		struct radix_file_receive_files_results r;
		status = radix_file_receive_files_with(syscall, slot, buffer, size,
				files->slots + files->count, (files->room - files->count) * sizeof(uint32_t),
				flags, &r);
		if(!status) {
			*got = r.count;
			files->count += r.files;
			files->lost = files->lost || r.lost;
		}
	} else {
		struct radix_file_receive_results r;
		status = radix_file_receive_with(syscall, slot, buffer, size, flags, &r);
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
// with the bytes they came with, as on Linux. A cancellation point until
// bytes come.
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
			int e = receive_bytes(total ? radix_syscall : radix::cancellable, fd, base + at,
					iov[i].iov_len - at, now, files, close_on_exec, &got);
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
	// With no room for a byte it is a cancellation point all the same.
	if(!total)
		mlibc::thread_testcancel();
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

bool is_inet(uint32_t slot, int *type = nullptr);
int inet_name(uint32_t slot, bool peer, struct sockaddr *addr, socklen_t max, socklen_t *actual);

// The address of socket `fd`, or with `peer` of the socket at the other
// end of its connection, as getsockname and getpeername give it: the
// family, and the name the socket was bound to, if it has one, with its
// NUL; or a network socket's address and port.
int address(int fd, bool peer, struct sockaddr *addr, socklen_t max, socklen_t *actual) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	if(is_inet(slot))
		return inet_name(slot, peer, addr, max, actual);
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

// Whether socket `slot` is a network socket, and its type in `type`
// (SOCK_STREAM or SOCK_DGRAM) if it is: a Unix socket has no options.
bool is_inet(uint32_t slot, int *type) {
	struct radix_file_socket_option_results r;
	if(radix_file_socket_option(slot, RADIX_SOCKET_OPTION_KIND, false, 0, &r))
		return false;
	if(type)
		*type = r.value == RADIX_SOCKET_TYPE_DATAGRAM ? SOCK_DGRAM : SOCK_STREAM;
	return true;
}

// Whether a call failed with `status` because the network capability in
// the slot it was given does not allow it, or there is none there: the
// next one may.
bool not_allowed(uint64_t status) {
	if(RADIX_STATUS_DOMAIN(status) != RADIX_KERNEL_ERROR_DOMAIN)
		return false;
	switch(RADIX_STATUS_CODE(status)) {
	case RADIX_KERNEL_EMPTY_SLOT:
	case RADIX_KERNEL_NO_SUCH_SLOT:
	case RADIX_KERNEL_WRONG_KIND:
	case RADIX_KERNEL_OUT_OF_RANGE: return true;
	// Without the send or receive right; a socket's own descriptor without
	// the right a call needs (read or write) is EBADF, as for the others.
	case RADIX_KERNEL_MISSING_RIGHTS:
		return RADIX_STATUS_DETAIL(status) & (RADIX_RIGHTS_SEND | RADIX_RIGHTS_RECEIVE);
	default: return false;
	}
}

// The errno of a network socket call that failed with `status`.
int inet_errno(uint64_t status) {
	if(not_allowed(status))
		return EACCES;
	return socket_errno(status);
}

// Makes `call(network)` with each network capability's slot in turn,
// until one allows it: 0, or the errno of the first failure that is not
// a refusal, or EACCES if every one refused.
template<typename F>
int through_network(F call) {
	for(uint32_t i = 0; i < RADIX_STARTUP_NETWORK_SLOTS; i++) {
		uint64_t status = call(RADIX_STARTUP_SLOT_NETWORK + i);
		if(!status)
			return 0;
		if(!not_allowed(status))
			return inet_errno(status);
	}
	return EACCES;
}

// The kernel's address for `addr`, an AF_INET address of `length` bytes.
int inet_address(const struct sockaddr *addr, socklen_t length, struct radix_address *out) {
	if(!addr || length < sizeof(struct sockaddr_in))
		return EINVAL;
	if(addr->sa_family != AF_INET)
		return EAFNOSUPPORT;
	auto in = reinterpret_cast<const struct sockaddr_in *>(addr);
	memset(out, 0, sizeof(*out));
	out->family = RADIX_ADDRESS_FAMILY_IPV4;
	out->port = ntohs(in->sin_port);
	memcpy(out->ip, &in->sin_addr, sizeof(in->sin_addr));
	return 0;
}

// Copies the AF_INET address of the kernel's `address` to `addr`, at most
// `max` bytes of it, and its length to `actual`.
void give_inet(const struct radix_address &address, struct sockaddr *addr, socklen_t max,
		socklen_t *actual) {
	struct sockaddr_in in;
	memset(&in, 0, sizeof(in));
	in.sin_family = AF_INET;
	in.sin_port = htons(address.port);
	memcpy(&in.sin_addr, address.ip, sizeof(in.sin_addr));
	if(addr)
		memcpy(addr, &in, max < sizeof(in) ? max : sizeof(in));
	*actual = sizeof(in);
}

// The address of network socket `slot`, or with `peer` where it is
// connected, as getsockname and getpeername give it.
int inet_name(uint32_t slot, bool peer, struct sockaddr *addr, socklen_t max, socklen_t *actual) {
	struct radix_address address;
	struct radix_file_socket_name_results r;
	if(uint64_t status = radix_file_socket_name(slot, peer, &address, sizeof(address), &r);
			status)
		return inet_errno(status);
	give_inet(address, addr, max, actual);
	return 0;
}

// Binds network socket `slot` to any address and a port the kernel picks,
// through a network capability with the receive right, as Linux binds a
// socket that listens unbound. EACCES if there is none.
int bind_to_listen(uint32_t slot) {
	struct radix_address any;
	memset(&any, 0, sizeof(any));
	any.family = RADIX_ADDRESS_FAMILY_IPV4;
	for(uint32_t i = 0; i < RADIX_STARTUP_NETWORK_SLOTS; i++) {
		uint32_t network = RADIX_STARTUP_SLOT_NETWORK + i;
		struct radix_capability_info_results info;
		if(radix_capability_info(network, &info) || info.kind != RADIX_OBJECT_KIND_NETWORK
				|| !(info.rights & RADIX_RIGHTS_RECEIVE))
			continue;
		uint64_t status = radix_file_bind_address(slot, network, &any, sizeof(any));
		return status ? inet_errno(status) : 0;
	}
	return EACCES;
}

// The bytes of the `count` buffers at `iov` gathered into one buffer of
// `size` bytes from malloc, in `out`: a datagram is sent in one call.
int gather(const struct iovec *iov, int count, size_t size, char **out) {
	*out = static_cast<char *>(malloc(size ? size : 1));
	if(!*out)
		return ENOBUFS;
	size_t at = 0;
	for(int i = 0; i < count; i++) {
		memcpy(*out + at, iov[i].iov_base, iov[i].iov_len);
		at += iov[i].iov_len;
	}
	return 0;
}

// The kernel's SocketFlags for a network socket's receive `flags`: only
// MSG_DONTWAIT means anything to a datagram's receive.
uint32_t receive_flags(int flags) {
	return flags & MSG_DONTWAIT ? RADIX_SOCKET_FLAGS_DONT_WAIT : 0;
}

// Receives one datagram on network socket `slot` into the buffers of
// `iov`, as recvmsg(2) with `flags`, and who sent it into `from` (up to
// `from_max` bytes, its length in `from_len`; nothing if `from` is null).
int receive_datagram(uint32_t slot, const struct iovec *iov, int count, int flags,
		struct sockaddr *from, socklen_t from_max, socklen_t *from_len, ssize_t *length) {
	if(flags & ~(MSG_DONTWAIT | MSG_CMSG_CLOEXEC))
		return EOPNOTSUPP;
	size_t size;
	if(int e = radix::iov_total(iov, count, &size); e)
		return e;
	// One buffer is received into in place, more through one of the sum's
	// size, as the datagram comes whole.
	char *buffer = count == 1 ? static_cast<char *>(iov[0].iov_base) : nullptr;
	if(count != 1) {
		buffer = static_cast<char *>(malloc(size ? size : 1));
		if(!buffer)
			return ENOBUFS;
	}
	struct radix_address address;
	memset(&address, 0, sizeof(address));
	struct radix_file_receive_from_results r;
	uint64_t status = radix_file_receive_from_with(radix::cancellable, slot, buffer, size,
			&address, sizeof(address), receive_flags(flags), &r);
	if(!status && count != 1) {
		size_t at = 0;
		for(int i = 0; i < count && at < r.count; i++) {
			size_t n = r.count - at < iov[i].iov_len ? r.count - at : iov[i].iov_len;
			memcpy(iov[i].iov_base, buffer + at, n);
			at += n;
		}
	}
	if(count != 1)
		free(buffer);
	if(status)
		return inet_errno(status);
	if(from_len) {
		if(from)
			give_inet(address, from, from_max, from_len);
		else
			*from_len = 0;
	}
	*length = static_cast<ssize_t>(r.count);
	return 0;
}

// Sends `size` bytes from `data` on network socket `slot` as send(2) with
// `flags`, to `to` (an AF_INET address of `length` bytes) if it is not
// null, through a network capability that allows it.
int send_inet(uint32_t slot, const void *data, size_t size, int flags,
		const struct sockaddr *to, socklen_t length, ssize_t *sent) {
	if(flags & ~SEND_FLAGS)
		return EOPNOTSUPP;
	if(!to) {
		struct radix_file_send_results r;
		uint64_t status = radix_file_send_with(radix::cancellable, slot, data, size,
				send_flags(flags), &r);
		if(status)
			return inet_errno(status);
		*sent = static_cast<ssize_t>(r.count);
		return 0;
	}
	struct radix_address address;
	if(int e = inet_address(to, length, &address); e)
		return e;
	struct radix_file_send_to_results r;
	if(int e = through_network([&](uint32_t network) {
			return radix_file_send_to_with(radix::cancellable, slot, network, &address,
					sizeof(address), data, size, send_flags(flags), &r);
		}); e)
		return e;
	*sent = static_cast<ssize_t>(r.count);
	return 0;
}

// Reads network socket option `option` of `slot` into `value`.
int get_option(uint32_t slot, enum radix_socket_option option, uint64_t *value) {
	struct radix_file_socket_option_results r;
	if(uint64_t status = radix_file_socket_option(slot, option, false, 0, &r); status)
		return inet_errno(status);
	*value = r.value;
	return 0;
}

// Sets network socket option `option` of `slot` to `value`.
int set_option(uint32_t slot, enum radix_socket_option option, uint64_t value) {
	struct radix_file_socket_option_results r;
	uint64_t status = radix_file_socket_option(slot, option, true, value, &r);
	return status ? inet_errno(status) : 0;
}

// The kernel's option for `layer` and `number`, and whether it is a time
// (a struct timeval, in nanoseconds to the kernel): false for one it does
// not have.
bool inet_option(int layer, int number, enum radix_socket_option *option, bool *time) {
	*time = false;
	if(layer == SOL_SOCKET) {
		switch(number) {
		case SO_REUSEADDR: *option = RADIX_SOCKET_OPTION_REUSE_ADDRESS; return true;
		case SO_KEEPALIVE: *option = RADIX_SOCKET_OPTION_KEEP_ALIVE; return true;
		case SO_BROADCAST: *option = RADIX_SOCKET_OPTION_BROADCAST; return true;
		case SO_RCVBUF: *option = RADIX_SOCKET_OPTION_RECEIVE_BUFFER; return true;
		case SO_SNDBUF: *option = RADIX_SOCKET_OPTION_SEND_BUFFER; return true;
		case SO_ERROR: *option = RADIX_SOCKET_OPTION_ERROR; return true;
		case SO_RCVTIMEO:
			*option = RADIX_SOCKET_OPTION_RECEIVE_TIMEOUT;
			*time = true;
			return true;
		case SO_SNDTIMEO:
			*option = RADIX_SOCKET_OPTION_SEND_TIMEOUT;
			*time = true;
			return true;
		default: return false;
		}
	}
	if(layer == IPPROTO_TCP && number == TCP_NODELAY) {
		*option = RADIX_SOCKET_OPTION_NO_DELAY;
		return true;
	}
	if(layer == IPPROTO_IP && number == IP_TTL) {
		*option = RADIX_SOCKET_OPTION_TIME_TO_LIVE;
		return true;
	}
	return false;
}

// getsockopt(2) on network socket `slot`, of type `type`.
int inet_getsockopt(uint32_t slot, int type, int layer, int number, void *buffer,
		socklen_t *size) {
	uint64_t value;
	enum radix_socket_option option;
	bool time = false;
	if(layer == SOL_SOCKET && (number == SO_TYPE || number == SO_DOMAIN
			|| number == SO_PROTOCOL || number == SO_ACCEPTCONN)) {
		struct radix_file_socket_state_results state;
		if(uint64_t status = radix_file_socket_state(slot, &state); status)
			return inet_errno(status);
		switch(number) {
		case SO_TYPE: value = type; break;
		case SO_DOMAIN: value = AF_INET; break;
		case SO_PROTOCOL: value = type == SOCK_DGRAM ? IPPROTO_UDP : IPPROTO_TCP; break;
		default: value = state.listening; break;
		}
	} else if(inet_option(layer, number, &option, &time)) {
		if(int e = get_option(slot, option, &value); e)
			return e;
		// Errors come back from the calls, and SO_ERROR as an errno.
		if(option == RADIX_SOCKET_OPTION_ERROR && value)
			value = static_cast<uint64_t>(radix::to_errno(value));
	} else {
		return ENOPROTOOPT;
	}
	if(time) {
		struct timeval tv;
		tv.tv_sec = static_cast<time_t>(value / 1000000000);
		tv.tv_usec = static_cast<suseconds_t>(value % 1000000000 / 1000);
		socklen_t len = *size < sizeof(tv) ? *size : sizeof(tv);
		memcpy(buffer, &tv, len);
		*size = len;
		return 0;
	}
	if(*size < sizeof(int))
		return EINVAL;
	int as_int = static_cast<int>(value);
	memcpy(buffer, &as_int, sizeof(int));
	*size = sizeof(int);
	return 0;
}

// setsockopt(2) on network socket `slot`.
int inet_setsockopt(uint32_t slot, int layer, int number, const void *buffer, socklen_t size) {
	enum radix_socket_option option;
	bool time;
	if(!inet_option(layer, number, &option, &time))
		return ENOPROTOOPT;
	uint64_t value;
	if(time) {
		struct timeval tv;
		if(size < sizeof(tv))
			return EINVAL;
		memcpy(&tv, buffer, sizeof(tv));
		if(tv.tv_sec < 0 || tv.tv_usec < 0 || tv.tv_usec >= 1000000)
			return EDOM;
		value = static_cast<uint64_t>(tv.tv_sec) * 1000000000
				+ static_cast<uint64_t>(tv.tv_usec) * 1000;
	} else {
		int as_int;
		if(size < sizeof(int))
			return EINVAL;
		memcpy(&as_int, buffer, sizeof(int));
		switch(option) {
		// A time to live of -1 is the default, as on Linux.
		case RADIX_SOCKET_OPTION_TIME_TO_LIVE:
			if(as_int == -1)
				as_int = 64;
			if(as_int < 1 || as_int > 255)
				return EINVAL;
			break;
		case RADIX_SOCKET_OPTION_ERROR: return ENOPROTOOPT;
		default: break;
		}
		value = static_cast<uint64_t>(as_int < 0 ? 0 : as_int);
	}
	return set_option(slot, option, value);
}

} // namespace

namespace mlibc {

int Sysdeps<Socket>::operator()(int family, int type_and_flags, int protocol, int *fd) {
	int type = type_and_flags & ~(SOCK_NONBLOCK | SOCK_CLOEXEC);
	if(family == AF_INET) {
		enum radix_socket_type kind;
		switch(type) {
		case SOCK_STREAM:
			kind = RADIX_SOCKET_TYPE_STREAM;
			if(protocol != 0 && protocol != IPPROTO_TCP)
				return EPROTONOSUPPORT;
			break;
		case SOCK_DGRAM:
			kind = RADIX_SOCKET_TYPE_DATAGRAM;
			if(protocol != 0 && protocol != IPPROTO_UDP)
				return EPROTONOSUPPORT;
			break;
		// Raw sockets need a right of their own (ICMP, later).
		case SOCK_RAW: return EPERM;
		case SOCK_SEQPACKET: return ESOCKTNOSUPPORT;
		default: return EINVAL;
		}
		uint32_t flags = description_flags(type_and_flags);
		for(uint32_t i = 0; i < RADIX_STARTUP_NETWORK_SLOTS; i++) {
			bool refused = false;
			int e = radix::place(0, fd, [&](uint32_t slot) {
				uint64_t status = radix_network_socket(RADIX_STARTUP_SLOT_NETWORK + i, slot,
						RADIX_ADDRESS_FAMILY_IPV4, kind, flags);
				refused = not_allowed(status);
				return status;
			});
			// Refused by this capability: the next one may allow it.
			if(!refused)
				return e;
		}
		return EACCES;
	}
	if(family == AF_INET6)
		return EAFNOSUPPORT;
	if(family != AF_UNIX)
		return EAFNOSUPPORT;
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
	if(is_inet(slot)) {
		struct radix_address address;
		if(int e = inet_address(addr, length, &address); e)
			return e;
		return through_network([&](uint32_t network) {
			return radix_file_bind_address(slot, network, &address, sizeof(address));
		});
	}
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
	if(is_inet(slot)) {
		uint64_t status = radix_file_listen(slot, most);
		// Unbound, it is bound to a port the kernel picks first, as on
		// Linux.
		if(not_allowed(status)) {
			struct radix_address bound;
			struct radix_file_socket_name_results r;
			if(!radix_file_socket_name(slot, false, &bound, sizeof(bound), &r) && !bound.port) {
				if(int e = bind_to_listen(slot); e)
					return e;
				status = radix_file_listen(slot, most);
			}
		}
		return status ? inet_errno(status) : 0;
	}
	uint64_t status = radix_file_listen(slot, most);
	return status ? socket_errno(status) : 0;
}

int Sysdeps<Connect>::operator()(int fd, const struct sockaddr *addr, socklen_t length) {
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	if(is_inet(slot)) {
		struct radix_address address;
		if(int e = inet_address(addr, length, &address); e)
			return e;
		// A cancellation point, as accept is.
		return through_network([&](uint32_t network) {
			return radix_file_connect_address_with(radix::cancellable, slot, network, &address,
					sizeof(address));
		});
	}
	char name[sizeof(sockaddr_un::sun_path) + 1];
	size_t len;
	if(int e = unix_path(addr, length, name, &len); e)
		return e;
	radix::Path path;
	if(int e = radix::resolve(AT_FDCWD, name, &path); e)
		return e;
	// A cancellation point, as accept is.
	uint64_t status = radix_directory_connect_socket_with(radix::cancellable, path.dir, path.text,
			path.len, slot);
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
		uint64_t status = radix_file_accept_with(radix::cancellable, slot, to, kernel);
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
	// send(2) is sendto with no address: the kernel's send is the same
	// call for both families, so the family is asked for only with an
	// address, as these calls are made often.
	if(sock_addr && addr_length) {
		uint32_t slot;
		if(int e = slot_of(fd, &slot); e)
			return e;
		if(is_inet(slot))
			return send_inet(slot, buffer, size, flags, sock_addr, addr_length, length);
		// A stream socket sends to the socket it is connected to: an
		// address is one too many, as on Linux.
		if(int e = check_socket(fd); e)
			return e;
		return EISCONN;
	}
	size_t sent;
	if(int e = send_bytes(radix::cancellable, fd, buffer, size, flags, nullptr, 0, &sent); e)
		return e;
	*length = static_cast<ssize_t>(sent);
	return 0;
}

int Sysdeps<MsgSend>::operator()(int fd, const struct msghdr *hdr, int flags, ssize_t *length) {
	// The family is asked for only when it changes what is sent, as in
	// sendto.
	bool control = hdr->msg_control && hdr->msg_controllen >= sizeof(struct cmsghdr);
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	int type;
	if(((hdr->msg_name && hdr->msg_namelen) || control || hdr->msg_iovlen > 1)
			&& is_inet(slot, &type)) {
		// Descriptors go only over Unix sockets.
		if(control)
			return EOPNOTSUPP;
		if(hdr->msg_iovlen < 0 || hdr->msg_iovlen > IOV_MAX)
			return EMSGSIZE;
		size_t sum;
		if(int e = radix::iov_total(hdr->msg_iov, hdr->msg_iovlen, &sum); e)
			return e;
		auto to = hdr->msg_namelen ? static_cast<const struct sockaddr *>(hdr->msg_name)
				: nullptr;
		// A datagram goes in one send, and so do a stream's bytes of one
		// buffer.
		if(hdr->msg_iovlen == 1 || (type == SOCK_STREAM && hdr->msg_iovlen == 0))
			return send_inet(slot, hdr->msg_iovlen ? hdr->msg_iov[0].iov_base : nullptr,
					sum, flags, to, hdr->msg_namelen, length);
		char *gathered;
		if(int e = gather(hdr->msg_iov, hdr->msg_iovlen, sum, &gathered); e)
			return e;
		int e = send_inet(slot, gathered, sum, flags, to, hdr->msg_namelen, length);
		free(gathered);
		return e;
	}
	if(hdr->msg_name && hdr->msg_namelen) {
		if(int e = check_socket(fd); e)
			return e;
		return EISCONN;
	}
	uint32_t slots[RADIX_FILE_SEND_FILES_MAX];
	size_t files;
	if(int e = rights_of(hdr, slots, &files); e)
		return e;
	size_t sum;
	// POSIX's int, which Linux reads as an unsigned count: a negative
	// one is too many.
	if(hdr->msg_iovlen < 0 || hdr->msg_iovlen > IOV_MAX)
		return EMSGSIZE;
	if(int e = radix::iov_total(hdr->msg_iov, hdr->msg_iovlen, &sum); e)
		return e;
	// Descriptors go with a byte, and a stream with no bytes sends none,
	// as on Linux.
	if(!sum) {
		if(int e = check_socket(fd); e)
			return e;
		thread_testcancel();
		*length = 0;
		return 0;
	}
	size_t sent;
	// Up to PIPE_BUF bytes go in one send, which the other end gets whole,
	// never mixed with another sender's bytes, as writev does on a pipe.
	if(hdr->msg_iovlen > 1 && sum <= PIPE_BUF) {
		char gathered[PIPE_BUF];
		size_t at = 0;
		for(int i = 0; i < hdr->msg_iovlen; i++) {
			memcpy(gathered + at, hdr->msg_iov[i].iov_base, hdr->msg_iov[i].iov_len);
			at += hdr->msg_iov[i].iov_len;
		}
		if(int e = send_bytes(radix::cancellable, fd, gathered, sum, flags, slots, files, &sent);
				e)
			return e;
		*length = static_cast<ssize_t>(sent);
		return 0;
	}
	size_t total = 0;
	for(int i = 0; i < hdr->msg_iovlen; i++) {
		if(!hdr->msg_iov[i].iov_len)
			continue;
		// The descriptors go with the first bytes, and only the first send
		// can act on a cancel, as writev's first write.
		if(int e = send_bytes(total ? radix_syscall : radix::cancellable, fd,
				hdr->msg_iov[i].iov_base, hdr->msg_iov[i].iov_len, flags,
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
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	// recv(2) is recvfrom with no address; a network datagram socket's
	// receive takes one datagram, so the family is asked for only when the
	// sender is, as in sendto.
	int type;
	if(addr_length && is_inet(slot, &type)) {
		if(type == SOCK_DGRAM)
			return receive_datagram(slot, &iov, 1, flags, sock_addr,
					addr_length ? *addr_length : 0, addr_length, length);
		// A stream's bytes say nothing of where they came from, as on
		// Linux.
		if(int e = receive_vector(fd, &iov, 1, flags, nullptr, length); e)
			return e;
		if(addr_length)
			*addr_length = 0;
		return 0;
	}
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
	size_t sum;
	// POSIX's int, which Linux reads as an unsigned count: a negative
	// one is too many.
	if(hdr->msg_iovlen < 0 || hdr->msg_iovlen > IOV_MAX)
		return EMSGSIZE;
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	// The family is asked for only when it changes what is received, as
	// in recvfrom.
	bool asks = (hdr->msg_name && hdr->msg_namelen) || hdr->msg_iovlen > 1
			|| (hdr->msg_control && hdr->msg_controllen);
	int type;
	if(asks && is_inet(slot, &type)) {
		auto from = static_cast<struct sockaddr *>(hdr->msg_name);
		socklen_t from_len = hdr->msg_namelen;
		int e;
		if(type == SOCK_DGRAM) {
			e = receive_datagram(slot, hdr->msg_iov, hdr->msg_iovlen, flags, from, from_len,
					&from_len, length);
		} else {
			e = receive_vector(fd, hdr->msg_iov, hdr->msg_iovlen,
					flags & ~MSG_CMSG_CLOEXEC, nullptr, length);
			from_len = 0;
		}
		if(e)
			return e;
		hdr->msg_flags = 0;
		hdr->msg_namelen = from ? from_len : 0;
		// Nothing comes but bytes.
		hdr->msg_controllen = 0;
		return 0;
	}
	if(int e = radix::iov_total(hdr->msg_iov, hdr->msg_iovlen, &sum); e)
		return e;
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
	if(int type; is_inet(slot, &type))
		return inet_getsockopt(slot, type, layer, number, buffer, size);
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
	uint32_t slot;
	if(int e = slot_of(fd, &slot); e)
		return e;
	if(is_inet(slot))
		return inet_setsockopt(slot, layer, number, buffer, size);
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
