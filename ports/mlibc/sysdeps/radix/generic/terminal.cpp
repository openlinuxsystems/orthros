// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Terminals on Radix (ADR 0066): termios, window sizes, job control and
// pseudo-terminals, all through a terminal's `control` method, which
// takes Linux's ioctl requests (radix-terminal(2)).

#include <asm/ioctls.h>
#include <bits/ensure.h>
#include <errno.h>
#include <fcntl.h>
#include <mlibc/all-sysdeps.hpp>
#include <string.h>
#include <sys/ioctl.h>
#include <termios.h>

#include "radix.hpp"

namespace {

static_assert(sizeof(struct termios) == sizeof(struct radix_termios),
		"struct termios is the kernel's");
static_assert(sizeof(struct winsize) == sizeof(struct radix_window_size),
		"struct winsize is the kernel's");

// Sends `request` to the terminal in `fd`, with `argument` and the
// `size` bytes at `buffer`. Returns 0 or an errno.
int control(int fd, uint32_t request, uint64_t argument, const void *buffer, size_t size,
		uint64_t *result = nullptr) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_file_control_results r;
	uint64_t status = radix_file_control(slot, request, argument,
			reinterpret_cast<uint64_t>(buffer), size, &r);
	if(status) {
		// Directories and other capabilities that are not files are not
		// terminals either.
		if(RADIX_STATUS_DOMAIN(status) == RADIX_KERNEL_ERROR_DOMAIN
				&& (RADIX_STATUS_CODE(status) == RADIX_KERNEL_WRONG_KIND
						|| RADIX_STATUS_CODE(status) == RADIX_KERNEL_NO_SUCH_METHOD))
			return ENOTTY;
		return radix::to_errno(status);
	}
	if(result)
		*result = r.result;
	return 0;
}

// The size of what Linux's terminal request `request` reads or writes.
size_t size_of(unsigned long request) {
	switch(request) {
	case TCGETS:
	case TCSETS:
	case TCSETSW:
	case TCSETSF:
		return sizeof(struct termios);
	case TIOCGWINSZ:
	case TIOCSWINSZ:
		return sizeof(struct winsize);
	case TIOCGPGRP:
	case TIOCSPGRP:
	case TIOCGSID:
	case FIONREAD:
		return sizeof(int);
	default:
		// The size requests made with _IOC carry.
		return _IOC_SIZE(request);
	}
}

// The pseudo-terminal number of the terminal in `fd`.
int pty_number(int fd, uint32_t *number) {
	return control(fd, RADIX_TERMINAL_GET_PTY_NUMBER, 0, number, sizeof(*number));
}

// Writes "/dev/pts/N" for `number` to `buf`, which holds `size` bytes.
int pts_path(uint32_t number, char *buf, size_t size) {
	char digits[10];
	int n = 0;
	do {
		digits[n++] = static_cast<char>('0' + number % 10);
		number /= 10;
	} while(number);
	const char prefix[] = "/dev/pts/";
	size_t len = sizeof(prefix) - 1;
	if(size < len + n + 1)
		return ERANGE;
	memcpy(buf, prefix, len);
	while(n)
		buf[len++] = digits[--n];
	buf[len] = 0;
	return 0;
}

} // namespace

namespace mlibc {

int Sysdeps<Isatty>::operator()(int fd) {
	struct termios attr;
	return control(fd, RADIX_TERMINAL_GET_ATTRIBUTES, 0, &attr, sizeof(attr));
}

int Sysdeps<Tcgetattr>::operator()(int fd, struct termios *attr) {
	return control(fd, RADIX_TERMINAL_GET_ATTRIBUTES, 0, attr, sizeof(*attr));
}

int Sysdeps<Tcsetattr>::operator()(int fd, int optional_action, const struct termios *attr) {
	uint32_t request;
	switch(optional_action) {
	case TCSANOW:
		request = RADIX_TERMINAL_SET_ATTRIBUTES;
		break;
	case TCSADRAIN:
		request = RADIX_TERMINAL_SET_ATTRIBUTES_DRAIN;
		break;
	case TCSAFLUSH:
		request = RADIX_TERMINAL_SET_ATTRIBUTES_FLUSH;
		break;
	default:
		return EINVAL;
	}
	return control(fd, request, 0, attr, sizeof(*attr));
}

int Sysdeps<Tcflow>::operator()(int fd, int action) {
	// TCOOFF, TCOON, TCIOFF and TCION are the kernel's.
	return control(fd, RADIX_TERMINAL_FLOW, static_cast<uint64_t>(action), nullptr, 0);
}

int Sysdeps<Tcflush>::operator()(int fd, int queue) {
	// TCIFLUSH, TCOFLUSH and TCIOFLUSH are the kernel's.
	return control(fd, RADIX_TERMINAL_FLUSH, static_cast<uint64_t>(queue), nullptr, 0);
}

int Sysdeps<Tcdrain>::operator()(int fd) {
	// A terminal's output is in its master's ring as soon as it is
	// written: there is nothing to wait for.
	return sysdep<Isatty>(fd);
}

int Sysdeps<Tcsendbreak>::operator()(int fd, int dur) {
	(void)dur;
	// Pseudo-terminals have no line to send a break on.
	return sysdep<Isatty>(fd);
}

int Sysdeps<Tcgetwinsize>::operator()(int fd, struct winsize *winsz) {
	return control(fd, RADIX_TERMINAL_GET_WINDOW_SIZE, 0, winsz, sizeof(*winsz));
}

int Sysdeps<Tcsetwinsize>::operator()(int fd, const struct winsize *winsz) {
	return control(fd, RADIX_TERMINAL_SET_WINDOW_SIZE, 0, winsz, sizeof(*winsz));
}

int Sysdeps<Ioctl>::operator()(int fd, unsigned long request, void *arg, int *result) {
	switch(request) {
	case FIONBIO: {
		// Non-blocking or not, as fcntl's F_SETFL.
		uint32_t slot = radix::slot_of(fd);
		if(slot == radix::NO_SLOT)
			return EBADF;
		struct radix_file_status_flags_results r;
		if(uint64_t status = radix_file_status_flags(slot, false, 0, &r); status)
			return radix::to_errno(status);
		uint32_t flags = r.flags & ~RADIX_DESCRIPTION_FLAGS_NON_BLOCKING;
		if(*static_cast<int *>(arg))
			flags |= RADIX_DESCRIPTION_FLAGS_NON_BLOCKING;
		if(uint64_t status = radix_file_status_flags(slot, true, flags, &r); status)
			return radix::to_errno(status);
		*result = 0;
		return 0;
	}
	case FIOCLEX:
	case FIONCLEX: {
		uint32_t slot = radix::slot_of(fd);
		if(slot == radix::NO_SLOT)
			return EBADF;
		uint32_t flags = request == FIOCLEX ? RADIX_SLOT_FLAGS_CLOSE_ON_EXEC : 0;
		struct radix_capability_flags_results r;
		if(uint64_t status = radix_capability_flags(slot, true, flags, &r); status)
			return radix::to_errno(status);
		*result = 0;
		return 0;
	}
	case TCSBRK:
		return sysdep<Isatty>(fd);
	default:
		break;
	}
	if(request > UINT32_MAX)
		return EINVAL;
	// The kernel takes Linux's requests as they are: the argument as a
	// number and, for requests that pass a structure, as its address.
	uint64_t value;
	if(int e = control(fd, static_cast<uint32_t>(request), reinterpret_cast<uint64_t>(arg), arg,
			   size_of(request), &value);
			e)
		return e;
	*result = static_cast<int>(value);
	return 0;
}

int Sysdeps<Ttyname>::operator()(int fd, char *buf, size_t size) {
	// Every terminal is a pseudo-terminal, /dev/pts/N.
	uint32_t number;
	if(int e = pty_number(fd, &number); e)
		return e;
	return pts_path(number, buf, size);
}

int Sysdeps<Ptsname>::operator()(int fd, char *buffer, size_t length) {
	uint32_t number;
	if(int e = pty_number(fd, &number); e)
		return e;
	return pts_path(number, buffer, length);
}

int Sysdeps<Unlockpt>::operator()(int fd) {
	uint32_t unlock = 0;
	return control(fd, RADIX_TERMINAL_SET_PTY_LOCK, 0, &unlock, sizeof(unlock));
}

} // namespace mlibc
