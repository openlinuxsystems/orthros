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

// Opens `path` as POSIX's openat does.
int open_path(const radix::Path &path, int flags, int *fd) {
	uint32_t kernel_flags = open_flags(flags);
	uint64_t rights;
	switch(flags & O_ACCMODE) {
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
	bool read_only = (flags & O_ACCMODE) == O_RDONLY;
	// A directory opened for reading gets the write right too, where its
	// filesystem allows it: POSIX lets openat make files beneath any
	// directory descriptor, which takes that right here.
	if(read_only && (flags & O_DIRECTORY)) {
		int e = open_rights(path, kernel_flags, RADIX_RIGHTS_READ | RADIX_RIGHTS_WRITE, fd,
				&kind);
		if(e != EROFS && e != EACCES) {
			if(!e)
				radix::set_dir_cursor(*fd, 0);
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
		radix::fill_stat(st, f.kind, f.size, f.allocated, f.id, f.created, f.modified);
		return 0;
	}
	if(!other_kind(status))
		return radix::to_errno(status);
	// A directory.
	struct radix_directory_stat_results d;
	status = radix_directory_stat(slot, ".", 1, &d);
	if(status)
		return radix::to_errno(status);
	radix::fill_stat(st, d.kind, d.size, d.allocated, d.id, d.created, d.modified);
	return 0;
}

// What the file at `path` is, as `struct stat`.
int stat_path(const radix::Path &path, struct stat *st) {
	struct radix_directory_stat_results d;
	uint64_t status = radix_directory_stat(path.dir, path.text, path.len, &d);
	if(status)
		return radix::to_errno(status);
	radix::fill_stat(st, d.kind, d.size, d.allocated, d.id, d.created, d.modified);
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
// on a directory.
int remove_path(const radix::Path &path, bool directory) {
	struct stat st;
	if(int e = stat_path(path, &st); e)
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

// Copies descriptor `fd` into the lowest free descriptor from `min_fd`.
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
		bool wait = request == F_SETLKW || request == F_OFD_SETLKW;
		uint64_t status = radix_file_lock(slot, owner, kind, start, length, wait);
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
	if(close_on_exec) {
		struct radix_capability_flags_results old;
		radix_capability_flags(radix::slot_of(*newfd), true, RADIX_SLOT_FLAGS_CLOSE_ON_EXEC, &old);
	}
	return 0;
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
		if(uint64_t status = radix_directory_stat(slot, ".", 1, &d); status)
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

int Sysdeps<Read>::operator()(int fd, void *buf, size_t count, ssize_t *bytes_read) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_file_read_results r;
	uint64_t status = radix_file_read(slot, RADIX_FILE_CURRENT, buf, count, &r);
	if(status)
		return io_errno(status, false);
	*bytes_read = static_cast<ssize_t>(r.count);
	return 0;
}

int Sysdeps<Write>::operator()(int fd, const void *buf, size_t count, ssize_t *bytes_written) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_file_write_results r;
	uint64_t status = radix_file_write(slot, RADIX_FILE_CURRENT, buf, count, &r);
	if(status)
		return io_errno(status, true);
	*bytes_written = static_cast<ssize_t>(r.count);
	return 0;
}

int Sysdeps<Pread>::operator()(int fd, void *buf, size_t n, off_t off, ssize_t *bytes_read) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	if(off < 0)
		return EINVAL;
	struct radix_file_read_results r;
	uint64_t status = radix_file_read(slot, static_cast<uint64_t>(off), buf, n, &r);
	if(status)
		return io_errno(status, false);
	*bytes_read = static_cast<ssize_t>(r.count);
	return 0;
}

int Sysdeps<Pwrite>::operator()(int fd, const void *buf, size_t n, off_t off,
		ssize_t *bytes_written) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	if(off < 0)
		return EINVAL;
	struct radix_file_write_results r;
	uint64_t status = radix_file_write(slot, static_cast<uint64_t>(off), buf, n, &r);
	if(status)
		return io_errno(status, true);
	*bytes_written = static_cast<ssize_t>(r.count);
	return 0;
}

int Sysdeps<Readv>::operator()(int fd, const struct iovec *iovs, int iovc, ssize_t *bytes_read) {
	if(iovc < 0 || iovc > IOV_MAX)
		return EINVAL;
	ssize_t total = 0;
	for(int i = 0; i < iovc; i++) {
		if(!iovs[i].iov_len)
			continue;
		ssize_t got;
		if(int e = sysdep<Read>(fd, iovs[i].iov_base, iovs[i].iov_len, &got); e) {
			if(total)
				break;
			return e;
		}
		total += got;
		if(static_cast<size_t>(got) < iovs[i].iov_len)
			break;
	}
	*bytes_read = total;
	return 0;
}

int Sysdeps<Writev>::operator()(int fd, const struct iovec *iovs, int iovc,
		ssize_t *bytes_written) {
	if(iovc < 0 || iovc > IOV_MAX)
		return EINVAL;
	size_t sum = 0;
	for(int i = 0; i < iovc; i++)
		sum += iovs[i].iov_len;
	// Up to PIPE_BUF bytes go in one write, which a pipe never mixes with
	// another's.
	if(iovc > 1 && sum <= PIPE_BUF) {
		char gathered[PIPE_BUF];
		size_t at = 0;
		for(int i = 0; i < iovc; i++) {
			memcpy(gathered + at, iovs[i].iov_base, iovs[i].iov_len);
			at += iovs[i].iov_len;
		}
		return sysdep<Write>(fd, gathered, sum, bytes_written);
	}
	ssize_t total = 0;
	for(int i = 0; i < iovc; i++) {
		if(!iovs[i].iov_len)
			continue;
		ssize_t put;
		if(int e = sysdep<Write>(fd, iovs[i].iov_base, iovs[i].iov_len, &put); e) {
			if(total)
				break;
			return e;
		}
		total += put;
		if(static_cast<size_t>(put) < iovs[i].iov_len)
			break;
	}
	*bytes_written = total;
	return 0;
}

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
	return stat_path(resolved, statbuf);
}

// Not in the dynamic linker, which has no strnlen (core.cpp).
#ifndef MLIBC_BUILDING_RTLD
int Sysdeps<Statvfs>::operator()(const char *path, struct statvfs *out) {
	int fd;
	if(int e = sysdep<Open>(path, O_RDONLY | O_CLOEXEC, 0, &fd); e)
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
	(void)flags;
	// There are no permissions until users come (Phase 13): a file that
	// exists may be read, written and run.
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, pathname, &resolved); e)
		return e;
	struct stat st;
	return stat_path(resolved, &st);
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
	int fd;
	if(int e = sysdep<Open>(path, O_WRONLY | O_CLOEXEC, 0, &fd); e)
		return e;
	int e = sysdep<Ftruncate>(fd, length);
	sysdep<Close>(fd);
	return e;
}

int Sysdeps<Ftruncate>::operator()(int fd, size_t size) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	uint64_t status = radix_file_set_size(slot, size);
	if(other_kind(status))
		return EINVAL;
	return radix::to_errno(status);
}

int Sysdeps<Fallocate>::operator()(int fd, off_t offset, size_t size) {
	struct stat st;
	if(int e = stat_fd(fd, &st); e)
		return e;
	if(offset < 0)
		return EINVAL;
	uint64_t end = static_cast<uint64_t>(offset) + size;
	if(end <= static_cast<uint64_t>(st.st_size))
		return 0;
	return sysdep<Ftruncate>(fd, end);
}

int Sysdeps<Fsync>::operator()(int fd) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	uint64_t status = radix_file_sync(slot);
	if(other_kind(status))
		status = radix_directory_sync(slot);
	// Pipes and terminals have nothing to write back.
	if(is(status, RADIX_KERNEL_MISSING_RIGHTS))
		return 0;
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
	char absolute[radix::PATH_MAX_BYTES + 1];
	absolute[0] = '/';
	if(resolved.len == 1 && resolved.text[0] == '.')
		absolute[1] = 0;
	else
		memcpy(absolute + 1, resolved.text, resolved.len + 1);
	return radix::set_cwd(absolute);
}

int Sysdeps<Fchdir>::operator()(int fd) {
	(void)fd;
	// A descriptor does not know its path (docs/design/posix.md).
	return ENOSYS;
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
	(void)buffer;
	(void)max_size;
	(void)length;
	// No filesystem has symbolic links yet: whatever exists is not one.
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, path, &resolved); e)
		return e;
	struct stat st;
	if(int e = stat_path(resolved, &st); e)
		return e;
	return EINVAL;
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
	(void)target_path;
	(void)dirfd;
	(void)link_path;
	// No filesystem has symbolic links yet.
	return EPERM;
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
	(void)flags;
	// Files have no permissions to change until users come (Phase 13).
	radix::Path resolved;
	if(int e = radix::resolve(fd, pathname, &resolved); e)
		return e;
	struct stat st;
	return stat_path(resolved, &st);
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
	return stat_path(resolved, &st);
}

int Sysdeps<Utimensat>::operator()(int dirfd, const char *pathname, const struct timespec times[2],
		int flags) {
	(void)times;
	(void)flags;
	// The kernel sets a file's times itself, and takes none from programs
	// yet (docs/design/posix.md).
	struct stat st;
	if(!pathname)
		return stat_fd(dirfd, &st);
	radix::Path resolved;
	if(int e = radix::resolve(dirfd, pathname, &resolved); e)
		return e;
	return stat_path(resolved, &st);
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

int Sysdeps<Recvfrom>::operator()(int fd, void *buffer, size_t size, int flags,
		struct sockaddr *sock_addr, socklen_t *addr_length, ssize_t *length) {
	(void)buffer;
	(void)size;
	(void)flags;
	(void)sock_addr;
	(void)addr_length;
	(void)length;
	// Sockets come with networking (Phase 10): no descriptor is one yet.
	struct stat st;
	if(int e = stat_fd(fd, &st); e)
		return e;
	return ENOTSOCK;
}

} // namespace mlibc
