// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the Radix system layer's files share (radix-posix(7), ADR 0068):
// the slots a program starts with, the mapping from a kernel status to an
// errno, file descriptors as capability slots, and paths, which the C
// library resolves against its working directory to a path beneath the
// root directory, the only form the kernel takes.

#pragma once

#include <stddef.h>
#include <stdint.h>

#include <radix/radix.h>

struct iovec;
struct stat;

namespace radix {

// The slots every program starts with (radix-startup(2)).
constexpr uint32_t SLOT_THREAD = RADIX_STARTUP_SLOT_THREAD;
constexpr uint32_t SLOT_SPACE = RADIX_STARTUP_SLOT_SPACE;
constexpr uint32_t SLOT_RANDOM = RADIX_STARTUP_SLOT_RANDOM;
constexpr uint32_t SLOT_LOG = RADIX_STARTUP_SLOT_LOG;
constexpr uint32_t SLOT_PROCESS = RADIX_STARTUP_SLOT_PROCESS;
constexpr uint32_t SLOT_ROOT = RADIX_STARTUP_SLOT_ROOT;

// File descriptor `n` is slot FD_BASE + n.
constexpr uint32_t FD_BASE = RADIX_STARTUP_SLOT_FD_BASE;

// The most slots a capability space may have (radix-process(2)).
constexpr uint32_t MAX_SLOTS = RADIX_PROCESS_MAX_SLOTS;

// The most file descriptors a program can have.
constexpr int MAX_FDS = MAX_SLOTS - FD_BASE;

// No slot, for `start`.
constexpr uint32_t NO_SLOT = RADIX_ENDPOINT_NO_SLOT;

// The longest path the kernel takes, with its NUL.
constexpr size_t PATH_MAX_BYTES = RADIX_DIRECTORY_PATH_MAX;

// The errno a kernel status maps to: 0 for success.
int to_errno(uint64_t status);

// The slot of file descriptor `fd`, or NO_SLOT if it cannot be one.
inline uint32_t slot_of(int fd) {
	if(fd < 0 || fd >= MAX_FDS)
		return NO_SLOT;
	return FD_BASE + static_cast<uint32_t>(fd);
}

// One more than the highest file descriptor the program may have: the
// soft RLIMIT_NOFILE, at most MAX_FDS (radix-posix(7)).
int files_limit();

// Notes that setrlimit made `limit` the soft RLIMIT_NOFILE.
void set_files_limit(uint64_t limit);

// Finds the lowest free file descriptor from `min_fd`, growing the
// capability space when every slot is taken. Returns 0 and the slot, or
// EMFILE if there is none below files_limit(). The slot may be taken by
// another thread before it is used: callers that get `occupied` look
// again (`place`).
int free_slot(int min_fd, uint32_t *slot);

// Forgets the path of directory descriptor `fd` (below).
void forget_dir_path(int fd);

// Runs `put(slot)`, which puts a capability in an empty slot, on the
// lowest free descriptor from `min_fd` until one is empty when it runs.
// Returns 0 and the descriptor, or an errno.
template<typename F>
int place(int min_fd, int *fd, F put) {
	for(;;) {
		uint32_t slot;
		if(int e = free_slot(min_fd, &slot); e)
			return e;
		uint64_t status = put(slot);
		if(!status) {
			*fd = static_cast<int>(slot - FD_BASE);
			// A new descriptor has no directory path until it is given one.
			forget_dir_path(*fd);
			return 0;
		}
		// Another thread took it first.
		if(RADIX_STATUS_DOMAIN(status) == RADIX_KERNEL_ERROR_DOMAIN
				&& RADIX_STATUS_CODE(status) == RADIX_KERNEL_OCCUPIED)
			continue;
		return to_errno(status);
	}
}

// A path as the kernel takes it: the directory slot it starts from and the
// path beneath it, never starting with `/` (`.` for the directory itself).
struct Path {
	uint32_t dir;
	size_t len;
	char text[PATH_MAX_BYTES];
};

// Resolves `path`, relative to the directory `dirfd` (AT_FDCWD for the
// working directory): an absolute path, or one relative to the working
// directory, becomes a path beneath the root, whose `.`, `..` and
// symbolic links the kernel works out. A path with a `..` in it,
// relative to a directory descriptor whose path is known, becomes one
// beneath the root too. Returns 0 or an errno.
int resolve(int dirfd, const char *path, Path *out);

// The paths of directory descriptors (ADR 0089). The kernel resolves a
// path beneath a directory capability and never above it, so the C
// library notes the path, beneath the root, by which each directory
// descriptor was opened, and goes by it from the root for a `..` beneath
// the descriptor and for fchdir. A descriptor from before exec, or one
// opened while the notes are full, has no path.

// Notes that descriptor `fd`, just opened as a directory by `path`, has
// the path `path` leads to from the root; one beneath a descriptor with
// no path has none.
void note_dir_path(int fd, const Path &path);

// Gives descriptor `to` the path of `from`, or none if it has none, as a
// copy of `from` it now is.
void copy_dir_path(int from, int to);

// Forgets the path of `fd`, which was closed or is to be another file.
void forget_dir_path(int fd);

// Copies the path of descriptor `fd`, beneath the root (`.` for the root
// itself), to `out`, which has room for PATH_MAX_BYTES bytes, and returns
// its length: 0 if it has none.
size_t dir_path(int fd, char *out);

// The working directory, absolute, as the program knows it.
const char *cwd();

// Makes `path` (absolute, with no links in it: what the kernel's
// real_path returned) the working directory.
int set_cwd(const char *path);

// Called at start-up with the auxiliary vector's entries.
void init_from_auxv(const uintptr_t *auxv);

// The first thread's stack: its lowest address and size. Returns 0, or
// ENOSYS if start-up did not find it.
int first_stack(void **base, size_t *size);

// The number of the calling process, cached until fork.
uint64_t pid();

// Forgets the cached process number, in a child after fork.
void forget_pid();

// A Linux-style `struct stat` from what the kernel says about a file.
void fill_stat(struct ::stat *st, uint8_t kind, uint64_t size, uint64_t allocated, uint64_t id,
		uint64_t created, uint64_t modified, uint64_t accessed, uint64_t mount);

// The cursor of the directory listing descriptor `fd` reads next.
uint64_t dir_cursor(int fd);
void set_dir_cursor(int fd, uint64_t cursor);

// The kernel's ClockKind for Linux's clock number `clock`, or EINVAL for
// one Linux does not have.
int clock_kind(int clock, enum radix_clock_kind *kind);

// The bytes of the `count` buffers at `iov` in all, in `sum`; EINVAL if
// they pass SSIZE_MAX, as readv, writev, sendmsg and recvmsg fail then
// on Linux.
int iov_total(const struct iovec *iov, size_t count, size_t *sum);

// Writes `len` bytes of `text` to the kernel log.
void log(const char *text, size_t len);

} // namespace radix
