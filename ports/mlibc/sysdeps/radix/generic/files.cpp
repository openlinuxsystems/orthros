// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Files, directories and descriptors on Radix (ADR 0064, ADR 0068).
//
// File descriptor `n` is the capability in slot FD_BASE + n: an open file
// description from radix-directory(2)'s `open` or radix-process(2)'s
// `pipe`, which the kernel gives an offset and status flags. dup derives
// a copy of the capability, which shares the description; close deletes
// it. Paths are resolved here against the working directory into a path
// beneath the root directory (SLOT_ROOT), as the kernel takes them.

#include <abi-bits/fcntl.h>
#include <bits/ensure.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <mlibc/all-sysdeps.hpp>
#include <mlibc/threads.hpp>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

#include "radix.hpp"

namespace {

// Whether `status` is the kernel error `code`.
bool is(uint64_t status, uint16_t code) {
	return RADIX_STATUS_DOMAIN(status) == RADIX_KERNEL_ERROR_DOMAIN
			&& RADIX_STATUS_CODE(status) == code;
}

// Whether a call failed because its capability is of another kind than
// the call is for: a file's method on a directory, or the other way
// round, fails with no_such_method (radix-ipc(7)).
bool other_kind(uint64_t status) {
	return is(status, RADIX_KERNEL_NO_SUCH_METHOD) || is(status, RADIX_KERNEL_WRONG_KIND);
}

// The errno of a read (or with `writing` a write) that failed with
// `status`: a directory is not read as a file, and is never open for
// writing, and a descriptor not open for reading or writing is a bad one
// for it, as on Linux.
int io_errno(uint64_t status, bool writing) {
	if(other_kind(status))
		return writing ? EBADF : EISDIR;
	if(is(status, RADIX_KERNEL_MISSING_RIGHTS))
		return EBADF;
	return radix::to_errno(status);
}

// The kernel's OpenFlags for `flags`.
uint32_t open_flags(int flags) {
	uint32_t out = 0;
	if(flags & O_CREAT)
		out |= RADIX_OPEN_FLAGS_CREATE;
	if(flags & O_EXCL)
		out |= RADIX_OPEN_FLAGS_EXCLUSIVE;
	if(flags & O_TRUNC)
		out |= RADIX_OPEN_FLAGS_TRUNCATE;
	if(flags & O_DIRECTORY)
		out |= RADIX_OPEN_FLAGS_DIRECTORY;
	if(flags & O_APPEND)
		out |= RADIX_OPEN_FLAGS_APPEND;
	if(flags & O_NONBLOCK)
		out |= RADIX_OPEN_FLAGS_NON_BLOCKING;
	if(flags & O_NOCTTY)
		out |= RADIX_OPEN_FLAGS_NO_TERMINAL;
	if(flags & O_CLOEXEC)
		out |= RADIX_OPEN_FLAGS_CLOSE_ON_EXEC;
	if(flags & O_NOFOLLOW)
		out |= RADIX_OPEN_FLAGS_NO_FOLLOW;
	return out;
}

// Opens `path` with `flags` (the kernel's) and `rights` into the lowest
// free descriptor; returns 0 and the descriptor and what it is.
int open_rights(const radix::Path &path, uint32_t flags, uint64_t rights, int *fd,
		uint8_t *kind) {
	return radix::place(0, fd, [&](uint32_t slot) {
		struct radix_directory_open_results r;
		uint64_t status = radix_directory_open(path.dir, path.text, path.len, flags, rights,
				slot, &r);
		if(!status)
			*kind = r.kind;
		return status;
	});
}

// The access mode of open's `flags`. mlibc's O_ACCMODE, as Linux's,
// takes in O_PATH, which opens a descriptor only to name a file by (to
// stat it, or to open files beneath it); that is one open for reading
// here, as Radix has nothing narrower.
int access_mode(int flags) {
	if(flags & O_PATH)
		return O_RDONLY;
	return flags & (O_RDONLY | O_WRONLY | O_RDWR);
}

// Opens `path` as POSIX's openat does.
int open_path(const radix::Path &path, int flags, int *fd) {
	// Linux 6.4 and later refuse to make a file that must be a directory,
	// and make nothing.
	if((flags & O_CREAT) && (flags & O_DIRECTORY))
		return EINVAL;
	uint32_t kernel_flags = open_flags(flags);
	uint64_t rights;
	switch(access_mode(flags)) {
	case O_WRONLY:
		rights = RADIX_RIGHTS_WRITE;
		break;
	case O_RDWR:
		rights = RADIX_RIGHTS_READ | RADIX_RIGHTS_WRITE;
		break;
	default:
		rights = RADIX_RIGHTS_READ;
		break;
	}
	uint8_t kind = 0;
	bool read_only = access_mode(flags) == O_RDONLY;
	// A directory opened for reading gets the write right too, where its
	// filesystem allows it: POSIX lets openat make files beneath any
	// directory descriptor, which takes that right here.
	if(read_only && (flags & O_DIRECTORY)) {
		int e = open_rights(path, kernel_flags, RADIX_RIGHTS_READ | RADIX_RIGHTS_WRITE, fd,
				&kind);
		if(e != EROFS && e != EACCES) {
			if(!e) {
				radix::set_dir_cursor(*fd, 0);
				radix::note_dir_path(*fd, path);
			}
			return e;
		}
	}
	if(int e = open_rights(path, kernel_flags, rights, fd, &kind); e)
		return e;
	if(kind != RADIX_FILE_KIND_DIRECTORY)
		return 0;
	if(!read_only) {
		// POSIX opens a directory for reading only; the write right a
		// directory capability has is for making files beneath it.
		radix_capability_delete(radix::slot_of(*fd));
		return EISDIR;
	}
	radix::set_dir_cursor(*fd, 0);
	if(!(flags & O_DIRECTORY)) {
		// It turned out to be a directory: again, with the write right if
		// it may have it, in the same descriptor.
		int wider;
		uint8_t again;
		if(!open_rights(path, kernel_flags, RADIX_RIGHTS_READ | RADIX_RIGHTS_WRITE, &wider,
				&again)) {
			uint32_t slot = radix::slot_of(*fd);
			radix_capability_delete(slot);
			if(radix_capability_move_to(radix::slot_of(wider), slot))
				*fd = wider;
		}
	}
	radix::note_dir_path(*fd, path);
	return 0;
}

// What the file `fd` is, as `struct stat`.
int stat_fd(int fd, struct stat *st) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_file_stat_results f;
	uint64_t status = radix_file_stat(slot, &f);
	if(!status) {
		radix::fill_stat(st, f.kind, f.size, f.allocated, f.id, f.created, f.modified, f.accessed,
				f.mount);
		return 0;
	}
	if(!other_kind(status))
		return radix::to_errno(status);
	// A directory.
	struct radix_directory_stat_results d;
	status = radix_directory_stat(slot, ".", 1, 0, &d);
	if(status)
		return radix::to_errno(status);
	radix::fill_stat(st, d.kind, d.size, d.allocated, d.id, d.created, d.modified, d.accessed,
			d.mount);
	return 0;
}

// The kernel's PathFlags for the AT_ flags `flags`.
uint32_t path_flags(int flags) {
	return flags & AT_SYMLINK_NOFOLLOW ? RADIX_PATH_FLAGS_NO_FOLLOW : 0;
}

// What the file at `path` is, as `struct stat`: with `flags`
// AT_SYMLINK_NOFOLLOW, a symbolic link there itself.
int stat_path(const radix::Path &path, struct stat *st, int flags = 0) {
	struct radix_directory_stat_results d;
	uint64_t status = radix_directory_stat(path.dir, path.text, path.len, path_flags(flags), &d);
	if(status)
		return radix::to_errno(status);
	radix::fill_stat(st, d.kind, d.size, d.allocated, d.id, d.created, d.modified, d.accessed,
			d.mount);
	return 0;
}

// Whether `path` names a directory: 0, ENOTDIR or an errno.
int must_be_dir(const radix::Path &path) {
	struct stat st;
	if(int e = stat_path(path, &st); e)
		return e;
	return S_ISDIR(st.st_mode) ? 0 : ENOTDIR;
}

// Removes `path`: a directory only with `directory`, which fails with
// ENOTDIR on anything else; a file only without, which fails with EISDIR
// on a directory. A symbolic link is removed itself, as a file.
int remove_path(const radix::Path &path, bool directory) {
	struct stat st;
	if(int e = stat_path(path, &st, AT_SYMLINK_NOFOLLOW); e)
		return e;
	if(directory && !S_ISDIR(st.st_mode))
		return ENOTDIR;
	if(!directory && S_ISDIR(st.st_mode))
		return EISDIR;
	return radix::to_errno(radix_directory_remove(path.dir, path.text, path.len));
}

// The rights of the capability for descriptor `fd`.
int rights_of(int fd, uint64_t *rights) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_capability_info_results info;
	if(uint64_t status = radix_capability_info(slot, &info); status)
		return radix::to_errno(status);
	*rights = info.rights;
	return 0;
}

// The errno of a lock that failed with `status`, as POSIX has it: a
// descriptor not open for what the lock guards is a bad one for it, and
// no memory for it is no locks.
int lock_errno(uint64_t status) {
	if(is(status, RADIX_KERNEL_MISSING_RIGHTS))
		return EBADF;
	if(is(status, RADIX_KERNEL_NO_MEMORY))
		return ENOLCK;
	return radix::to_errno(status);
}

// fcntl's record lock requests (F_GETLK, F_SETLK, F_SETLKW) and their
// open file description forms (F_OFD_...) with `lock` (radix-file(2)'s
// `lock` and `lock_holder`).
int record_lock(int fd, int request, struct flock *lock) {
	uint32_t slot = radix::slot_of(fd);
	bool ofd = request == F_OFD_GETLK || request == F_OFD_SETLK || request == F_OFD_SETLKW;
	bool testing = request == F_GETLK || request == F_OFD_GETLK;
	enum radix_lock_kind kind;
	switch(lock->l_type) {
	case F_RDLCK: kind = RADIX_LOCK_KIND_SHARED; break;
	case F_WRLCK: kind = RADIX_LOCK_KIND_EXCLUSIVE; break;
	case F_UNLCK:
		if(testing)
			return EINVAL;
		kind = RADIX_LOCK_KIND_NONE;
		break;
	default:
		return EINVAL;
	}
	// Linux asks open file description locks for a process number of 0.
	if(ofd && lock->l_pid)
		return EINVAL;
	// Where the range starts: counted from the start, the description's
	// offset or the end of the file.
	off_t base = 0;
	switch(lock->l_whence) {
	case SEEK_SET:
		break;
	case SEEK_CUR:
	case SEEK_END: {
		struct radix_file_seek_results r;
		uint64_t status = radix_file_seek(slot, 0, static_cast<enum radix_whence>(lock->l_whence),
				&r);
		if(status)
			return other_kind(status) ? EINVAL : radix::to_errno(status);
		base = static_cast<off_t>(r.offset);
		break;
	}
	default:
		return EINVAL;
	}
	// A negative length is the bytes before the start.
	off_t start, length = lock->l_len;
	if(__builtin_add_overflow(base, lock->l_start, &start))
		return EOVERFLOW;
	if(length < 0) {
		if(length == INT64_MIN || __builtin_add_overflow(start, length, &start))
			return EINVAL;
		length = -length;
	}
	if(start < 0)
		return EINVAL;
	off_t last;
	if(length && __builtin_add_overflow(start, length - 1, &last))
		return EOVERFLOW;
	auto owner = ofd ? RADIX_LOCK_OWNER_DESCRIPTION_RANGE : RADIX_LOCK_OWNER_PROCESS;
	if(!testing) {
		// The wait for a lock is a cancellation point.
		bool wait = request == F_SETLKW || request == F_OFD_SETLKW;
		uint64_t status = radix_file_lock_with(wait ? radix::cancellable : radix_syscall, slot,
				owner, kind, start, length, wait);
		return status ? lock_errno(status) : 0;
	}
	struct radix_file_lock_holder_results r;
	if(uint64_t status = radix_file_lock_holder(slot, owner, kind, start, length, &r); status)
		return lock_errno(status);
	if(r.kind == RADIX_LOCK_KIND_NONE) {
		lock->l_type = F_UNLCK;
		return 0;
	}
	lock->l_type = r.kind == RADIX_LOCK_KIND_SHARED ? F_RDLCK : F_WRLCK;
	lock->l_whence = SEEK_SET;
	lock->l_start = static_cast<off_t>(r.start);
	lock->l_len = static_cast<off_t>(r.length);
	// An open file description's lock has no process: -1, as on Linux.
	lock->l_pid = r.process == UINT64_MAX ? -1 : static_cast<pid_t>(r.process);
	return 0;
}

// Copies descriptor `fd` into the lowest free descriptor from `min_fd`.
int dup_from(int fd, int min_fd, bool close_on_exec, int *newfd) {
	uint64_t rights;
	if(int e = rights_of(fd, &rights); e)
		return e;
	uint32_t from = radix::slot_of(fd);
	if(int e = radix::place(min_fd, newfd, [&](uint32_t slot) {
			return radix_capability_derive(from, slot, rights, 0);
		}); e)
		return e;
	radix::set_dir_cursor(*newfd, radix::dir_cursor(fd));
	radix::copy_dir_path(fd, *newfd);
	if(close_on_exec) {
		struct radix_capability_flags_results old;
		radix_capability_flags(radix::slot_of(*newfd), true, RADIX_SLOT_FLAGS_CLOSE_ON_EXEC, &old);
	}
	return 0;
}

// The descriptor a name in /devices/fd stands for (ADR 0084): fd/N for
// descriptor N, and stdin, stdout and stderr for 0, 1 and 2, as Linux and
// the BSDs have them in /dev, where scripts and the shell's process
// substitution (`diff <(a) <(b)`) find them. /dev is a link to /devices
// (ADR 0113), and the names are the same in both. No filesystem holds a
// process's descriptors, so the C library gives these names itself.
// Returns -1 for any other path.
int named_fd(const radix::Path &path) {
	if(path.dir != radix::SLOT_ROOT)
		return -1;
	// The bytes of the path from `at` on start with `word`. (The dynamic
	// linker, which has this code too, has no strncmp.)
	auto starts = [&](size_t at, const char *word) {
		size_t n = strlen(word);
		if(path.len < at + n)
			return false;
		for(size_t i = 0; i < n; i++) {
			if(path.text[at + i] != word[i])
				return false;
		}
		return true;
	};
	size_t at;
	if(starts(0, "devices/"))
		at = 8;
	else if(starts(0, "dev/"))
		at = 4;
	else
		return -1;
	static const char *const STANDARD[] = {"stdin", "stdout", "stderr"};
	for(int i = 0; i < 3; i++) {
		if(path.len == at + strlen(STANDARD[i]) && starts(at, STANDARD[i]))
			return i;
	}
	if(path.len <= at + 3 || !starts(at, "fd/"))
		return -1;
	// Only the number as it is written: no sign, and no leading zero.
	const char *digits = path.text + at + 3;
	if(digits[0] == '0' && digits[1])
		return -1;
	int fd = 0;
	for(const char *p = digits; *p; p++) {
		if(*p < '0' || *p > '9')
			return -1;
		fd = fd * 10 + (*p - '0');
		if(fd >= radix::MAX_FDS)
			return -1;
	}
	return fd;
}

// What the descriptor `from`, which a name in /devices/fd stands for, is:
// ENOENT if it is not open, as there is then no such name.
int stat_named(int from, struct stat *st) {
	uint64_t rights;
	if(rights_of(from, &rights))
		return ENOENT;
	return stat_fd(from, st);
}

// Opens descriptor `from` by its name in /devices/fd, with openat's `flags`:
// a copy of it, as dup makes, which shares its open file description and
// offset, as on the BSDs (Linux opens a file anew, at its start). As
// there, it has no more access than `from` has, and O_TRUNC does nothing.
int open_named(int from, int flags, int *fd) {
	uint64_t rights;
	if(rights_of(from, &rights))
		return ENOENT;
	struct stat st;
	if(int e = stat_fd(from, &st); e)
		return e;
	if((flags & O_CREAT) && (flags & O_EXCL))
		return EEXIST;
	bool reads = access_mode(flags) != O_WRONLY;
	bool writes = access_mode(flags) != O_RDONLY;
	if(S_ISDIR(st.st_mode) && writes)
		return EISDIR;
	if(!S_ISDIR(st.st_mode) && (flags & O_DIRECTORY))
		return ENOTDIR;
	if((reads && !(rights & RADIX_RIGHTS_READ)) || (writes && !(rights & RADIX_RIGHTS_WRITE)))
		return EACCES;
	return dup_from(from, 0, flags & O_CLOEXEC, fd);
}

} // namespace

namespace mlibc {

int Sysdeps<Open>::operator()(const char *pathname, int flags, mode_t mode, int *fd) {
	return sysdep<Openat>(AT_FDCWD, pathname, flags, mode, fd);
}

int Sysdeps<Openat>::operator()(int dirfd, const char *path, int flags, mode_t mode, int *fd) {
	(void)mode;
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, path, &resolved); e)
		return e;
	if(int from = named_fd(resolved); from >= 0)
		return open_named(from, flags, fd);
	return open_path(resolved, flags, fd);
}

int Sysdeps<OpenDir>::operator()(const char *path, int *handle) {
	return sysdep<Open>(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC, 0, handle);
}

// readdir lists `.` and `..` first, as Linux does, though the kernel's
// listings leave them out: their cursors are above any a filesystem gives.
constexpr uint64_t DOTDOT_CURSOR = (uint64_t(1) << 62) | 1;
constexpr uint64_t LISTING_CURSOR = (uint64_t(1) << 62) | 2;

// Appends a dirent for `name` to the `max_size` bytes at `out`, of which
// `used` are taken; false if it does not fit.
static bool put_entry(char *out, size_t max_size, size_t &used, const char *name, size_t len,
		uint64_t next, unsigned char type) {
	size_t reclen = (offsetof(struct dirent, d_name) + len + 1 + 7) & ~size_t(7);
	if(used + reclen > max_size)
		return false;
	struct dirent *d = reinterpret_cast<struct dirent *>(out + used);
	// Listings carry no file numbers; the cursor is unique in the
	// directory, and never 0, which some programs skip.
	d->d_ino = next;
	d->d_off = static_cast<off_t>(next);
	d->d_reclen = static_cast<unsigned short>(reclen);
	d->d_type = type;
	memcpy(d->d_name, name, len);
	d->d_name[len] = 0;
	used += reclen;
	return true;
}

int Sysdeps<ReadEntries>::operator()(int handle, void *buffer, size_t max_size,
		size_t *bytes_read) {
	uint32_t slot = radix::slot_of(handle);
	if(slot == radix::NO_SLOT)
		return EBADF;
	auto out = static_cast<char *>(buffer);
	size_t used = 0;
	uint64_t cursor = radix::dir_cursor(handle);
	if(cursor == 0) {
		// Only a directory lists.
		struct radix_directory_stat_results d;
		if(uint64_t status = radix_directory_stat(slot, ".", 1, 0, &d); status)
			return other_kind(status) ? ENOTDIR : radix::to_errno(status);
		if(!put_entry(out, max_size, used, ".", 1, DOTDOT_CURSOR, DT_DIR))
			return EINVAL;
		cursor = DOTDOT_CURSOR;
	}
	if(cursor == DOTDOT_CURSOR && put_entry(out, max_size, used, "..", 2, LISTING_CURSOR, DT_DIR))
		cursor = LISTING_CURSOR;
	// A record here is at most 8 bytes longer than the kernel's (16 bytes
	// and the name, 8-byte aligned), so asking for two thirds of the room
	// always fits.
	char listed[1536];
	size_t ask = (max_size - used) * 2 / 3;
	if(ask > sizeof(listed))
		ask = sizeof(listed);
	if(cursor != DOTDOT_CURSOR) {
		struct radix_directory_list_results r;
		uint64_t from = cursor == LISTING_CURSOR ? 0 : cursor;
		if(uint64_t status = radix_directory_list(slot, from, listed, ask, &r); status) {
			if(!used)
				return radix::to_errno(status);
			r.count = 0;
		}
		size_t at = 0;
		for(uint32_t i = 0; i < r.count; i++) {
			struct radix_dir_entry entry;
			memcpy(&entry, listed + at, sizeof(entry));
			const char *name = listed + at + sizeof(entry);
			at += (sizeof(entry) + entry.name_len + 7) & ~size_t(7);
			unsigned char type;
			switch(entry.kind) {
			case RADIX_FILE_KIND_DIRECTORY: type = DT_DIR; break;
			case RADIX_FILE_KIND_BLOCK_DEVICE: type = DT_BLK; break;
			case RADIX_FILE_KIND_CHAR_DEVICE: type = DT_CHR; break;
			case RADIX_FILE_KIND_PIPE: type = DT_FIFO; break;
			case RADIX_FILE_KIND_SYMLINK: type = DT_LNK; break;
			case RADIX_FILE_KIND_SOCKET: type = DT_SOCK; break;
			default: type = DT_REG; break;
			}
			if(!put_entry(out, max_size, used, name, entry.name_len, entry.next, type))
				break;
			cursor = entry.next;
		}
	}
	radix::set_dir_cursor(handle, cursor);
	*bytes_read = used;
	return 0;
}

namespace {

// Reads up to `count` bytes of `fd` into `buf`, from `offset` or, with
// RADIX_FILE_CURRENT, the description's offset, made with `syscall`.
int read_with(radix_syscall_fn *syscall, int fd, uint64_t offset, void *buf, size_t count,
		ssize_t *bytes_read) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_file_read_results r;
	uint64_t status = radix_file_read_with(syscall, slot, offset, buf, count, &r);
	if(status)
		return io_errno(status, false);
	*bytes_read = static_cast<ssize_t>(r.count);
	return 0;
}

// Writes `count` bytes of `buf` to `fd` as read_with reads.
int write_with(radix_syscall_fn *syscall, int fd, uint64_t offset, const void *buf, size_t count,
		ssize_t *bytes_written) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_file_write_results r;
	uint64_t status = radix_file_write_with(syscall, slot, offset, buf, count, &r);
	if(status)
		return io_errno(status, true);
	*bytes_written = static_cast<ssize_t>(r.count);
	return 0;
}

} // namespace

int Sysdeps<Read>::operator()(int fd, void *buf, size_t count, ssize_t *bytes_read) {
	return read_with(radix_syscall, fd, RADIX_FILE_CURRENT, buf, count, bytes_read);
}

int Sysdeps<Write>::operator()(int fd, const void *buf, size_t count, ssize_t *bytes_written) {
	return write_with(radix_syscall, fd, RADIX_FILE_CURRENT, buf, count, bytes_written);
}

// Not in the dynamic linker, which has no threads to cancel; nor are the
// rest of the cancellation points.
#ifndef MLIBC_BUILDING_RTLD
// read(2) and write(2), cancellation points, unlike the C library's own
// reads and writes.
int Sysdeps<CancellableRead>::operator()(int fd, void *buf, size_t count, ssize_t *bytes_read) {
	return read_with(radix::cancellable, fd, RADIX_FILE_CURRENT, buf, count, bytes_read);
}

int Sysdeps<CancellableWrite>::operator()(int fd, const void *buf, size_t count,
		ssize_t *bytes_written) {
	return write_with(radix::cancellable, fd, RADIX_FILE_CURRENT, buf, count, bytes_written);
}

int Sysdeps<Pread>::operator()(int fd, void *buf, size_t n, off_t off, ssize_t *bytes_read) {
	if(off < 0)
		return radix::slot_of(fd) == radix::NO_SLOT ? EBADF : EINVAL;
	return read_with(radix::cancellable, fd, static_cast<uint64_t>(off), buf, n, bytes_read);
}

int Sysdeps<Pwrite>::operator()(int fd, const void *buf, size_t n, off_t off,
		ssize_t *bytes_written) {
	if(off < 0)
		return radix::slot_of(fd) == radix::NO_SLOT ? EBADF : EINVAL;
	return write_with(radix::cancellable, fd, static_cast<uint64_t>(off), buf, n,
			bytes_written);
}

// Only the first read of readv, and the first write of writev, can act on
// a cancel: one that comes after it has taken bytes leaves them for the
// call to return.
int Sysdeps<Readv>::operator()(int fd, const struct iovec *iovs, int iovc, ssize_t *bytes_read) {
	size_t sum;
	if(iovc < 0 || iovc > IOV_MAX)
		return EINVAL;
	if(int e = radix::iov_total(iovs, iovc, &sum); e)
		return e;
	ssize_t total = 0;
	radix_syscall_fn *syscall = radix::cancellable;
	for(int i = 0; i < iovc; i++) {
		if(!iovs[i].iov_len)
			continue;
		ssize_t got;
		int e = read_with(syscall, fd, RADIX_FILE_CURRENT, iovs[i].iov_base, iovs[i].iov_len,
				&got);
		syscall = radix_syscall;
		if(e) {
			if(total)
				break;
			return e;
		}
		total += got;
		if(static_cast<size_t>(got) < iovs[i].iov_len)
			break;
	}
	// With no bytes to move it is a cancellation point all the same.
	if(syscall == radix::cancellable)
		thread_testcancel();
	*bytes_read = total;
	return 0;
}

int Sysdeps<Writev>::operator()(int fd, const struct iovec *iovs, int iovc,
		ssize_t *bytes_written) {
	size_t sum;
	if(iovc < 0 || iovc > IOV_MAX)
		return EINVAL;
	if(int e = radix::iov_total(iovs, iovc, &sum); e)
		return e;
	// Up to PIPE_BUF bytes go in one write, which a pipe never mixes with
	// another's.
	if(iovc > 1 && sum <= PIPE_BUF) {
		char gathered[PIPE_BUF];
		size_t at = 0;
		for(int i = 0; i < iovc; i++) {
			memcpy(gathered + at, iovs[i].iov_base, iovs[i].iov_len);
			at += iovs[i].iov_len;
		}
		return write_with(radix::cancellable, fd, RADIX_FILE_CURRENT, gathered, sum,
				bytes_written);
	}
	ssize_t total = 0;
	radix_syscall_fn *syscall = radix::cancellable;
	for(int i = 0; i < iovc; i++) {
		if(!iovs[i].iov_len)
			continue;
		ssize_t put;
		int e = write_with(syscall, fd, RADIX_FILE_CURRENT, iovs[i].iov_base, iovs[i].iov_len,
				&put);
		syscall = radix_syscall;
		if(e) {
			if(total)
				break;
			return e;
		}
		total += put;
		if(static_cast<size_t>(put) < iovs[i].iov_len)
			break;
	}
	// With no bytes to move it is a cancellation point all the same.
	if(syscall == radix::cancellable)
		thread_testcancel();
	*bytes_written = total;
	return 0;
}
#endif

int Sysdeps<Seek>::operator()(int fd, off_t offset, int whence, off_t *new_offset) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	if(whence < SEEK_SET || whence > SEEK_END)
		return EINVAL;
	struct radix_file_seek_results r;
	uint64_t status = radix_file_seek(slot, offset, static_cast<enum radix_whence>(whence), &r);
	if(!status) {
		*new_offset = static_cast<off_t>(r.offset);
		return 0;
	}
	if(!other_kind(status))
		return radix::to_errno(status);
	// A directory: its listing's cursor, which readdir's offsets are
	// (rewinddir and seekdir).
	if(whence != SEEK_SET || offset < 0)
		return EINVAL;
	radix::set_dir_cursor(fd, static_cast<uint64_t>(offset));
	*new_offset = offset;
	return 0;
}

int Sysdeps<Close>::operator()(int fd) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	radix::forget_dir_path(fd);
	return radix::to_errno(radix_capability_delete(slot));
}

int Sysdeps<Dup>::operator()(int fd, int flags, int *newfd) {
	return dup_from(fd, 0, flags & O_CLOEXEC, newfd);
}

int Sysdeps<Dup2>::operator()(int fd, int flags, int newfd) {
	uint32_t from = radix::slot_of(fd);
	uint32_t to = radix::slot_of(newfd);
	if(from == radix::NO_SLOT || to == radix::NO_SLOT || newfd >= radix::files_limit())
		return EBADF;
	uint64_t rights;
	if(int e = rights_of(fd, &rights); e)
		return e;
	if(fd == newfd)
		return 0;
	for(;;) {
		// Not atomic as POSIX asks: another thread may open newfd between
		// the close and the copy, which then takes it again.
		radix_capability_delete(to);
		uint64_t status = radix_capability_derive(from, to, rights, 0);
		if(!status)
			break;
		if(is(status, RADIX_KERNEL_OCCUPIED))
			continue;
		if(is(status, RADIX_KERNEL_NO_SUCH_SLOT)) {
			// Past the space's end: grow it, then try again.
			uint32_t slot;
			if(int e = radix::free_slot(newfd, &slot); e)
				return e;
			continue;
		}
		return radix::to_errno(status);
	}
	radix::set_dir_cursor(newfd, radix::dir_cursor(fd));
	radix::copy_dir_path(fd, newfd);
	if(flags & O_CLOEXEC) {
		struct radix_capability_flags_results old;
		radix_capability_flags(to, true, RADIX_SLOT_FLAGS_CLOSE_ON_EXEC, &old);
	}
	return 0;
}

int Sysdeps<Fcntl>::operator()(int fd, int request, va_list args, int *result) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	switch(request) {
	case F_DUPFD:
	case F_DUPFD_CLOEXEC: {
		int min_fd = va_arg(args, int);
		if(min_fd < 0 || min_fd >= radix::files_limit())
			return EINVAL;
		return dup_from(fd, min_fd, request == F_DUPFD_CLOEXEC, result);
	}
	case F_GETFD: {
		struct radix_capability_flags_results r;
		if(uint64_t status = radix_capability_flags(slot, false, 0, &r); status)
			return radix::to_errno(status);
		*result = (r.flags & RADIX_SLOT_FLAGS_CLOSE_ON_EXEC) ? FD_CLOEXEC : 0;
		return 0;
	}
	case F_SETFD: {
		int value = va_arg(args, int);
		struct radix_capability_flags_results r;
		uint32_t flags = (value & FD_CLOEXEC) ? RADIX_SLOT_FLAGS_CLOSE_ON_EXEC : 0;
		*result = 0;
		return radix::to_errno(radix_capability_flags(slot, true, flags, &r));
	}
	case F_GETFL: {
		uint64_t rights;
		if(int e = rights_of(fd, &rights); e)
			return e;
		bool reads = rights & RADIX_RIGHTS_READ;
		bool writes = rights & RADIX_RIGHTS_WRITE;
		int flags = reads && writes ? O_RDWR : writes ? O_WRONLY : O_RDONLY;
		struct radix_file_status_flags_results r;
		uint64_t status = radix_file_status_flags(slot, false, 0, &r);
		if(!status) {
			if(r.flags & RADIX_DESCRIPTION_FLAGS_APPEND)
				flags |= O_APPEND;
			if(r.flags & RADIX_DESCRIPTION_FLAGS_NON_BLOCKING)
				flags |= O_NONBLOCK;
		} else if(!other_kind(status)) {
			return radix::to_errno(status);
		} else {
			// A directory is open for reading only, though its capability
			// may have the write right, for making files beneath it.
			flags = O_RDONLY | O_DIRECTORY;
		}
		*result = flags;
		return 0;
	}
	case F_SETFL: {
		int value = va_arg(args, int);
		uint32_t flags = 0;
		if(value & O_APPEND)
			flags |= RADIX_DESCRIPTION_FLAGS_APPEND;
		if(value & O_NONBLOCK)
			flags |= RADIX_DESCRIPTION_FLAGS_NON_BLOCKING;
		struct radix_file_status_flags_results r;
		uint64_t status = radix_file_status_flags(slot, true, flags, &r);
		*result = 0;
		// Directories have no status flags to set.
		if(other_kind(status))
			return 0;
		return radix::to_errno(status);
	}
	case F_GETLK:
	case F_OFD_GETLK:
	case F_SETLK:
	case F_SETLKW:
	case F_OFD_SETLK:
	case F_OFD_SETLKW:
		*result = 0;
		return record_lock(fd, request, va_arg(args, struct flock *));
	default:
		return EINVAL;
	}
}

int Sysdeps<Stat>::operator()(mlibc::fsfd_target fsfdt, int fd, const char *path, int flags,
		struct stat *statbuf) {
	switch(fsfdt) {
	case fsfd_target::fd:
		return stat_fd(fd, statbuf);
	case fsfd_target::fd_path:
		if((flags & AT_EMPTY_PATH) && path && !*path)
			return stat_fd(fd, statbuf);
		break;
	case fsfd_target::path:
		fd = AT_FDCWD;
		break;
	default:
		return EINVAL;
	}
	radix::Path resolved;
	if(int e = radix::resolve(fd, path, &resolved); e)
		return e;
	if(int from = named_fd(resolved); from >= 0)
		return stat_named(from, statbuf);
	return stat_path(resolved, statbuf, flags);
}

// Not in the dynamic linker, which has no strnlen (core.cpp).
#ifndef MLIBC_BUILDING_RTLD
int Sysdeps<Statvfs>::operator()(const char *path, struct statvfs *out) {
	// Opened only to be asked about: a terminal does not become the
	// controlling one, and a named pipe does not wait for a writer.
	int fd;
	if(int e = sysdep<Open>(path, O_RDONLY | O_CLOEXEC | O_NOCTTY | O_NONBLOCK, 0, &fd); e)
		return e;
	int e = sysdep<Fstatvfs>(fd, out);
	sysdep<Close>(fd);
	if(e != ENOSYS)
		return e;
	// A file: its directory is on the same filesystem.
	char dir[PATH_MAX];
	size_t len = strnlen(path, sizeof(dir));
	if(len == sizeof(dir))
		return ENAMETOOLONG;
	while(len > 1 && path[len - 1] == '/')
		len--;
	while(len && path[len - 1] != '/')
		len--;
	if(!len) {
		dir[0] = '.';
		len = 1;
	} else {
		memcpy(dir, path, len);
	}
	dir[len] = 0;
	if((e = sysdep<Open>(dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC, 0, &fd)))
		return e;
	e = sysdep<Fstatvfs>(fd, out);
	sysdep<Close>(fd);
	return e;
}
#endif

int Sysdeps<Fstatvfs>::operator()(int fd, struct statvfs *out) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_directory_info_results r;
	uint64_t status = radix_directory_info(slot, &r);
	if(other_kind(status))
		// A file: the kernel tells about its filesystem only through
		// directories, yet.
		return ENOSYS;
	if(status)
		return radix::to_errno(status);
	memset(out, 0, sizeof(*out));
	out->f_bsize = r.block_size;
	out->f_frsize = r.block_size;
	out->f_blocks = r.blocks;
	out->f_bfree = r.free;
	out->f_bavail = r.free;
	out->f_files = r.files;
	out->f_ffree = r.free_files;
	out->f_favail = r.free_files;
	out->f_namemax = NAME_MAX;
	if(r.flags & RADIX_MOUNT_FLAGS_READ_ONLY)
		out->f_flag |= ST_RDONLY;
	return 0;
}

int Sysdeps<Access>::operator()(const char *path, int mode) {
	return sysdep<Faccessat>(AT_FDCWD, path, mode, 0);
}

int Sysdeps<Faccessat>::operator()(int dirfd, const char *pathname, int mode, int flags) {
	(void)mode;
	// There are no permissions until users come (Phase 13): a file that
	// exists may be read, written and run.
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, pathname, &resolved); e)
		return e;
	struct stat st;
	if(int from = named_fd(resolved); from >= 0)
		return stat_named(from, &st);
	return stat_path(resolved, &st, flags);
}

int Sysdeps<Mkdir>::operator()(const char *path, mode_t mode) {
	return sysdep<Mkdirat>(AT_FDCWD, path, mode);
}

int Sysdeps<Mkdirat>::operator()(int dirfd, const char *path, mode_t mode) {
	(void)mode;
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, path, &resolved); e)
		return e;
	return radix::to_errno(radix_directory_make_directory(resolved.dir, resolved.text,
			resolved.len));
}

int Sysdeps<Rmdir>::operator()(const char *path) {
	return sysdep<Unlinkat>(AT_FDCWD, path, AT_REMOVEDIR);
}

int Sysdeps<Unlinkat>::operator()(int dirfd, const char *path, int flags) {
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, path, &resolved); e)
		return e;
	return remove_path(resolved, flags & AT_REMOVEDIR);
}

int Sysdeps<Rename>::operator()(const char *path, const char *new_path) {
	return sysdep<Renameat>(AT_FDCWD, path, AT_FDCWD, new_path);
}

int Sysdeps<Renameat>::operator()(int olddirfd, const char *old_path, int newdirfd,
		const char *new_path) {
	radix::Path from;
	if(int e = radix::resolve(olddirfd, old_path, &from); e)
		return e;
	radix::Path to;
	if(int e = radix::resolve(newdirfd, new_path, &to); e)
		return e;
	return radix::to_errno(radix_directory_rename(from.dir, from.text, from.len, to.dir, to.text,
			to.len));
}

int Sysdeps<Truncate>::operator()(const char *path, off_t length) {
	if(length < 0)
		return EINVAL;
	int fd;
	if(int e = sysdep<Open>(path, O_WRONLY | O_CLOEXEC, 0, &fd); e)
		return e;
	int e = sysdep<Ftruncate>(fd, length);
	sysdep<Close>(fd);
	return e;
}

// As on Linux, a negative length, a descriptor not open for writing and
// one that is no file are EINVAL. ftruncate passes its off_t as it is.
int Sysdeps<Ftruncate>::operator()(int fd, size_t size) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	if(static_cast<off_t>(size) < 0)
		return EINVAL;
	uint64_t status = radix_file_set_size(slot, size);
	if(other_kind(status) || is(status, RADIX_KERNEL_MISSING_RIGHTS))
		return EINVAL;
	int e = radix::to_errno(status);
	return e == EOPNOTSUPP ? EINVAL : e;
}

// posix_fallocate's errors in Linux's order: a negative offset or a
// length that is not positive, then a descriptor not open for writing (a
// directory never is), a pipe, anything but a file, and an end past the
// largest offset. posix_fallocate passes its off_t length as it is.
int Sysdeps<Fallocate>::operator()(int fd, off_t offset, size_t size) {
	auto length = static_cast<off_t>(size);
	if(offset < 0 || length <= 0)
		return EINVAL;
	struct stat st;
	if(int e = stat_fd(fd, &st); e)
		return e;
	uint64_t rights;
	if(int e = rights_of(fd, &rights); e)
		return e;
	if(S_ISDIR(st.st_mode) || !(rights & RADIX_RIGHTS_WRITE))
		return EBADF;
	if(S_ISFIFO(st.st_mode))
		return ESPIPE;
	if(!S_ISREG(st.st_mode))
		return ENODEV;
	if(length > INT64_MAX - offset)
		return EFBIG;
	off_t end = offset + length;
	if(end <= st.st_size)
		return 0;
	return sysdep<Ftruncate>(fd, end);
}

// A cancellation point, as fdatasync is.
int Sysdeps<Fsync>::operator()(int fd) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	uint64_t status = radix_file_sync_with(radix::cancellable, slot);
	if(other_kind(status))
		status = radix_directory_sync_with(radix::cancellable, slot);
	return radix::to_errno(status);
}

int Sysdeps<Fdatasync>::operator()(int fd) {
	return sysdep<Fsync>(fd);
}

void Sysdeps<Sync>::operator()() {
	// The root's filesystem; each writes its changes back every five
	// seconds anyway (radix-file(2)).
	radix_directory_sync(radix::SLOT_ROOT);
}

int Sysdeps<Chdir>::operator()(const char *path) {
	radix::Path resolved;
	if(int e = radix::resolve(AT_FDCWD, path, &resolved); e)
		return e;
	if(int e = must_be_dir(resolved); e)
		return e;
	// Where it really is, links followed, so that getcwd says so and
	// `..` from it leads where the kernel's would.
	char absolute[radix::PATH_MAX_BYTES];
	if(int e = sysdep<Realpath>(path, absolute, sizeof(absolute)); e)
		return e;
	if(int e = radix::set_cwd(absolute); e)
		return e;
	// The kernel shows it in /processes (radix-proc(5)); a failure there is
	// no failure of chdir.
	const char *dir = radix::cwd();
	radix_process_set_directory(radix::SLOT_PROCESS, dir, strlen(dir));
	return 0;
}

int Sysdeps<Fchdir>::operator()(int fd) {
	struct stat st;
	if(int e = stat_fd(fd, &st); e)
		return e;
	if(!S_ISDIR(st.st_mode))
		return ENOTDIR;
	// By the path the C library noted for it (ADR 0089): a descriptor
	// does not know its own, and one from before exec has none.
	char path[radix::PATH_MAX_BYTES + 1];
	path[0] = '/';
	if(!radix::dir_path(fd, path + 1))
		return ENOSYS;
	return sysdep<Chdir>(path);
}

int Sysdeps<GetCwd>::operator()(char *buffer, size_t size) {
	const char *dir = radix::cwd();
	size_t len = strlen(dir);
	if(len + 1 > size)
		return ERANGE;
	memcpy(buffer, dir, len + 1);
	return 0;
}

int Sysdeps<Readlink>::operator()(const char *path, void *buffer, size_t max_size,
		ssize_t *length) {
	return sysdep<Readlinkat>(AT_FDCWD, path, buffer, max_size, length);
}

int Sysdeps<Readlinkat>::operator()(int dirfd, const char *path, void *buffer, size_t max_size,
		ssize_t *length) {
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, path, &resolved); e)
		return e;
	struct radix_directory_read_link_results r;
	if(uint64_t status = radix_directory_read_link(resolved.dir, resolved.text, resolved.len,
			buffer, max_size, &r); status)
		return radix::to_errno(status);
	// What did not fit is cut off, as POSIX has it.
	*length = static_cast<ssize_t>(r.len < max_size ? r.len : max_size);
	return 0;
}

int Sysdeps<Realpath>::operator()(const char *path, char *out, size_t size) {
	radix::Path resolved;
	if(int e = radix::resolve(AT_FDCWD, path, &resolved); e)
		return e;
	if(size < 2)
		return ERANGE;
	// The kernel's answer is beneath the root: after a `/`, with room for
	// the NUL.
	struct radix_directory_real_path_results r;
	if(uint64_t status = radix_directory_real_path(resolved.dir, resolved.text, resolved.len,
			out + 1, size - 2, &r); status)
		return radix::to_errno(status);
	if(r.len > size - 2)
		return ENAMETOOLONG;
	out[0] = '/';
	if(r.len == 1 && out[1] == '.')
		out[1] = 0;
	else
		out[r.len + 1] = 0;
	return 0;
}

int Sysdeps<Link>::operator()(const char *old_path, const char *new_path) {
	return sysdep<Linkat>(AT_FDCWD, old_path, AT_FDCWD, new_path, 0);
}

int Sysdeps<Linkat>::operator()(int olddirfd, const char *old_path, int newdirfd,
		const char *new_path, int flags) {
	(void)olddirfd;
	(void)old_path;
	(void)newdirfd;
	(void)new_path;
	(void)flags;
	// No filesystem has hard links yet.
	return EPERM;
}

int Sysdeps<Symlink>::operator()(const char *target_path, const char *link_path) {
	return sysdep<Symlinkat>(target_path, AT_FDCWD, link_path);
}

int Sysdeps<Symlinkat>::operator()(const char *target_path, int dirfd, const char *link_path) {
	if(!target_path)
		return EFAULT;
	size_t len = strlen(target_path);
	if(!len)
		return ENOENT;
	if(len >= radix::PATH_MAX_BYTES)
		return ENAMETOOLONG;
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, link_path, &resolved); e)
		return e;
	return radix::to_errno(radix_directory_make_link(resolved.dir, resolved.text, resolved.len,
			target_path, len));
}

int Sysdeps<Chmod>::operator()(const char *pathname, mode_t mode) {
	return sysdep<Fchmodat>(AT_FDCWD, pathname, mode, 0);
}

int Sysdeps<Fchmod>::operator()(int fd, mode_t mode) {
	(void)mode;
	struct stat st;
	return stat_fd(fd, &st);
}

int Sysdeps<Fchmodat>::operator()(int fd, const char *pathname, mode_t mode, int flags) {
	(void)mode;
	// Files have no permissions to change until users come (Phase 13).
	radix::Path resolved;
	if(int e = radix::resolve(fd, pathname, &resolved); e)
		return e;
	struct stat st;
	return stat_path(resolved, &st, flags);
}

int Sysdeps<Fchownat>::operator()(int dirfd, const char *pathname, uid_t owner, gid_t group,
		int flags) {
	(void)owner;
	(void)group;
	if((flags & AT_EMPTY_PATH) && pathname && !*pathname) {
		struct stat st;
		return stat_fd(dirfd, &st);
	}
	// Files have no owners until users come (Phase 13).
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, pathname, &resolved); e)
		return e;
	struct stat st;
	return stat_path(resolved, &st, flags);
}

namespace {

// A time of utimensat's as set_times takes it: TIME_OMIT, TIME_NOW, or
// nanoseconds since 1970, which cannot be before 1970 or past 2554.
int kernel_time(const struct timespec &ts, uint64_t *out) {
	if(ts.tv_nsec == UTIME_OMIT) {
		*out = RADIX_FILE_TIME_OMIT;
		return 0;
	}
	if(ts.tv_nsec == UTIME_NOW) {
		*out = RADIX_FILE_TIME_NOW;
		return 0;
	}
	if(ts.tv_nsec < 0 || ts.tv_nsec >= 1000000000 || ts.tv_sec < 0)
		return EINVAL;
	uint64_t nsec = ts.tv_nsec;
	if(static_cast<uint64_t>(ts.tv_sec) > (RADIX_FILE_TIME_NOW - 1 - nsec) / 1000000000)
		return EINVAL;
	*out = static_cast<uint64_t>(ts.tv_sec) * 1000000000 + nsec;
	return 0;
}

} // namespace

int Sysdeps<Utimensat>::operator()(int dirfd, const char *pathname, const struct timespec times[2],
		int flags) {
	uint64_t accessed = RADIX_FILE_TIME_NOW, modified = RADIX_FILE_TIME_NOW;
	if(times) {
		if(int e = kernel_time(times[0], &accessed); e)
			return e;
		if(int e = kernel_time(times[1], &modified); e)
			return e;
	}
	if(!pathname) {
		// futimens: a file, or a directory as its own ".".
		uint32_t slot = radix::slot_of(dirfd);
		if(slot == radix::NO_SLOT)
			return EBADF;
		uint64_t status = radix_file_set_times(slot, accessed, modified);
		if(other_kind(status))
			status = radix_directory_set_times(slot, ".", 1, accessed, modified, 0);
		return radix::to_errno(status);
	}
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, pathname, &resolved); e)
		return e;
	return radix::to_errno(radix_directory_set_times(resolved.dir, resolved.text, resolved.len,
			accessed, modified, path_flags(flags)));
}

int Sysdeps<Umask>::operator()(mode_t mode, mode_t *old) {
	// Kept by the kernel, so that exec keeps it; only kept, as files have
	// no permissions until users come (Phase 13).
	struct radix_process_file_mask_results r;
	if(uint64_t status = radix_process_file_mask(radix::SLOT_PROCESS, true, mode & 0777, &r);
			status)
		return radix::to_errno(status);
	*old = static_cast<mode_t>(r.mask);
	return 0;
}

int Sysdeps<Mkfifoat>::operator()(int dirfd, const char *path, mode_t mode) {
	(void)dirfd;
	(void)path;
	(void)mode;
	// Named pipes need a filesystem that keeps them (docs/design/posix.md).
	return ENOSYS;
}

int Sysdeps<Mknodat>::operator()(int dirfd, const char *path, int mode, int dev) {
	(void)dirfd;
	(void)path;
	(void)mode;
	(void)dev;
	return EPERM;
}

int Sysdeps<Pipe>::operator()(int *fds, int flags) {
	uint32_t pipe_flags = 0;
	if(flags & O_NONBLOCK)
		pipe_flags |= RADIX_PIPE_FLAGS_NON_BLOCKING;
	if(flags & O_CLOEXEC)
		pipe_flags |= RADIX_PIPE_FLAGS_CLOSE_ON_EXEC;
	if(flags & ~(O_NONBLOCK | O_CLOEXEC | O_DIRECT))
		return EINVAL;
	for(;;) {
		uint32_t read_slot;
		uint32_t write_slot;
		if(int e = radix::free_slot(0, &read_slot); e)
			return e;
		int next = static_cast<int>(read_slot - radix::FD_BASE) + 1;
		if(next >= radix::MAX_FDS)
			return EMFILE;
		if(int e = radix::free_slot(next, &write_slot); e)
			return e;
		uint64_t status = radix_process_pipe(radix::SLOT_PROCESS, read_slot, write_slot,
				pipe_flags);
		if(is(status, RADIX_KERNEL_OCCUPIED))
			continue;
		if(status)
			return radix::to_errno(status);
		fds[0] = static_cast<int>(read_slot - radix::FD_BASE);
		fds[1] = static_cast<int>(write_slot - radix::FD_BASE);
		return 0;
	}
}

int Sysdeps<Flock>::operator()(int fd, int options) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	enum radix_lock_kind kind;
	switch(options & ~LOCK_NB) {
	case LOCK_SH: kind = RADIX_LOCK_KIND_SHARED; break;
	case LOCK_EX: kind = RADIX_LOCK_KIND_EXCLUSIVE; break;
	case LOCK_UN: kind = RADIX_LOCK_KIND_NONE; break;
	default: return EINVAL;
	}
	// The description's lock on the whole file (radix-file(2)'s `lock`).
	uint64_t status = radix_file_lock(slot, RADIX_LOCK_OWNER_DESCRIPTION, kind, 0, 0,
			!(options & LOCK_NB));
	return status ? lock_errno(status) : 0;
}

int Sysdeps<Fadvise>::operator()(int fd, off_t offset, off_t length, int advice) {
	(void)offset;
	(void)length;
	(void)advice;
	struct stat st;
	return stat_fd(fd, &st);
}

} // namespace mlibc
