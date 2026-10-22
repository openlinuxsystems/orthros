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
#include <string.h>
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
	radix::set_dir_cursor(*fd, 0);
	if(read_only && !(flags & O_DIRECTORY)) {
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
	if(!is(status, RADIX_KERNEL_WRONG_KIND))
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

// The umask: only kept, as files have no permissions until users come
// (Phase 13).
mode_t file_mask = 022;

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

int Sysdeps<ReadEntries>::operator()(int handle, void *buffer, size_t max_size,
		size_t *bytes_read) {
	uint32_t slot = radix::slot_of(handle);
	if(slot == radix::NO_SLOT)
		return EBADF;
	// A record here is at most 8 bytes longer than the kernel's (16 bytes
	// and the name, 8-byte aligned), so asking for two thirds of the room
	// always fits.
	char listed[1536];
	size_t ask = max_size * 2 / 3;
	if(ask > sizeof(listed))
		ask = sizeof(listed);
	uint64_t cursor = radix::dir_cursor(handle);
	struct radix_directory_list_results r;
	if(uint64_t status = radix_directory_list(slot, cursor, listed, ask, &r); status)
		return radix::to_errno(status);
	auto out = static_cast<char *>(buffer);
	size_t used = 0;
	size_t at = 0;
	for(uint32_t i = 0; i < r.count; i++) {
		struct radix_dir_entry entry;
		memcpy(&entry, listed + at, sizeof(entry));
		const char *name = listed + at + sizeof(entry);
		at += (sizeof(entry) + entry.name_len + 7) & ~size_t(7);
		size_t reclen = (offsetof(struct dirent, d_name) + entry.name_len + 1 + 7) & ~size_t(7);
		if(used + reclen > max_size)
			break;
		struct dirent *d = reinterpret_cast<struct dirent *>(out + used);
		// Listings carry no file numbers; the cursor is unique in the
		// directory, and never 0, which some programs skip.
		d->d_ino = entry.next;
		d->d_off = static_cast<off_t>(entry.next);
		d->d_reclen = static_cast<unsigned short>(reclen);
		switch(entry.kind) {
		case RADIX_FILE_KIND_DIRECTORY: d->d_type = DT_DIR; break;
		case RADIX_FILE_KIND_BLOCK_DEVICE: d->d_type = DT_BLK; break;
		case RADIX_FILE_KIND_CHAR_DEVICE: d->d_type = DT_CHR; break;
		case RADIX_FILE_KIND_PIPE: d->d_type = DT_FIFO; break;
		default: d->d_type = DT_REG; break;
		}
		memcpy(d->d_name, name, entry.name_len);
		d->d_name[entry.name_len] = 0;
		used += reclen;
		cursor = entry.next;
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
		return is(status, RADIX_KERNEL_WRONG_KIND) ? EISDIR : radix::to_errno(status);
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
		return is(status, RADIX_KERNEL_WRONG_KIND) ? EISDIR : radix::to_errno(status);
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
		return is(status, RADIX_KERNEL_WRONG_KIND) ? EISDIR : radix::to_errno(status);
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
		return is(status, RADIX_KERNEL_WRONG_KIND) ? EISDIR : radix::to_errno(status);
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
	if(!is(status, RADIX_KERNEL_WRONG_KIND))
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
	if(from == radix::NO_SLOT || to == radix::NO_SLOT)
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
		if(min_fd < 0 || min_fd >= radix::MAX_FDS)
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
		} else if(!is(status, RADIX_KERNEL_WRONG_KIND)) {
			return radix::to_errno(status);
		} else {
			flags |= O_DIRECTORY;
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
		if(is(status, RADIX_KERNEL_WRONG_KIND))
			return 0;
		return radix::to_errno(status);
	}
	case F_GETLK:
	case F_OFD_GETLK: {
		// No locks are kept yet (docs/design/posix.md): nothing stands in
		// a lock's way.
		auto lock = va_arg(args, struct flock *);
		lock->l_type = F_UNLCK;
		*result = 0;
		return 0;
	}
	case F_SETLK:
	case F_SETLKW:
	case F_OFD_SETLK:
	case F_OFD_SETLKW:
		*result = 0;
		return 0;
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

int Sysdeps<Statvfs>::operator()(const char *path, struct statvfs *out) {
	int fd;
	if(int e = sysdep<Open>(path, O_RDONLY | O_CLOEXEC, 0, &fd); e)
		return e;
	int e = sysdep<Fstatvfs>(fd, out);
	sysdep<Close>(fd);
	return e;
}

int Sysdeps<Fstatvfs>::operator()(int fd, struct statvfs *out) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	struct radix_directory_info_results r;
	uint64_t status = radix_directory_info(slot, &r);
	if(is(status, RADIX_KERNEL_WRONG_KIND))
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
	if(is(status, RADIX_KERNEL_WRONG_KIND))
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
	if(is(status, RADIX_KERNEL_WRONG_KIND))
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
	*old = __atomic_exchange_n(&file_mask, mode & 0777, __ATOMIC_RELAXED);
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
	(void)options;
	// No locks are kept yet (docs/design/posix.md).
	struct stat st;
	return stat_fd(fd, &st);
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
