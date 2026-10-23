// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The helpers the Radix system layer's files share (radix.hpp).

#include <abi-bits/auxv.h>
#include <abi-bits/fcntl.h>
#include <abi-bits/stat.h>
#include <bits/ensure.h>
#include <elf.h>
#include <errno.h>
#include <string.h>

#include "radix.hpp"

namespace radix {

namespace {

// The working directory, absolute, with no `/` at its end but for the
// root. Threads share it: chdir is rare, and a path resolved while it
// changes uses the old one or the new one.
char current_dir[PATH_MAX_BYTES] = "/";

// The cached process number: 0 until asked.
uint64_t cached_pid;

// Each descriptor's next listing cursor (readdir): only directories use
// theirs. Untouched pages of it take no memory.
uint64_t cursors[MAX_FDS];

// <sys/auxv.h>'s numbers for Radix's own entries are the kernel's.
static_assert(AT_RADIX_CWD == RADIX_PROCESS_AUX_CWD);
static_assert(AT_RADIX_STARTUP == RADIX_PROCESS_AUX_STARTUP);

// The first thread's stack, which exec makes 8 MiB (radix-posix(7)), and
// its top: 0 until start-up finds it.
constexpr size_t FIRST_STACK_BYTES = 8 << 20;
uintptr_t first_stack_top;

} // namespace

int to_errno(uint64_t status) {
	if(!status)
		return 0;
	switch(RADIX_STATUS_DOMAIN(status)) {
	case RADIX_KERNEL_ERROR_DOMAIN:
		return radix_kernel_errno(status);
	case RADIX_FILE_ERROR_DOMAIN:
		return radix_file_errno(status);
	default:
		return EIO;
	}
}

namespace {

// The soft RLIMIT_NOFILE, once read (-1 before): only this program's
// setrlimit changes it, and exec reads it again.
int files_soft = -1;

} // namespace

int files_limit() {
	int limit = __atomic_load_n(&files_soft, __ATOMIC_RELAXED);
	if(limit >= 0)
		return limit;
	struct radix_process_limits_results r;
	if(radix_process_limits(SLOT_PROCESS, RADIX_LIMIT_KIND_FILES, false, 0, 0, &r))
		return MAX_FDS;
	set_files_limit(r.current);
	return __atomic_load_n(&files_soft, __ATOMIC_RELAXED);
}

void set_files_limit(uint64_t limit) {
	int fds = limit < static_cast<uint64_t>(MAX_FDS) ? static_cast<int>(limit) : MAX_FDS;
	__atomic_store_n(&files_soft, fds, __ATOMIC_RELAXED);
}

int free_slot(int min_fd, uint32_t *slot) {
	if(min_fd < 0 || min_fd >= MAX_FDS)
		return EINVAL;
	uint32_t first = FD_BASE + static_cast<uint32_t>(min_fd);
	uint32_t end = FD_BASE + static_cast<uint32_t>(files_limit());
	for(;;) {
		if(first >= end)
			return EMFILE;
		struct radix_process_slots_used_results used;
		if(uint64_t status = radix_process_slots_used(SLOT_PROCESS, first, &used); status)
			return to_errno(status);
		if(first < used.slots) {
			// Only the slots the space has count as free.
			uint32_t room = used.slots - first;
			uint64_t free = ~used.used;
			if(room < 64)
				free &= (UINT64_C(1) << room) - 1;
			if(free) {
				*slot = first + static_cast<uint32_t>(__builtin_ctzll(free));
				return *slot < end ? 0 : EMFILE;
			}
			first += 64;
			if(first < used.slots)
				continue;
			if(first >= end)
				return EMFILE;
		}
		// Every slot is taken: twice as many, up to the most there may be.
		if(used.slots >= MAX_SLOTS)
			return EMFILE;
		uint32_t want = used.slots * 2 < MAX_SLOTS ? used.slots * 2 : MAX_SLOTS;
		struct radix_process_grow_results grown;
		if(uint64_t status = radix_process_grow(SLOT_PROCESS, want, &grown); status) {
			int e = to_errno(status);
			return e == ENOMEM ? EMFILE : e;
		}
	}
}

namespace {

// Appends the components of `path` to the resolved path in `out` (which
// holds `len` bytes, no `/` at either end), working out `.` and `..` by
// their names: only for a working directory, which has no links in it.
int append(char *out, size_t *len, const char *path) {
	const char *p = path;
	while(*p) {
		while(*p == '/')
			p++;
		const char *end = p;
		while(*end && *end != '/')
			end++;
		size_t n = end - p;
		if(n == 0 || (n == 1 && p[0] == '.')) {
			// Nothing, or this directory.
		} else if(n == 2 && p[0] == '.' && p[1] == '.') {
			// The parent: the root is its own.
			while(*len > 0 && out[*len - 1] != '/')
				(*len)--;
			if(*len > 0)
				(*len)--;
		} else {
			size_t need = *len + (*len ? 1 : 0) + n;
			if(need >= PATH_MAX_BYTES)
				return ENAMETOOLONG;
			if(*len)
				out[(*len)++] = '/';
			memcpy(out + *len, p, n);
			*len += n;
		}
		p = end;
	}
	return 0;
}

} // namespace

int resolve(int dirfd, const char *path, Path *out) {
	if(!path)
		return EFAULT;
	if(!*path)
		return ENOENT;
	size_t total = strlen(path);
	if(total >= PATH_MAX_BYTES)
		return ENAMETOOLONG;
	if(path[0] != '/' && dirfd != AT_FDCWD) {
		// Beneath the directory `dirfd` holds, which the kernel resolves:
		// `..` cannot climb above that directory (docs/design/posix.md).
		out->dir = slot_of(dirfd);
		if(out->dir == NO_SLOT)
			return EBADF;
		memcpy(out->text, path, total + 1);
		out->len = total;
		return 0;
	}
	// Beneath the root: an absolute path without its leading slashes, or
	// the working directory (which has no links or `..` in it) and the
	// path. The kernel works out `.`, `..` and symbolic links, as a link
	// changes where a `..` after it leads.
	out->dir = SLOT_ROOT;
	size_t len = 0;
	if(path[0] == '/') {
		while(*path == '/')
			path++;
	} else if(current_dir[1]) {
		size_t dir = strlen(current_dir + 1);
		memcpy(out->text, current_dir + 1, dir);
		out->text[dir] = '/';
		len = dir + 1;
	}
	size_t rest = strlen(path);
	if(len + rest >= PATH_MAX_BYTES)
		return ENAMETOOLONG;
	memcpy(out->text + len, path, rest);
	len += rest;
	if(!len)
		out->text[len++] = '.';
	out->text[len] = 0;
	out->len = len;
	return 0;
}

const char *cwd() {
	return current_dir;
}

int set_cwd(const char *path) {
	char resolved[PATH_MAX_BYTES];
	size_t len = 0;
	if(int e = append(resolved, &len, path); e)
		return e;
	if(len + 2 > PATH_MAX_BYTES)
		return ENAMETOOLONG;
	current_dir[0] = '/';
	memcpy(current_dir + 1, resolved, len);
	current_dir[len + 1] = 0;
	return 0;
}

void init_from_auxv(const uintptr_t *auxv) {
	const char *last = nullptr;
	for(; auxv[0] != AT_NULL; auxv += 2) {
		if(auxv[0] == AT_RADIX_CWD) {
			auto dir = reinterpret_cast<const char *>(auxv[1]);
			if(dir && dir[0] == '/')
				set_cwd(dir);
			last = dir;
		} else if(auxv[0] == AT_EXECFN && !last) {
			last = reinterpret_cast<const char *>(auxv[1]);
		}
	}
	// The working directory's string is the stack's last, or with none
	// the path the program was run as, and 8 zero bytes follow it to the
	// top, as on Linux.
	if(last)
		first_stack_top = reinterpret_cast<uintptr_t>(last) + strlen(last) + 1 + 8;
}

int first_stack(void **base, size_t *size) {
	if(!first_stack_top)
		return ENOSYS;
	*base = reinterpret_cast<void *>(first_stack_top - FIRST_STACK_BYTES);
	*size = FIRST_STACK_BYTES;
	return 0;
}

uint64_t pid() {
	uint64_t known = __atomic_load_n(&cached_pid, __ATOMIC_RELAXED);
	if(known)
		return known;
	struct radix_process_ids_results ids;
	if(radix_process_ids(SLOT_PROCESS, 0, &ids))
		return 0;
	__atomic_store_n(&cached_pid, ids.pid, __ATOMIC_RELAXED);
	return ids.pid;
}

void forget_pid() {
	__atomic_store_n(&cached_pid, 0, __ATOMIC_RELAXED);
}

void fill_stat(struct ::stat *st, uint8_t kind, uint64_t size, uint64_t allocated, uint64_t id,
		uint64_t created, uint64_t modified, uint64_t accessed, uint64_t mount) {
	memset(st, 0, sizeof(*st));
	// A file's number is unique on its filesystem, and the mount's number
	// tells the filesystems apart.
	st->st_dev = mount;
	st->st_ino = id;
	switch(kind) {
	case RADIX_FILE_KIND_DIRECTORY:
		st->st_mode = S_IFDIR | 0755;
		st->st_nlink = 2;
		break;
	case RADIX_FILE_KIND_BLOCK_DEVICE:
		st->st_mode = S_IFBLK | 0660;
		st->st_nlink = 1;
		break;
	case RADIX_FILE_KIND_CHAR_DEVICE:
		st->st_mode = S_IFCHR | 0666;
		st->st_nlink = 1;
		break;
	case RADIX_FILE_KIND_PIPE:
		st->st_mode = S_IFIFO | 0600;
		st->st_nlink = 1;
		break;
	case RADIX_FILE_KIND_SYMLINK:
		st->st_mode = S_IFLNK | 0777;
		st->st_nlink = 1;
		break;
	default:
		// Files have no permissions until users come (Phase 13): all may
		// read, write and run them.
		st->st_mode = S_IFREG | 0755;
		st->st_nlink = 1;
		break;
	}
	st->st_size = size;
	st->st_blksize = 4096;
	st->st_blocks = (allocated + 511) / 512;
	auto at = [](uint64_t ns) {
		struct timespec ts;
		ts.tv_sec = ns / 1000000000;
		ts.tv_nsec = ns % 1000000000;
		return ts;
	};
	// The kernel keeps no time of the last change to a file's status, so
	// st_ctim is the last change to its contents (docs/design/posix.md).
	st->st_mtim = at(modified);
	st->st_ctim = at(modified);
	st->st_atim = at(accessed);
	(void)created;
}

uint64_t dir_cursor(int fd) {
	if(fd < 0 || fd >= MAX_FDS)
		return 0;
	return __atomic_load_n(&cursors[fd], __ATOMIC_RELAXED);
}

void set_dir_cursor(int fd, uint64_t cursor) {
	if(fd >= 0 && fd < MAX_FDS)
		__atomic_store_n(&cursors[fd], cursor, __ATOMIC_RELAXED);
}

void log(const char *text, size_t len) {
	radix_debug_log(SLOT_LOG, text, len);
}

} // namespace radix
