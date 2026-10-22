// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// poll, ppoll and pselect on Radix (ADR 0065): the process's `poll`
// method takes POSIX's `struct pollfd` array as it is; select's sets
// become such an array.

#include <bits/ensure.h>
#include <errno.h>
#include <mlibc/all-sysdeps.hpp>
#include <poll.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>

#include "radix.hpp"

namespace {

static_assert(sizeof(struct pollfd) == sizeof(struct radix_poll_entry), "pollfd is the kernel's");

// The events the kernel knows, which are POSIX's: POLLIN, POLLPRI,
// POLLOUT, POLLERR, POLLHUP and POLLNVAL.
constexpr short KERNEL_EVENTS = POLLIN | POLLPRI | POLLOUT | POLLERR | POLLHUP | POLLNVAL;

// The events the kernel knows that stand for `events`: the normal and
// priority band events are the plain ones.
short kernel_events(short events) {
	short out = events & KERNEL_EVENTS;
	if(events & POLLRDNORM)
		out |= POLLIN;
	if(events & POLLRDBAND)
		out |= POLLPRI;
	if(events & (POLLWRNORM | POLLWRBAND))
		out |= POLLOUT;
	return out;
}

// What a poll of `events` found, given what the kernel found.
short found_events(short events, short found) {
	short out = found & (events | POLLERR | POLLHUP | POLLNVAL);
	if((events & POLLRDNORM) && (found & POLLIN))
		out |= POLLRDNORM;
	if((events & POLLRDBAND) && (found & POLLPRI))
		out |= POLLRDBAND;
	if((events & POLLWRNORM) && (found & POLLOUT))
		out |= POLLWRNORM;
	if((events & POLLWRBAND) && (found & POLLOUT))
		out |= POLLWRBAND;
	return out;
}

// Polls `fds` for at most `timeout_ns` (forever if negative), with the
// signal mask `mask` while it waits if there is one.
int poll_ns(struct pollfd *fds, nfds_t count, int64_t timeout_ns, const sigset_t *mask,
		int *num_events) {
	if(count > RADIX_PROCESS_MAX_SLOTS)
		return EINVAL;
	uint32_t flags = 0;
	if(timeout_ns < 0)
		flags |= RADIX_POLL_FLAGS_FOREVER;
	uint64_t kernel_mask = 0;
	if(mask) {
		flags |= RADIX_POLL_FLAGS_MASK;
		memcpy(&kernel_mask, mask, sizeof(kernel_mask));
	}
	// The kernel takes the array as it is unless an entry asks for an
	// event it does not know.
	bool plain = true;
	for(nfds_t i = 0; i < count && plain; i++)
		plain = !(fds[i].events & ~KERNEL_EVENTS);
	struct pollfd *entries = fds;
	if(!plain) {
		entries = static_cast<struct pollfd *>(malloc(count * sizeof(struct pollfd)));
		if(!entries)
			return ENOMEM;
		for(nfds_t i = 0; i < count; i++) {
			entries[i].fd = fds[i].fd;
			entries[i].events = kernel_events(fds[i].events);
			entries[i].revents = 0;
		}
	}
	struct radix_process_poll_results r;
	uint64_t status = radix_process_poll(radix::SLOT_PROCESS, reinterpret_cast<uint64_t>(entries),
			static_cast<uint32_t>(count), radix::FD_BASE,
			timeout_ns < 0 ? 0 : static_cast<uint64_t>(timeout_ns), kernel_mask, flags, &r);
	if(!plain) {
		if(!status) {
			for(nfds_t i = 0; i < count; i++)
				fds[i].revents = found_events(fds[i].events, entries[i].revents);
		}
		free(entries);
	}
	if(status)
		return radix::to_errno(status);
	*num_events = static_cast<int>(r.ready);
	return 0;
}

int64_t ns_of(const struct timespec *ts) {
	if(!ts)
		return -1;
	if(ts->tv_sec < 0 || ts->tv_nsec < 0 || ts->tv_nsec >= 1000000000)
		return -2;
	// About 292 years: longer is forever.
	if(ts->tv_sec >= INT64_MAX / 1000000000 - 1)
		return -1;
	return static_cast<int64_t>(ts->tv_sec) * 1000000000 + ts->tv_nsec;
}

} // namespace

namespace mlibc {

int Sysdeps<Poll>::operator()(struct pollfd *fds, nfds_t count, int timeout, int *num_events) {
	int64_t ns = timeout < 0 ? -1 : static_cast<int64_t>(timeout) * 1000000;
	return poll_ns(fds, count, ns, nullptr, num_events);
}

int Sysdeps<Ppoll>::operator()(struct pollfd *fds, nfds_t count, const struct timespec *ts,
		const sigset_t *mask, int *num_events) {
	int64_t ns = ns_of(ts);
	if(ns == -2)
		return EINVAL;
	return poll_ns(fds, count, ns, mask, num_events);
}

int Sysdeps<Pselect>::operator()(int num_fds, fd_set *read_set, fd_set *write_set,
		fd_set *except_set, const struct timespec *timeout, const sigset_t *sigmask,
		int *num_events) {
	if(num_fds < 0 || num_fds > FD_SETSIZE)
		return EINVAL;
	int64_t ns = ns_of(timeout);
	if(ns == -2)
		return EINVAL;
	auto in = [](fd_set *set, int fd) { return set && FD_ISSET(fd, set); };
	// One entry for each descriptor in a set.
	struct pollfd entries[FD_SETSIZE];
	nfds_t count = 0;
	for(int fd = 0; fd < num_fds; fd++) {
		short events = 0;
		if(in(read_set, fd))
			events |= POLLIN;
		if(in(write_set, fd))
			events |= POLLOUT;
		if(in(except_set, fd))
			events |= POLLPRI;
		if(events) {
			entries[count].fd = fd;
			entries[count].events = events;
			entries[count].revents = 0;
			count++;
		}
	}
	int ready;
	if(int e = poll_ns(entries, count, ns, sigmask, &ready); e)
		return e;
	for(nfds_t i = 0; i < count; i++) {
		if(entries[i].revents & POLLNVAL)
			return EBADF;
	}
	if(read_set)
		FD_ZERO(read_set);
	if(write_set)
		FD_ZERO(write_set);
	if(except_set)
		FD_ZERO(except_set);
	int total = 0;
	for(nfds_t i = 0; i < count; i++) {
		int fd = entries[i].fd;
		short found = entries[i].revents;
		// As Linux: an end or an error is something to read, and an error
		// something to write.
		if((entries[i].events & POLLIN) && (found & (POLLIN | POLLHUP | POLLERR))) {
			FD_SET(fd, read_set);
			total++;
		}
		if((entries[i].events & POLLOUT) && (found & (POLLOUT | POLLERR))) {
			FD_SET(fd, write_set);
			total++;
		}
		if((entries[i].events & POLLPRI) && (found & POLLPRI)) {
			FD_SET(fd, except_set);
			total++;
		}
	}
	*num_events = total;
	return 0;
}

} // namespace mlibc
