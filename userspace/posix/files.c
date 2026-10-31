// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Files through the C library: open and its flags, reading, writing and
// seeking, descriptions that share an offset, stdio, directories and
// their descriptors, temporary files, renaming and removing, times,
// symbolic links, `..` beneath directory descriptors and fchdir, reading
// into and writing from memory not made yet, the root's folders by their
// names and by Unix's, descriptors by their names in /devices/fd, and
// owners, groups and modes, extended attributes and access lists, on the
// tmpfs at /tmp and the read-only /system; and what each refuses, with
// Linux's errors.

#include <dirent.h>
#include <fcntl.h>
#include <limits.h>
#include <ctype.h>
#include <stdint.h>
#include <ftw.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/sysmacros.h>
#include <sys/uio.h>
#include <sys/xattr.h>

#include "check.h"

static void basics(void) {
	int fd = open("/tmp/files-a", O_RDWR | O_CREAT | O_EXCL, 0644);
	CHECK(fd >= 0);
	CHECK_ERROR(open("/tmp/files-a", O_RDWR | O_CREAT | O_EXCL, 0644), EEXIST);
	CHECK(write(fd, "hello world", 11) == 11);
	CHECK(lseek(fd, 0, SEEK_CUR) == 11);
	CHECK(lseek(fd, 6, SEEK_SET) == 6);
	char buffer[32] = {0};
	CHECK(read(fd, buffer, sizeof(buffer)) == 5 && !strcmp(buffer, "world"));
	CHECK(read(fd, buffer, sizeof(buffer)) == 0);
	CHECK(lseek(fd, -5, SEEK_END) == 6);
	CHECK_ERROR(lseek(fd, -100, SEEK_SET), EINVAL);
	CHECK(pwrite(fd, "W", 1, 6) == 1 && lseek(fd, 0, SEEK_CUR) == 6);
	memset(buffer, 0, sizeof(buffer));
	CHECK(pread(fd, buffer, 11, 0) == 11 && !strcmp(buffer, "hello World"));
	// Scattered and gathered.
	struct iovec parts[2] = {{"ab", 2}, {"cd", 2}};
	CHECK(lseek(fd, 0, SEEK_SET) == 0 && writev(fd, parts, 2) == 4);
	char one[3] = {0}, two[3] = {0};
	struct iovec into[2] = {{one, 2}, {two, 2}};
	CHECK(lseek(fd, 0, SEEK_SET) == 0 && readv(fd, into, 2) == 4);
	CHECK(!strcmp(one, "ab") && !strcmp(two, "cd"));
	// At an offset, which they leave as it was; a read ends at the end.
	struct iovec more[2] = {{"xy", 2}, {"z", 1}};
	CHECK(pwritev(fd, more, 2, 7) == 3 && lseek(fd, 0, SEEK_CUR) == 4);
	memset(one, 0, sizeof(one));
	memset(two, 0, sizeof(two));
	CHECK(preadv(fd, into, 2, 8) == 3 && lseek(fd, 0, SEEK_CUR) == 4);
	CHECK(!strcmp(one, "yz") && !strcmp(two, "d"));
	CHECK_ERROR(preadv(fd, into, -1, 0), EINVAL);
	CHECK_ERROR(pwritev(fd, more, 2, -1), EINVAL);
	// Sizes.
	struct stat st;
	CHECK(fstat(fd, &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 11);
	CHECK(ftruncate(fd, 4) == 0 && fstat(fd, &st) == 0 && st.st_size == 4);
	CHECK(ftruncate(fd, 10000) == 0 && pread(fd, buffer, 4, 9000) == 4 && !buffer[0]);
	CHECK(stat("/tmp/files-a", &st) == 0 && st.st_size == 10000);
	CHECK(fsync(fd) == 0);
	close(fd);
	// Appending: every write goes at the end.
	fd = open("/tmp/files-a", O_WRONLY | O_TRUNC | O_APPEND);
	CHECK(fd >= 0 && write(fd, "1", 1) == 1);
	CHECK(lseek(fd, 0, SEEK_SET) == 0 && write(fd, "2", 1) == 1);
	CHECK(fstat(fd, &st) == 0 && st.st_size == 2);
	CHECK(fcntl(fd, F_GETFL) & O_APPEND);
	CHECK_ERROR(read(fd, buffer, 1), EBADF);
	close(fd);
	CHECK_ERROR(open("/tmp/nothing", O_RDONLY), ENOENT);
	CHECK_ERROR(open("/tmp/files-a/x", O_RDONLY), ENOTDIR);
	CHECK_ERROR(open("/tmp", O_WRONLY), EISDIR);
	CHECK(unlink("/tmp/files-a") == 0);
	CHECK_ERROR(unlink("/tmp/files-a"), ENOENT);
}

// dup's copies share a description, and its offset; a second open does
// not.
static void descriptions(void) {
	int fd = open("/tmp/files-b", O_RDWR | O_CREAT | O_TRUNC, 0644);
	CHECK(write(fd, "0123456789", 10) == 10);
	int copy = dup(fd);
	CHECK(lseek(fd, 3, SEEK_SET) == 3 && lseek(copy, 0, SEEK_CUR) == 3);
	int other = open("/tmp/files-b", O_RDONLY);
	CHECK(lseek(other, 0, SEEK_CUR) == 0);
	char byte;
	CHECK(read(copy, &byte, 1) == 1 && byte == '3' && lseek(fd, 0, SEEK_CUR) == 4);
	CHECK(read(other, &byte, 1) == 1 && byte == '0');
	close(fd);
	close(copy);
	close(other);
	unlink("/tmp/files-b");
}

// A descriptor saved and put back, as a shell does around each
// redirection, far more often than capabilities may be derived from one
// another: it still copies, and a child still gets it.
static void saved_and_restored(void) {
	int fd = open("/devices/null", O_RDONLY);
	CHECK(fd >= 0);
	for(int i = 0; i < 1000; i++) {
		int saved = fcntl(fd, F_DUPFD_CLOEXEC, 10);
		CHECK(saved >= 10 && dup2(saved, fd) == fd && close(saved) == 0);
	}
	int copy = dup(fd);
	CHECK(copy >= 0);
	pid_t pid = check_fork();
	if(pid == 0)
		_exit(fcntl(fd, F_GETFD) < 0);
	CHECK_CHILD(pid);
	close(copy);
	close(fd);
}

static void standard_io(void) {
	FILE *file = fopen("/tmp/files-c", "w+");
	CHECK(file != NULL);
	if(!file)
		return;
	for(int i = 0; i < 1000; i++)
		fprintf(file, "line %d\n", i);
	CHECK(fflush(file) == 0);
	rewind(file);
	char line[64];
	int count = 0, last = -1;
	while(fgets(line, sizeof(line), file)) {
		count++;
		sscanf(line, "line %d", &last);
	}
	CHECK(count == 1000 && last == 999 && feof(file));
	CHECK(fseek(file, 0, SEEK_END) == 0 && ftell(file) == 8890);
	CHECK(fclose(file) == 0);
	CHECK(remove("/tmp/files-c") == 0);
}

static void directories(void) {
	CHECK(mkdir("/tmp/files-dir", 0755) == 0);
	CHECK_ERROR(mkdir("/tmp/files-dir", 0755), EEXIST);
	for(int i = 0; i < 20; i++) {
		char path[64];
		snprintf(path, sizeof(path), "/tmp/files-dir/f%d", i);
		int fd = open(path, O_WRONLY | O_CREAT, 0644);
		CHECK(fd >= 0);
		close(fd);
	}
	DIR *dir = opendir("/tmp/files-dir");
	CHECK(dir != NULL);
	int files = 0, dots = 0;
	struct dirent *entry;
	while(dir && (entry = readdir(dir))) {
		if(!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, ".."))
			dots++;
		else if(entry->d_name[0] == 'f')
			files++;
	}
	if(dir)
		closedir(dir);
	CHECK(files == 20 && dots == 2);
	CHECK_ERROR(rmdir("/tmp/files-dir"), ENOTEMPTY);
	CHECK(rename("/tmp/files-dir/f0", "/tmp/files-dir/g0") == 0);
	struct stat st;
	CHECK(stat("/tmp/files-dir/g0", &st) == 0 && S_ISREG(st.st_mode));
	CHECK_ERROR(stat("/tmp/files-dir/f0", &st), ENOENT);
	for(int i = 0; i < 20; i++) {
		char path[64];
		snprintf(path, sizeof(path), "/tmp/files-dir/%c%d", i ? 'f' : 'g', i);
		CHECK(unlink(path) == 0);
	}
	// The working directory, and paths relative to it.
	char cwd[PATH_MAX];
	CHECK(getcwd(cwd, sizeof(cwd)) != NULL);
	CHECK(chdir("/tmp/files-dir") == 0);
	CHECK(getcwd(cwd, sizeof(cwd)) && !strcmp(cwd, "/tmp/files-dir"));
	int fd = open("relative", O_WRONLY | O_CREAT, 0644);
	CHECK(fd >= 0);
	close(fd);
	CHECK(stat("/tmp/files-dir/relative", &st) == 0);
	CHECK(chdir("..") == 0 && getcwd(cwd, sizeof(cwd)) && !strcmp(cwd, "/tmp"));
	CHECK(unlink("files-dir/relative") == 0);
	CHECK(rmdir("files-dir") == 0);
	CHECK(chdir("/") == 0);
	CHECK_ERROR(chdir("/nowhere"), ENOENT);
	// /system is read-only.
	CHECK(stat("/system/tests/posix/files", &st) == 0 && S_ISREG(st.st_mode));
	CHECK(stat("/system", &st) == 0 && S_ISDIR(st.st_mode));
	CHECK_ERROR(open("/system/new", O_WRONLY | O_CREAT, 0644), EROFS);
	CHECK_ERROR(utimensat(AT_FDCWD, "/system", NULL, 0), EROFS);
	// Each mount has a device number of its own.
	struct stat tmp;
	CHECK(stat("/tmp", &tmp) == 0 && tmp.st_dev != st.st_dev);
	CHECK(access("/system/tests/posix/files", R_OK) == 0);
	CHECK_ERROR(access("/nowhere", F_OK), ENOENT);
}

// A file's times, as utimensat and futimens set them; reading leaves the
// read time.
static void times(void) {
	int fd = open("/tmp/files-times", O_RDWR | O_CREAT | O_EXCL, 0644);
	CHECK(fd >= 0);
	struct timespec set[2] = {{.tv_sec = 1000000000, .tv_nsec = 5}, {.tv_sec = 2, .tv_nsec = 7}};
	CHECK(utimensat(AT_FDCWD, "/tmp/files-times", set, 0) == 0);
	struct stat st;
	CHECK(fstat(fd, &st) == 0);
	CHECK(st.st_atim.tv_sec == 1000000000 && st.st_atim.tv_nsec == 5);
	CHECK(st.st_mtim.tv_sec == 2 && st.st_mtim.tv_nsec == 7);
	char byte;
	CHECK(read(fd, &byte, 1) == 0);
	CHECK(fstat(fd, &st) == 0 && st.st_atim.tv_sec == 1000000000);
	// UTIME_NOW and UTIME_OMIT; a write changes the change time.
	time_t now = time(NULL);
	struct timespec omit_now[2] = {{.tv_nsec = UTIME_OMIT}, {.tv_nsec = UTIME_NOW}};
	CHECK(futimens(fd, omit_now) == 0);
	CHECK(fstat(fd, &st) == 0 && st.st_atim.tv_sec == 1000000000 && st.st_mtim.tv_sec >= now);
	CHECK(futimens(fd, set) == 0 && write(fd, "x", 1) == 1);
	CHECK(fstat(fd, &st) == 0 && st.st_mtim.tv_sec >= now && st.st_atim.tv_sec == 1000000000);
	struct timespec bad[2] = {{.tv_nsec = 1000000000}, {.tv_nsec = UTIME_OMIT}};
	CHECK_ERROR(futimens(fd, bad), EINVAL);
	struct timespec early[2] = {{.tv_sec = -1}, {.tv_nsec = UTIME_OMIT}};
	CHECK_ERROR(futimens(fd, early), EINVAL);
	close(fd);
	// A descriptor not open for writing may not set them.
	fd = open("/tmp/files-times", O_RDONLY);
	CHECK(fd >= 0);
	CHECK_ERROR(futimens(fd, NULL), EACCES);
	close(fd);
	// Directories have times too, and devices none.
	CHECK(utimensat(AT_FDCWD, "/tmp", set, 0) == 0);
	CHECK(stat("/tmp", &st) == 0 && st.st_mtim.tv_sec == 2);
	CHECK_ERROR(utimensat(AT_FDCWD, "/devices/null", NULL, 0), EPERM);
	CHECK(unlink("/tmp/files-times") == 0);
}

// A descriptor of a directory, opened with or without O_DIRECTORY, as
// the calls that take one see it: nftw and fdopendir open directories
// so.
static void directory_descriptors(void) {
	CHECK(mkdir("/tmp/files-dd", 0755) == 0);
	CHECK(mkdir("/tmp/files-dd/sub", 0755) == 0);
	int made = open("/tmp/files-dd/sub/f", O_WRONLY | O_CREAT, 0644);
	CHECK(made >= 0);
	close(made);
	for(int flags = O_RDONLY; flags <= (O_RDONLY | O_DIRECTORY); flags += O_DIRECTORY) {
		int fd = open("/tmp/files-dd", flags);
		CHECK(fd >= 0);
		struct stat st;
		CHECK(fstat(fd, &st) == 0 && S_ISDIR(st.st_mode));
		char byte;
		CHECK_ERROR(read(fd, &byte, 1), EISDIR);
		CHECK_ERROR(write(fd, "x", 1), EBADF);
		CHECK((fcntl(fd, F_GETFL) & O_ACCMODE) == O_RDONLY);
		CHECK(fcntl(fd, F_SETFL, O_NONBLOCK) == 0);
		CHECK(fsync(fd) == 0);
		CHECK_ERROR(ftruncate(fd, 0), EINVAL);
		struct statvfs vfs;
		CHECK(fstatvfs(fd, &vfs) == 0 && vfs.f_bsize > 0);
		CHECK(fstatat(fd, "sub", &st, 0) == 0 && S_ISDIR(st.st_mode));
		DIR *dir = fdopendir(fd);
		CHECK(dir != NULL);
		int entries = 0;
		while(dir && readdir(dir))
			entries++;
		CHECK(entries == 3);
		// Again from the start.
		if(dir)
			rewinddir(dir);
		while(dir && readdir(dir))
			entries--;
		CHECK(entries == 0);
		if(dir)
			closedir(dir);
		else
			close(fd);
	}
	// statvfs of a file tells of its filesystem.
	struct statvfs vfs;
	CHECK(statvfs("/tmp/files-dd/sub/f", &vfs) == 0 && vfs.f_bsize > 0);
	CHECK(unlink("/tmp/files-dd/sub/f") == 0);
	CHECK(rmdir("/tmp/files-dd/sub") == 0 && rmdir("/tmp/files-dd") == 0);
}

static int visited;

static int count_entry(const char *path, const struct stat *st, int type, struct FTW *ftw) {
	(void)path;
	(void)st;
	(void)ftw;
	visited += type == FTW_F ? 1 : type == FTW_D ? 100 : 10000;
	return 0;
}

static int remove_entry(const char *path, const struct stat *st, int type, struct FTW *ftw) {
	(void)st;
	(void)type;
	(void)ftw;
	return remove(path);
}

// Whether the 6 characters at `x` are letters and digits.
static int letters_and_digits(const char *x) {
	for(int i = 0; i < 6; i++)
		if(!isalnum((unsigned char)x[i]))
			return 0;
	return 1;
}

// Temporary files and directories get names no one can guess: two in a
// row differ, and neither is 000000.
static void temporary_files(void) {
	char first[] = "/tmp/files-XXXXXX";
	char second[] = "/tmp/files-XXXXXX";
	int a = mkstemp(first);
	int b = mkstemp(second);
	CHECK(a >= 0 && b >= 0 && strcmp(first, second));
	CHECK(letters_and_digits(first + 11) && strcmp(first + 11, "000000"));
	close(a);
	close(b);
	CHECK(unlink(first) == 0 && unlink(second) == 0);
	char suffixed[] = "/tmp/files-XXXXXX.txt";
	a = mkstemps(suffixed, 4);
	CHECK(a >= 0 && !strcmp(suffixed + 17, ".txt") && letters_and_digits(suffixed + 11));
	close(a);
	CHECK(unlink(suffixed) == 0);
	char bad[] = "/tmp/files-XXXXX";
	CHECK_ERROR(mkstemp(bad), EINVAL);
	char unused[] = "/tmp/files-XXXXXX";
	CHECK(mktemp(unused) == unused && letters_and_digits(unused + 11));
	struct stat st;
	CHECK_ERROR(stat(unused, &st), ENOENT);
	char dir[] = "/tmp/files-XXXXXX";
	CHECK(mkdtemp(dir) == dir && letters_and_digits(dir + 11));
	// A tree to walk: the directory, a file in it, and a directory with a
	// file in it.
	char path[64];
	snprintf(path, sizeof(path), "%s/file", dir);
	a = creat(path, 0644);
	CHECK(a >= 0);
	close(a);
	snprintf(path, sizeof(path), "%s/sub", dir);
	CHECK(mkdir(path, 0755) == 0);
	snprintf(path, sizeof(path), "%s/sub/file", dir);
	a = creat(path, 0644);
	CHECK(a >= 0);
	close(a);
	visited = 0;
	CHECK(nftw(dir, count_entry, 8, FTW_PHYS) == 0 && visited == 202);
	CHECK(nftw(dir, remove_entry, 8, FTW_DEPTH | FTW_PHYS) == 0);
	CHECK_ERROR(stat(dir, &st), ENOENT);
	FILE *file = tmpfile();
	CHECK(file != NULL);
	if(file) {
		CHECK(fputs("scratch", file) >= 0 && fseek(file, 0, SEEK_SET) == 0);
		char text[8] = {0};
		CHECK(fgets(text, sizeof(text), file) && !strcmp(text, "scratch"));
		fclose(file);
	}
}

// Symbolic links (ADR 0081): made and read, followed by the calls that
// take a path but not by those that work on a link itself, `..` after
// one taken from where it led, links that lead nowhere or round in a
// loop, and the links into /system that Spark makes.
static void links(void) {
	CHECK(mkdir("/tmp/links", 0755) == 0);
	CHECK(mkdir("/tmp/links/a", 0755) == 0);
	CHECK(mkdir("/tmp/links/a/b", 0755) == 0);
	int fd = open("/tmp/links/a/b/file", O_WRONLY | O_CREAT, 0644);
	CHECK(fd >= 0 && write(fd, "linked", 6) == 6);
	close(fd);
	// A relative target goes on from the link's directory.
	CHECK(symlink("a/b", "/tmp/links/rel") == 0);
	CHECK_ERROR(symlink("x", "/tmp/links/rel"), EEXIST);
	struct stat st;
	CHECK(lstat("/tmp/links/rel", &st) == 0 && S_ISLNK(st.st_mode) && st.st_size == 3);
	CHECK(stat("/tmp/links/rel", &st) == 0 && S_ISDIR(st.st_mode));
	char target[16];
	CHECK(readlink("/tmp/links/rel", target, sizeof(target)) == 3 && !memcmp(target, "a/b", 3));
	// readlink cuts the target short to the buffer, with no NUL added.
	CHECK(readlink("/tmp/links/rel", target, 2) == 2 && !memcmp(target, "a/", 2));
	CHECK_ERROR(readlink("/tmp/links/a", target, sizeof(target)), EINVAL);
	CHECK_ERROR(readlink("/tmp/links/none", target, sizeof(target)), ENOENT);
	char text[8] = {0};
	fd = open("/tmp/links/rel/file", O_RDONLY);
	CHECK(fd >= 0 && read(fd, text, sizeof(text)) == 6 && !strcmp(text, "linked"));
	close(fd);
	// An absolute target; `..` after a link goes up from where it led,
	// not back along the path's text.
	CHECK(symlink("/tmp/links/a/b", "/tmp/links/abs") == 0);
	CHECK(stat("/tmp/links/abs/file", &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 6);
	CHECK(stat("/tmp/links/abs/../b/file", &st) == 0 && S_ISREG(st.st_mode));
	CHECK_ERROR(stat("/tmp/links/abs/../rel", &st), ENOENT);
	// realpath and the working directory name where links lead.
	char real[PATH_MAX];
	CHECK(realpath("/tmp/links/rel/../b/./file", real) && !strcmp(real, "/tmp/links/a/b/file"));
	CHECK(realpath("/tmp/links/abs/", real) && !strcmp(real, "/tmp/links/a/b"));
	errno = 0;
	CHECK(realpath("/tmp/links/rel/none", real) == NULL && errno == ENOENT);
	CHECK(chdir("/tmp/links/abs") == 0);
	CHECK(getcwd(real, sizeof(real)) && !strcmp(real, "/tmp/links/a/b"));
	CHECK(chdir("..") == 0 && getcwd(real, sizeof(real)) && !strcmp(real, "/tmp/links/a"));
	CHECK(readlink("../rel", target, sizeof(target)) == 3);
	CHECK(chdir("/") == 0);
	// The last name of a path is followed unless the call says not to,
	// or a slash after it says to all the same.
	CHECK_ERROR(open("/tmp/links/rel", O_RDONLY | O_NOFOLLOW), ELOOP);
	CHECK(fstatat(AT_FDCWD, "/tmp/links/rel", &st, AT_SYMLINK_NOFOLLOW) == 0 && S_ISLNK(st.st_mode));
	CHECK(lstat("/tmp/links/rel/", &st) == 0 && S_ISDIR(st.st_mode));
	struct timespec set[2] = {{.tv_sec = 5}, {.tv_sec = 6}};
	CHECK(utimensat(AT_FDCWD, "/tmp/links/rel", set, AT_SYMLINK_NOFOLLOW) == 0);
	CHECK(lstat("/tmp/links/rel", &st) == 0 && st.st_mtim.tv_sec == 6);
	CHECK(stat("/tmp/links/rel", &st) == 0 && st.st_mtim.tv_sec != 6);
	// A directory lists a link as one.
	DIR *dir = opendir("/tmp/links");
	CHECK(dir != NULL);
	int listed = 0;
	struct dirent *entry;
	while(dir && (entry = readdir(dir)))
		listed += entry->d_type == DT_LNK;
	if(dir)
		closedir(dir);
	CHECK(listed == 2);
	// A link that leads nowhere: O_CREAT through it makes its target,
	// beneath the root for an absolute one, and O_EXCL refuses it.
	CHECK(symlink("made", "/tmp/links/dangling") == 0);
	CHECK_ERROR(stat("/tmp/links/dangling", &st), ENOENT);
	CHECK(lstat("/tmp/links/dangling", &st) == 0 && S_ISLNK(st.st_mode));
	CHECK_ERROR(open("/tmp/links/dangling", O_WRONLY | O_CREAT | O_EXCL, 0644), EEXIST);
	fd = open("/tmp/links/dangling", O_WRONLY | O_CREAT, 0644);
	CHECK(fd >= 0);
	close(fd);
	CHECK(stat("/tmp/links/made", &st) == 0 && S_ISREG(st.st_mode));
	CHECK(symlink("/tmp/links/a/made", "/tmp/links/a/dangling") == 0);
	fd = open("/tmp/links/a/dangling", O_WRONLY | O_CREAT, 0644);
	CHECK(fd >= 0);
	close(fd);
	CHECK(stat("/tmp/links/a/made", &st) == 0 && S_ISREG(st.st_mode));
	// Links round in a loop.
	CHECK(symlink("loop2", "/tmp/links/loop1") == 0 && symlink("loop1", "/tmp/links/loop2") == 0);
	CHECK_ERROR(stat("/tmp/links/loop1", &st), ELOOP);
	CHECK_ERROR(open("/tmp/links/loop1", O_RDONLY), ELOOP);
	CHECK_ERROR(open("/tmp/links/loop1", O_WRONLY | O_CREAT, 0644), ELOOP);
	// nftw reports links as links with FTW_PHYS: three directories, three
	// files and six links.
	visited = 0;
	CHECK(nftw("/tmp/links", count_entry, 8, FTW_PHYS) == 0 && visited == 60303);
	// Renaming and removing work on a link, never on its target.
	CHECK(rename("/tmp/links/rel", "/tmp/links/moved") == 0);
	CHECK(lstat("/tmp/links/moved", &st) == 0 && S_ISLNK(st.st_mode));
	CHECK(stat("/tmp/links/a/b", &st) == 0 && S_ISDIR(st.st_mode));
	CHECK(unlink("/tmp/links/moved") == 0 && unlink("/tmp/links/abs") == 0);
	CHECK(stat("/tmp/links/a/b/file", &st) == 0);
	// A hard link to a symbolic link is to the link itself, unless
	// followed; nothing new goes in /system.
	CHECK(symlink("a/b/file", "/tmp/links/to-file") == 0);
	CHECK(link("/tmp/links/to-file", "/tmp/links/hard") == 0);
	CHECK(lstat("/tmp/links/hard", &st) == 0 && S_ISLNK(st.st_mode) && st.st_nlink == 2);
	CHECK(linkat(AT_FDCWD, "/tmp/links/to-file", AT_FDCWD, "/tmp/links/followed",
			AT_SYMLINK_FOLLOW) == 0);
	CHECK(lstat("/tmp/links/followed", &st) == 0 && S_ISREG(st.st_mode) && st.st_nlink == 2);
	CHECK_ERROR(linkat(AT_FDCWD, "/tmp/links/to-file", AT_FDCWD, "/tmp/links/x", 0x10000), EINVAL);
	CHECK_ERROR(symlink("x", "/system/new"), EROFS);
	CHECK_ERROR(link("/tmp/links/a/b/file", "/system/new"), EROFS);
	CHECK(nftw("/tmp/links", remove_entry, 8, FTW_DEPTH | FTW_PHYS) == 0);
	CHECK_ERROR(lstat("/tmp/links", &st), ENOENT);
	// Spark's links to /system, for Unix programs and scripts.
	CHECK(lstat("/bin", &st) == 0 && S_ISLNK(st.st_mode));
	CHECK(readlink("/usr", target, sizeof(target)) == 6 && !memcmp(target, "system", 6));
	CHECK(realpath("/usr/share/../bin", real) && !strcmp(real, "/system/commands"));
	CHECK(stat("/etc", &st) == 0 && S_ISDIR(st.st_mode));
	CHECK(stat("/var", &st) == 0 && S_ISDIR(st.st_mode));
	CHECK(realpath("/sbin/.", real) && !strcmp(real, "/system/commands"));
}

// A file's second name, through which it reads and changes, and which
// it outlives; the change time, which a name and times change and
// reading does not.
static void hard_links(void) {
	CHECK(mkdir("/tmp/hard", 0755) == 0 && mkdir("/tmp/hard/sub", 0755) == 0);
	int fd = open("/tmp/hard/a", O_RDWR | O_CREAT | O_EXCL, 0644);
	CHECK(fd >= 0 && write(fd, "named", 5) == 5);
	struct stat st, other;
	CHECK(fstat(fd, &st) == 0 && st.st_nlink == 1);
	// A directory says 1, as on Linux's Btrfs.
	CHECK(stat("/tmp/hard/sub", &other) == 0 && other.st_nlink == 1);
	struct timespec old[2] = {{.tv_sec = 5}, {.tv_sec = 5}};
	CHECK(futimens(fd, old) == 0 && fstat(fd, &st) == 0);
	time_t before = st.st_ctim.tv_sec;
	CHECK(st.st_mtim.tv_sec == 5 && before >= time(NULL) - 60);
	CHECK(link("/tmp/hard/a", "/tmp/hard/sub/b") == 0);
	CHECK(stat("/tmp/hard/sub/b", &other) == 0 && other.st_ino == st.st_ino);
	CHECK(other.st_dev == st.st_dev && other.st_nlink == 2 && other.st_ctim.tv_sec >= before);
	CHECK(fstat(fd, &st) == 0 && st.st_nlink == 2);
	CHECK_ERROR(link("/tmp/hard/a", "/tmp/hard/sub/b"), EEXIST);
	CHECK_ERROR(link("/tmp/hard/sub", "/tmp/hard/dir"), EPERM);
	CHECK_ERROR(link("/tmp/hard/none", "/tmp/hard/c"), ENOENT);
	CHECK_ERROR(link("/tmp/hard/a", "/tmp/hard/none/c"), ENOENT);
	// Written through one name, read through the other.
	int second = open("/tmp/hard/sub/b", O_WRONLY | O_APPEND);
	CHECK(second >= 0 && write(second, "!", 1) == 1);
	close(second);
	char text[8] = {0};
	CHECK(pread(fd, text, sizeof(text), 0) == 6 && !strcmp(text, "named!"));
	// Reading leaves the change time; renaming a name onto another of
	// the same file does nothing, as POSIX has it.
	CHECK(fstat(fd, &st) == 0);
	CHECK(rename("/tmp/hard/a", "/tmp/hard/sub/b") == 0);
	CHECK(stat("/tmp/hard/a", &other) == 0 && other.st_nlink == 2);
	CHECK(other.st_ctim.tv_sec == st.st_ctim.tv_sec && other.st_ctim.tv_nsec == st.st_ctim.tv_nsec);
	// The file outlives its first name, and then its second.
	CHECK(unlink("/tmp/hard/a") == 0);
	CHECK(stat("/tmp/hard/sub/b", &other) == 0 && other.st_nlink == 1);
	CHECK(other.st_ctim.tv_sec >= st.st_ctim.tv_sec);
	CHECK(unlink("/tmp/hard/sub/b") == 0);
	CHECK(fstat(fd, &st) == 0 && st.st_nlink == 0);
	CHECK(pread(fd, text, sizeof(text), 0) == 6);
	close(fd);
	CHECK(rmdir("/tmp/hard/sub") == 0 && rmdir("/tmp/hard") == 0);
}

// Named pipes: an open to read waits for a writer and one to write for a
// reader, but not with O_NONBLOCK or both ways; the ends share one pipe
// while any is open. Device nodes open the device of Linux's number for
// them, and a socket's name opens nothing.
static void special_files(void) {
	CHECK(mkdir("/tmp/special", 0755) == 0);
	CHECK(mkfifo("/tmp/special/fifo", 0644) == 0);
	CHECK_ERROR(mkfifo("/tmp/special/fifo", 0644), EEXIST);
	struct stat st;
	CHECK(stat("/tmp/special/fifo", &st) == 0 && S_ISFIFO(st.st_mode) && st.st_size == 0);
	// No reader: a writer that will not wait is refused.
	CHECK_ERROR(open("/tmp/special/fifo", O_WRONLY | O_NONBLOCK), ENXIO);
	int reader = open("/tmp/special/fifo", O_RDONLY | O_NONBLOCK);
	CHECK(reader >= 0);
	char byte;
	CHECK(read(reader, &byte, 1) == 0);
	int writer = open("/tmp/special/fifo", O_WRONLY | O_NONBLOCK);
	CHECK(writer >= 0 && write(writer, "p", 1) == 1);
	CHECK(read(reader, &byte, 1) == 1 && byte == 'p');
	close(writer);
	close(reader);
	// A reader and a writer that wait for each other.
	pid_t pid = fork();
	CHECK(pid >= 0);
	if(pid == 0) {
		int out = open("/tmp/special/fifo", O_WRONLY);
		_exit(out >= 0 && write(out, "hi", 2) == 2 ? 0 : 1);
	}
	reader = open("/tmp/special/fifo", O_RDONLY);
	char text[4] = {0};
	CHECK(reader >= 0 && read(reader, text, sizeof(text)) == 2 && !strcmp(text, "hi"));
	CHECK(read(reader, text, 1) == 0);
	close(reader);
	CHECK_CHILD(pid);
	// Both ways at once waits for nothing.
	int both = open("/tmp/special/fifo", O_RDWR);
	CHECK(both >= 0 && write(both, "x", 1) == 1 && read(both, &byte, 1) == 1 && byte == 'x');
	close(both);
	CHECK(mknod("/tmp/special/fifo2", S_IFIFO | 0600, 0) == 0);
	CHECK(lstat("/tmp/special/fifo2", &st) == 0 && S_ISFIFO(st.st_mode));
	// Device nodes, by Linux's numbers.
	CHECK(mknod("/tmp/special/null", S_IFCHR | 0666, makedev(1, 3)) == 0);
	CHECK(mknod("/tmp/special/zero", S_IFCHR | 0666, makedev(1, 5)) == 0);
	CHECK(mknod("/tmp/special/other", S_IFCHR | 0666, makedev(250, 70000)) == 0);
	CHECK(mknod("/tmp/special/disk", S_IFBLK | 0600, makedev(259, 0)) == 0);
	CHECK(stat("/tmp/special/null", &st) == 0 && S_ISCHR(st.st_mode));
	CHECK(major(st.st_rdev) == 1 && minor(st.st_rdev) == 3);
	CHECK(stat("/tmp/special/other", &st) == 0 && major(st.st_rdev) == 250);
	CHECK(minor(st.st_rdev) == 70000);
	CHECK(stat("/tmp/special/disk", &st) == 0 && S_ISBLK(st.st_mode));
	int fd = open("/tmp/special/null", O_RDWR);
	CHECK(fd >= 0 && write(fd, "gone", 4) == 4 && read(fd, text, sizeof(text)) == 0);
	close(fd);
	fd = open("/tmp/special/zero", O_RDONLY);
	memset(text, 'x', sizeof(text));
	CHECK(fd >= 0 && read(fd, text, sizeof(text)) == 4 && !memcmp(text, "\0\0\0\0", 4));
	close(fd);
	CHECK_ERROR(open("/tmp/special/other", O_RDONLY), ENXIO);
	CHECK_ERROR(open("/tmp/special/disk", O_RDONLY), ENXIO);
	CHECK(mknod("/tmp/special/file", S_IFREG | 0644, 0) == 0);
	CHECK(stat("/tmp/special/file", &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 0);
	CHECK(mknod("/tmp/special/socket", S_IFSOCK | 0755, 0) == 0);
	CHECK(lstat("/tmp/special/socket", &st) == 0 && S_ISSOCK(st.st_mode));
	CHECK_ERROR(open("/tmp/special/socket", O_RDONLY), ENXIO);
	CHECK_ERROR(mknod("/tmp/special/dir", S_IFDIR | 0755, 0), EPERM);
	CHECK_ERROR(mknod("/tmp/special/bad", 0170000 | 0644, 0), EINVAL);
	CHECK_ERROR(mkfifo("/system/fifo", 0644), EROFS);
	CHECK(nftw("/tmp/special", remove_entry, 8, FTW_DEPTH | FTW_PHYS) == 0);
}

// Whether descriptor `fd` is the directory `path`.
static int same_dir(int fd, const char *path) {
	struct stat st, want;
	return fstat(fd, &st) == 0 && stat(path, &want) == 0 && st.st_ino == want.st_ino
			&& st.st_dev == want.st_dev;
}

// Whether the working directory is `path`.
static int in_dir(const char *path) {
	char cwd[PATH_MAX];
	return getcwd(cwd, sizeof(cwd)) && !strcmp(cwd, path);
}

// `..` beneath a directory descriptor, and fchdir, which go by the path
// the C library notes for each one (ADR 0089): up from descriptors
// opened beneath others, with O_PATH as tree walks open them, and from a
// link, in a walk up and down that would outgrow a path if each `..`
// were kept, and from a copy.
static void climbing(void) {
	CHECK(mkdir("/tmp/files-up", 0755) == 0);
	CHECK(mkdir("/tmp/files-up/a", 0755) == 0);
	CHECK(mkdir("/tmp/files-up/a/b", 0755) == 0);
	CHECK(mkdir("/tmp/files-up/c", 0755) == 0);
	CHECK(symlink("a/b", "/tmp/files-up/l") == 0);
	int made = open("/tmp/files-up/a/b/f", O_WRONLY | O_CREAT, 0644);
	CHECK(made >= 0);
	close(made);

	int top = open("/tmp/files-up", O_RDONLY | O_DIRECTORY);
	CHECK(top >= 0);
	int a = openat(top, "a", O_PATH | O_DIRECTORY);
	CHECK(a >= 0 && same_dir(a, "/tmp/files-up/a"));
	int b = openat(a, "b", O_RDONLY | O_DIRECTORY);
	CHECK(b >= 0 && same_dir(b, "/tmp/files-up/a/b"));
	int c = openat(b, "../../c", O_RDONLY | O_DIRECTORY);
	CHECK(c >= 0 && same_dir(c, "/tmp/files-up/c"));
	struct stat st;
	CHECK(fstatat(c, "../a/b/f", &st, 0) == 0 && S_ISREG(st.st_mode));
	// `..` from a link is the parent of where it leads.
	int l = openat(top, "l", O_RDONLY | O_DIRECTORY);
	CHECK(l >= 0);
	int up = openat(l, "..", O_RDONLY | O_DIRECTORY);
	CHECK(up >= 0 && same_dir(up, "/tmp/files-up/a"));
	// 2,000 steps up and down, from a copy.
	int walk = dup(b);
	CHECK(walk >= 0);
	for(int i = 0; walk >= 0 && i < 2000; i++) {
		int next = openat(walk, i % 2 ? "b" : "..", O_RDONLY | O_DIRECTORY);
		close(walk);
		walk = next;
	}
	CHECK(walk >= 0 && same_dir(walk, "/tmp/files-up/a/b"));

	char saved[PATH_MAX];
	CHECK(getcwd(saved, sizeof(saved)) != NULL);
	CHECK(fchdir(walk) == 0 && in_dir("/tmp/files-up/a/b"));
	CHECK(fchdir(l) == 0 && in_dir("/tmp/files-up/a/b"));
	CHECK(fchdir(a) == 0 && in_dir("/tmp/files-up/a"));
	CHECK(chdir("..") == 0 && in_dir("/tmp/files-up"));
	int file = openat(b, "f", O_RDONLY);
	CHECK(file >= 0);
	CHECK_ERROR(fchdir(file), ENOTDIR);
	CHECK_ERROR(fchdir(-1), EBADF);
	CHECK(chdir(saved) == 0);

	int fds[] = {top, a, b, c, l, up, walk, file};
	for(size_t i = 0; i < sizeof(fds) / sizeof(fds[0]); i++)
		close(fds[i]);
	CHECK(unlink("/tmp/files-up/a/b/f") == 0 && unlink("/tmp/files-up/l") == 0);
	CHECK(rmdir("/tmp/files-up/a/b") == 0 && rmdir("/tmp/files-up/a") == 0);
	CHECK(rmdir("/tmp/files-up/c") == 0 && rmdir("/tmp/files-up") == 0);
}

// Reading into and writing from memory none of whose pages are there yet,
// so that the copies fault: new anonymous memory, and private mappings of
// the very file read or written, whose faults read that file.
static void mapped(void) {
	enum { SIZE = 3 * 4096 };
	static char pattern[SIZE];
	for(int i = 0; i < SIZE; i++)
		pattern[i] = (char)(i % 251);
	int fd = open("/tmp/mapped", O_RDWR | O_CREAT | O_TRUNC, 0644);
	CHECK(fd >= 0 && write(fd, pattern, SIZE) == SIZE);
	char *map = mmap(NULL, SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
	CHECK(map != MAP_FAILED);
	CHECK(pread(fd, map, SIZE, 0) == SIZE && !memcmp(map, pattern, SIZE));
	char *again = mmap(NULL, SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
	CHECK(again != MAP_FAILED);
	CHECK(pwrite(fd, again, SIZE, SIZE) == SIZE);
	char *fresh = mmap(NULL, 2 * SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	CHECK(fresh != MAP_FAILED);
	CHECK(pread(fd, fresh, 2 * SIZE, 0) == 2 * SIZE);
	CHECK(!memcmp(fresh, pattern, SIZE) && !memcmp(fresh + SIZE, pattern, SIZE));
	// Copies into memory a little past the file's page within a page, and
	// from memory into the file a little past it, go from the end, and
	// make the pages as they go: memory none of whose pages are there yet,
	// 1, 16 and 511 bytes past, and 512, where the copies go forward.
	static const char none[SIZE];
	static const int ahead[] = {1, 16, 511, 512};
	for(size_t i = 0; i < sizeof(ahead) / sizeof(ahead[0]); i++) {
		int flags = MAP_PRIVATE | MAP_ANONYMOUS;
		char *into = mmap(NULL, SIZE + 4096, PROT_READ | PROT_WRITE, flags, -1, 0);
		char *blank = mmap(NULL, SIZE, PROT_READ | PROT_WRITE, flags, -1, 0);
		CHECK(into != MAP_FAILED && blank != MAP_FAILED);
		char *at = into + ahead[i];
		CHECK(pread(fd, at, SIZE, 0) == SIZE && !memcmp(at, pattern, SIZE));
		CHECK(pwrite(fd, blank, SIZE, ahead[i]) == SIZE);
		CHECK(pread(fd, at, SIZE, ahead[i]) == SIZE && !memcmp(at, none, SIZE));
		CHECK(pwrite(fd, fresh, 2 * SIZE, 0) == 2 * SIZE);
		CHECK(!munmap(into, SIZE + 4096) && !munmap(blank, SIZE));
	}
	// New memory reads as zeros, and writes them.
	char *zeros = mmap(NULL, SIZE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	CHECK(zeros != MAP_FAILED);
	CHECK(pwrite(fd, zeros, SIZE, 0) == SIZE);
	CHECK(pread(fd, fresh, SIZE, 0) == SIZE && !memcmp(fresh, zeros, SIZE));
	CHECK(!munmap(map, SIZE) && !munmap(again, SIZE));
	CHECK(!munmap(fresh, 2 * SIZE) && !munmap(zeros, SIZE));
	CHECK(!close(fd) && !unlink("/tmp/mapped"));
}

// The folders by their plain names and by Unix's (ADRs 0113 and 0114):
// each Unix name is a link to the folder that holds what it held, and
// the .hidden of the root and of /system list them, so that ls leaves
// them out.
static void folder_names(void) {
	static const char *const links[][2] = {
		{"/bin", "system/commands"},
		{"/dev", "devices"},
		{"/etc", "system/settings"},
		{"/proc", "processes"},
		{"/run", "system/live"},
		{"/sbin", "system/commands"},
		{"/sys", "kernel"},
		{"/usr", "system"},
		{"/var", "system/state"},
		{"/var/run", "../live"},
		{"/system/bin", "commands"},
		{"/system/lib", "libraries"},
		{"/system/share", "resources"},
	};
	char target[64], real[PATH_MAX];
	for(size_t i = 0; i < sizeof(links) / sizeof(links[0]); i++) {
		ssize_t n = readlink(links[i][0], target, sizeof(target));
		CHECK(n == (ssize_t)strlen(links[i][1]) && !memcmp(target, links[i][1], n));
	}
	CHECK(realpath("/dev/null", real) && !strcmp(real, "/devices/null"));
	CHECK(realpath("/var/run", real) && !strcmp(real, "/system/live"));
	CHECK(realpath("/etc/nanorc", real) && !strcmp(real, "/system/settings/nanorc"));
	CHECK(realpath("/usr/lib/ld.so", real) && !strcmp(real, "/system/libraries/ld.so"));
	CHECK(realpath("/usr/share/man", real) && !strcmp(real, "/system/resources/man"));
	struct stat st, want;
	CHECK(stat("/dev/null", &st) == 0 && stat("/devices/null", &want) == 0);
	CHECK(st.st_ino == want.st_ino && st.st_dev == want.st_dev);
	CHECK(access("/proc/mounts", R_OK) == 0 && access("/sys/version", R_OK) == 0);
	char hidden[128] = {0};
	int fd = open("/.hidden", O_RDONLY);
	CHECK(fd >= 0 && read(fd, hidden, sizeof(hidden) - 1) > 0);
	close(fd);
	CHECK(!strcmp(hidden, "bin\ndev\netc\nproc\nrun\nsbin\nsys\nusr\nvar\n"));
	memset(hidden, 0, sizeof(hidden));
	fd = open("/system/.hidden", O_RDONLY);
	CHECK(fd >= 0 && read(fd, hidden, sizeof(hidden) - 1) > 0);
	close(fd);
	CHECK(!strcmp(hidden, "bin\nlib\nshare\n"));
	// The working directory is where the link led.
	CHECK(chdir("/dev") == 0 && in_dir("/devices"));
	CHECK(chdir("/") == 0);
	// Descriptors by Unix's names for them.
	int fds[2];
	CHECK(pipe(fds) == 0);
	snprintf(target, sizeof(target), "/dev/fd/%d", fds[0]);
	CHECK(stat(target, &st) == 0 && S_ISFIFO(st.st_mode));
	CHECK(stat("/dev/stdin", &st) == 0);
	close(fds[0]);
	close(fds[1]);
}

// Descriptors by their names in /devices/fd, and /devices/stdin,
// /devices/stdout and /devices/stderr: opening one copies the descriptor,
// as dup does, so the copy shares its offset, and never with more access
// than it has.
static void named_descriptors(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	char name[32];
	snprintf(name, sizeof(name), "/devices/fd/%d", fds[0]);
	struct stat st;
	CHECK(stat(name, &st) == 0 && S_ISFIFO(st.st_mode));
	CHECK(access(name, R_OK) == 0);
	int reader = open(name, O_RDONLY | O_CLOEXEC);
	CHECK(reader >= 0 && reader != fds[0]);
	CHECK(fcntl(reader, F_GETFD) & FD_CLOEXEC);
	// The reading end cannot be opened for writing.
	CHECK_ERROR(open(name, O_WRONLY), EACCES);
	CHECK(write(fds[1], "piped", 5) == 5);
	char buffer[16] = {0};
	CHECK(read(reader, buffer, sizeof(buffer)) == 5 && !memcmp(buffer, "piped", 5));
	close(reader);
	// What a shell's process substitution does: a child reads the pipe
	// by its name, as `cat /devices/fd/N` would.
	snprintf(name, sizeof(name), "/devices/fd/%d", fds[1]);
	FILE *out = fopen(name, "w");
	CHECK(out != NULL);
	if(out) {
		CHECK(fputs("through stdio\n", out) >= 0);
		CHECK(fclose(out) == 0);
	}
	CHECK(read(fds[0], buffer, sizeof(buffer)) == 14 && !memcmp(buffer, "through stdio\n", 14));
	close(fds[0]);
	close(fds[1]);
	// A file's copy shares the offset: reading on goes on from it.
	int fd = open("/tmp/named", O_RDWR | O_CREAT | O_TRUNC, 0644);
	CHECK(fd >= 0 && write(fd, "0123456789", 10) == 10);
	CHECK(lseek(fd, 4, SEEK_SET) == 4);
	snprintf(name, sizeof(name), "/devices/fd/%d", fd);
	// O_TRUNC does nothing, as on the BSDs.
	int copy = open(name, O_RDWR | O_TRUNC);
	CHECK(copy >= 0);
	CHECK(read(copy, buffer, 3) == 3 && !memcmp(buffer, "456", 3));
	CHECK(lseek(fd, 0, SEEK_CUR) == 7);
	CHECK_ERROR(open(name, O_RDONLY | O_CREAT | O_EXCL, 0644), EEXIST);
	CHECK_ERROR(open(name, O_RDONLY | O_DIRECTORY), ENOTDIR);
	close(copy);
	// The standard ones, and relative to /devices.
	int saved = dup(0);
	CHECK(saved >= 0 && dup2(fd, 0) == 0);
	CHECK(lseek(0, 0, SEEK_SET) == 0);
	int in = open("/devices/stdin", O_RDONLY);
	CHECK(in >= 0 && read(in, buffer, 2) == 2 && !memcmp(buffer, "01", 2));
	close(in);
	CHECK(dup2(saved, 0) == 0);
	close(saved);
	CHECK(chdir("/devices") == 0);
	snprintf(name, sizeof(name), "fd/%d", fd);
	CHECK(stat(name, &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 10);
	CHECK(chdir("/") == 0);
	CHECK(stat("/devices/stdout", &st) == 0 && stat("/devices/stderr", &st) == 0);
	// A descriptor that is not open has no name; nor have others there.
	close(fd);
	snprintf(name, sizeof(name), "/devices/fd/%d", fd);
	CHECK_ERROR(open(name, O_RDONLY), ENOENT);
	CHECK_ERROR(stat("/devices/fd/999", &st), ENOENT);
	CHECK_ERROR(stat("/devices/fd/01", &st), ENOENT);
	CHECK_ERROR(stat("/devices/fd/x", &st), ENOENT);
	CHECK(unlink("/tmp/named") == 0);
}

// The permission bits and owners of `path` (without following a link
// there) are `mode`, `uid` and `gid`.
static int has(const char *path, mode_t mode, uid_t uid, gid_t gid) {
	struct stat st;
	return lstat(path, &st) == 0 && (st.st_mode & 07777) == mode && st.st_uid == uid
			&& st.st_gid == gid;
}

// Owners, groups and modes (ADR 0213): what is made gets the mode asked
// for less the umask, chmod and chown change them, a chown takes the
// set-user-ID bit away, a set-group-ID directory passes on its group,
// and a symbolic link's mode cannot change.
static void modes(void) {
	mode_t old = umask(022);
	// The trees Spark makes: /tmp for all, the rest root's.
	CHECK(has("/tmp", 01777, 0, 0));
	CHECK(has("/system", 0755, 0, 0));
	CHECK(has("/system/live", 0755, 0, 0));
	CHECK(has("/system/tests/posix/files", 0755, 0, 0));
	CHECK(has("/devices/null", 0666, 0, 0));
	CHECK(mkdir("/tmp/files-m", 0777) == 0 && has("/tmp/files-m", 0755, 0, 0));
	int fd = open("/tmp/files-m/f", O_RDWR | O_CREAT | O_EXCL, 0666);
	CHECK(fd >= 0 && has("/tmp/files-m/f", 0644, 0, 0));
	// The mask is the process's, and counts at once.
	umask(077);
	CHECK(close(open("/tmp/files-m/p", O_WRONLY | O_CREAT | O_EXCL, 0666)) == 0);
	CHECK(has("/tmp/files-m/p", 0600, 0, 0));
	umask(022);
	// A file that is there keeps its mode, whatever open asks.
	CHECK(close(open("/tmp/files-m/p", O_WRONLY | O_CREAT, 0777)) == 0);
	CHECK(has("/tmp/files-m/p", 0600, 0, 0));
	CHECK(mkfifo("/tmp/files-m/fifo", 0666) == 0 && has("/tmp/files-m/fifo", 0644, 0, 0));
	CHECK(mknod("/tmp/files-m/node", S_IFREG | 0640, 0) == 0);
	CHECK(has("/tmp/files-m/node", 0640, 0, 0));

	// chmod, fchmod and fchmodat; bits past 07777 are dropped.
	CHECK(chmod("/tmp/files-m/f", 04755) == 0 && has("/tmp/files-m/f", 04755, 0, 0));
	CHECK(fchmod(fd, 0600) == 0 && has("/tmp/files-m/f", 0600, 0, 0));
	CHECK(fchmodat(AT_FDCWD, "/tmp/files-m/f", 0640, 0) == 0);
	CHECK(has("/tmp/files-m/f", 0640, 0, 0));
	CHECK(chmod("/tmp/files-m/f", S_IFREG | 0644) == 0 && has("/tmp/files-m/f", 0644, 0, 0));
	// fchmod and fchown need a descriptor open for writing.
	int read_only = open("/tmp/files-m/f", O_RDONLY);
	CHECK(read_only >= 0);
	CHECK_ERROR(fchmod(read_only, 0600), EACCES);
	CHECK_ERROR(fchown(read_only, 1000, 100), EACCES);
	close(read_only);

	// chown, with -1 leaving one; a chown takes the set-user-ID bit away,
	// and the set-group-ID bit of a file its group may run, as Linux does,
	// even when nothing else changes.
	CHECK(chmod("/tmp/files-m/f", 06755) == 0);
	CHECK(chown("/tmp/files-m/f", 1000, 100) == 0 && has("/tmp/files-m/f", 0755, 1000, 100));
	CHECK(chown("/tmp/files-m/f", -1, 0) == 0 && has("/tmp/files-m/f", 0755, 1000, 0));
	CHECK(fchown(fd, 0, -1) == 0 && has("/tmp/files-m/f", 0755, 0, 0));
	CHECK(chmod("/tmp/files-m/f", 04755) == 0 && chown("/tmp/files-m/f", -1, -1) == 0);
	CHECK(has("/tmp/files-m/f", 0755, 0, 0));
	CHECK(chmod("/tmp/files-m/f", 02640) == 0 && chown("/tmp/files-m/f", -1, -1) == 0);
	CHECK(has("/tmp/files-m/f", 02640, 0, 0));
	close(fd);

	// A set-group-ID directory keeps its bits through a chown, and passes
	// its group on to what is made in it, and the bit to directories;
	// mkdir takes no set-ID bit from its mode.
	CHECK(chmod("/tmp/files-m", 02775) == 0 && chown("/tmp/files-m", -1, 100) == 0);
	CHECK(has("/tmp/files-m", 02775, 0, 100));
	CHECK(close(open("/tmp/files-m/g", O_WRONLY | O_CREAT, 0644)) == 0);
	CHECK(has("/tmp/files-m/g", 0644, 0, 100));
	CHECK(mkdir("/tmp/files-m/sub", 04777) == 0 && has("/tmp/files-m/sub", 02755, 0, 100));
	CHECK(rmdir("/tmp/files-m/sub") == 0);
	CHECK(chmod("/tmp/files-m", 0755) == 0 && chown("/tmp/files-m", -1, 0) == 0);

	// A symbolic link is 0777; lchown changes it and chown its target,
	// and its own mode cannot change.
	CHECK(symlink("f", "/tmp/files-m/l") == 0 && has("/tmp/files-m/l", 0777, 0, 0));
	CHECK(lchown("/tmp/files-m/l", 1000, -1) == 0 && has("/tmp/files-m/l", 0777, 1000, 0));
	CHECK(has("/tmp/files-m/f", 02640, 0, 0));
	CHECK(chown("/tmp/files-m/l", 1000, -1) == 0 && has("/tmp/files-m/f", 02640, 1000, 0));
	CHECK_ERROR(fchmodat(AT_FDCWD, "/tmp/files-m/l", 0600, AT_SYMLINK_NOFOLLOW), EOPNOTSUPP);
	CHECK(chmod("/tmp/files-m/l", 0600) == 0 && has("/tmp/files-m/f", 0600, 1000, 0));
	CHECK_ERROR(fchmodat(AT_FDCWD, "/tmp/files-m/f", 0600, 0x4000), EINVAL);

	// What cannot change: a read-only filesystem, and a missing file.
	CHECK_ERROR(chmod("/system/tests/posix/files", 0700), EROFS);
	CHECK_ERROR(chown("/system/tests/posix/files", 1000, -1), EROFS);
	CHECK_ERROR(chmod("/tmp/files-m/nowhere", 0600), ENOENT);
	static const char *const MADE[] = {"f", "p", "fifo", "node", "g", "l"};
	for(size_t i = 0; i < sizeof(MADE) / sizeof(MADE[0]); i++) {
		char path[64];
		snprintf(path, sizeof(path), "/tmp/files-m/%s", MADE[i]);
		CHECK(unlink(path) == 0);
	}
	CHECK(rmdir("/tmp/files-m") == 0);
	umask(old);
}

// An access list's attribute value, in Linux's posix_acl_xattr form, of
// `count` entries of tag, permissions and number, into `out`; returns
// its length.
static size_t acl_value(uint8_t *out, int count, const uint32_t (*entries)[3]) {
	const uint32_t version = 2;
	memcpy(out, &version, 4);
	for(int i = 0; i < count; i++) {
		uint16_t tag = (uint16_t)entries[i][0];
		uint16_t perm = (uint16_t)entries[i][1];
		memcpy(out + 4 + i * 8, &tag, 2);
		memcpy(out + 4 + i * 8 + 2, &perm, 2);
		memcpy(out + 4 + i * 8 + 4, &entries[i][2], 4);
	}
	return 4 + (size_t)count * 8;
}

// Linux's tags, and the number of an entry that names no one.
enum { USER_OBJ = 1, USER = 2, GROUP_OBJ = 4, GROUP = 8, MASK = 0x10, OTHER = 0x20 };
#define NO_ID UINT32_MAX

// Extended attributes and access lists (ADR 0256), on the tmpfs, as
// root: `user.` names on files and directories, with XATTR_CREATE and
// XATTR_REPLACE, the size alone for an empty buffer and ERANGE for one
// too short; `trusted.` names, root's; an access list, which sets the
// mode, and a directory's default list, which what is made in it takes,
// the umask left out; and what each refuses, with Linux's errors.
static void attributes(void) {
	int fd = open("/tmp/files-x", O_RDWR | O_CREAT | O_EXCL, 0644);
	CHECK(fd >= 0);
	CHECK(setxattr("/tmp/files-x", "user.origin", "wonderland", 10, 0) == 0);
	char value[64] = {0};
	CHECK(getxattr("/tmp/files-x", "user.origin", value, sizeof(value)) == 10);
	CHECK(!memcmp(value, "wonderland", 10));
	CHECK(getxattr("/tmp/files-x", "user.origin", NULL, 0) == 10);
	CHECK_ERROR(getxattr("/tmp/files-x", "user.origin", value, 4), ERANGE);
	CHECK_ERROR(getxattr("/tmp/files-x", "user.none", value, sizeof(value)), ENODATA);
	CHECK_ERROR(setxattr("/tmp/files-x", "user.origin", "x", 1, XATTR_CREATE), EEXIST);
	CHECK_ERROR(setxattr("/tmp/files-x", "user.none", "x", 1, XATTR_REPLACE), ENODATA);
	CHECK(setxattr("/tmp/files-x", "user.origin", "looking glass", 13, XATTR_REPLACE) == 0);
	CHECK(fsetxattr(fd, "user.empty", "", 0, XATTR_CREATE) == 0);
	CHECK(fgetxattr(fd, "user.empty", value, sizeof(value)) == 0);
	// Listed with a NUL after each.
	char names[256];
	ssize_t n = listxattr("/tmp/files-x", names, sizeof(names));
	CHECK(n == (ssize_t)(sizeof("user.origin") + sizeof("user.empty")));
	CHECK(flistxattr(fd, NULL, 0) == n);
	CHECK_ERROR(listxattr("/tmp/files-x", names, 3), ERANGE);
	int seen = 0;
	for(char *p = names; n > 0 && p < names + n; p += strlen(p) + 1)
		seen += !strcmp(p, "user.origin") + !strcmp(p, "user.empty");
	CHECK(seen == 2);
	CHECK(fremovexattr(fd, "user.empty") == 0);
	CHECK_ERROR(removexattr("/tmp/files-x", "user.empty"), ENODATA);
	// Names of no namespace, with nothing after it, or too long; flags
	// setxattr has not; a value past XATTR_SIZE_MAX.
	CHECK_ERROR(setxattr("/tmp/files-x", "other.name", "x", 1, 0), EOPNOTSUPP);
	CHECK_ERROR(setxattr("/tmp/files-x", "user.", "x", 1, 0), EINVAL);
	char long_name[300];
	memset(long_name, 'a', sizeof(long_name) - 1);
	memcpy(long_name, "user.", 5);
	long_name[sizeof(long_name) - 1] = 0;
	CHECK_ERROR(setxattr("/tmp/files-x", long_name, "x", 1, 0), ERANGE);
	CHECK_ERROR(setxattr("/tmp/files-x", "user.flags", "x", 1, 4), EINVAL);
	static char big[65537];
	CHECK_ERROR(setxattr("/tmp/files-x", "user.big", big, sizeof(big), 0), E2BIG);
	// `user.` names are files' and directories': a link has none, and
	// the calls without l follow it.
	CHECK(symlink("files-x", "/tmp/files-xl") == 0);
	CHECK_ERROR(lgetxattr("/tmp/files-xl", "user.origin", value, sizeof(value)), ENODATA);
	CHECK_ERROR(lsetxattr("/tmp/files-xl", "user.origin", "x", 1, 0), EPERM);
	CHECK(getxattr("/tmp/files-xl", "user.origin", value, sizeof(value)) == 13);
	// `trusted.` names are root's, on anything.
	CHECK(lsetxattr("/tmp/files-xl", "trusted.mark", "1", 1, 0) == 0);
	CHECK(lgetxattr("/tmp/files-xl", "trusted.mark", value, sizeof(value)) == 1);
	CHECK(llistxattr("/tmp/files-xl", names, sizeof(names)) == (ssize_t)sizeof("trusted.mark"));
	CHECK(lremovexattr("/tmp/files-xl", "trusted.mark") == 0);

	// An access list sets the mode's permission bits: the owner's, the
	// mask's for the group's, and the others'.
	static const uint32_t BOB[][3] = {
			{USER_OBJ, 6, NO_ID}, {USER, 7, 1001}, {GROUP_OBJ, 4, NO_ID},
			{MASK, 5, NO_ID}, {OTHER, 0, NO_ID}};
	uint8_t acl[64];
	size_t len = acl_value(acl, 5, BOB);
	CHECK(setxattr("/tmp/files-x", "system.posix_acl_access", acl, len, 0) == 0);
	struct stat st;
	CHECK(stat("/tmp/files-x", &st) == 0 && (st.st_mode & 07777) == 0650);
	// chmod sets the list's owner, mask and others, as it reads back.
	CHECK(fchmod(fd, 0640) == 0);
	uint8_t back[64];
	CHECK(getxattr("/tmp/files-x", "system.posix_acl_access", back, sizeof(back)) == (ssize_t)len);
	CHECK(back[4 + 3 * 8] == MASK && back[4 + 3 * 8 + 2] == 4);
	CHECK(back[4 + 1 * 8] == USER && back[4 + 1 * 8 + 2] == 7);
	// One the mode says all of is not kept; one out of order is refused.
	static const uint32_t PLAIN[][3] = {{USER_OBJ, 6, NO_ID}, {GROUP_OBJ, 4, NO_ID}, {OTHER, 4, NO_ID}};
	len = acl_value(acl, 3, PLAIN);
	CHECK(setxattr("/tmp/files-x", "system.posix_acl_access", acl, len, 0) == 0);
	CHECK_ERROR(getxattr("/tmp/files-x", "system.posix_acl_access", back, sizeof(back)), ENODATA);
	CHECK(stat("/tmp/files-x", &st) == 0 && (st.st_mode & 07777) == 0644);
	static const uint32_t BACKWARDS[][3] = {{OTHER, 4, NO_ID}, {USER_OBJ, 6, NO_ID}, {GROUP_OBJ, 4, NO_ID}};
	len = acl_value(acl, 3, BACKWARDS);
	CHECK_ERROR(setxattr("/tmp/files-x", "system.posix_acl_access", acl, len, 0), EINVAL);
	CHECK_ERROR(lsetxattr("/tmp/files-xl", "system.posix_acl_access", acl, len, 0), EOPNOTSUPP);

	// A default list is a directory's: what is made in it takes it, and
	// it decides the mode, not the umask.
	CHECK(mkdir("/tmp/files-xd", 0755) == 0);
	static const uint32_t SHARED[][3] = {
			{USER_OBJ, 7, NO_ID}, {USER, 6, 1001}, {GROUP_OBJ, 5, NO_ID},
			{MASK, 7, NO_ID}, {OTHER, 0, NO_ID}};
	len = acl_value(acl, 5, SHARED);
	CHECK_ERROR(setxattr("/tmp/files-x", "system.posix_acl_default", acl, len, 0), EACCES);
	CHECK(setxattr("/tmp/files-xd", "system.posix_acl_default", acl, len, 0) == 0);
	CHECK(getxattr("/tmp/files-xd", "system.posix_acl_default", back, sizeof(back)) == (ssize_t)len);
	mode_t old = umask(077);
	int made = open("/tmp/files-xd/f", O_RDWR | O_CREAT | O_EXCL, 0666);
	CHECK(made >= 0);
	CHECK(mkdir("/tmp/files-xd/sub", 0777) == 0);
	umask(old);
	CHECK(fstat(made, &st) == 0 && (st.st_mode & 07777) == 0660);
	CHECK(fgetxattr(made, "system.posix_acl_access", back, sizeof(back)) == (ssize_t)len);
	CHECK(back[4 + 1 * 8] == USER && back[4 + 1 * 8 + 2] == 6);
	CHECK(stat("/tmp/files-xd/sub", &st) == 0 && (st.st_mode & 07777) == 0770);
	CHECK(getxattr("/tmp/files-xd/sub", "system.posix_acl_default", back, sizeof(back)) == (ssize_t)len);
	close(made);
	close(fd);
	CHECK(unlink("/tmp/files-xd/f") == 0);
	CHECK(rmdir("/tmp/files-xd/sub") == 0);
	CHECK(rmdir("/tmp/files-xd") == 0);
	CHECK(unlink("/tmp/files-xl") == 0);
	CHECK(unlink("/tmp/files-x") == 0);
}

// What open, truncation, preallocation and scattered reads and writes
// refuse, with the errors Linux 6.8 gives.
static void refusals(void) {
	CHECK(mkdir("/tmp/files-r", 0755) == 0);
	// Open neither makes nor cuts a directory, and makes none with
	// O_DIRECTORY (Linux 6.4 and later).
	CHECK_ERROR(open("/tmp/files-r", O_RDONLY | O_CREAT, 0644), EISDIR);
	CHECK_ERROR(open("/tmp/files-r", O_RDONLY | O_TRUNC), EISDIR);
	CHECK_ERROR(open("/tmp/files-r", O_RDONLY | O_CREAT | O_DIRECTORY, 0755), EINVAL);
	CHECK_ERROR(open("/tmp/files-r/new", O_RDONLY | O_CREAT | O_DIRECTORY, 0755), EINVAL);
	CHECK_ERROR(access("/tmp/files-r/new", F_OK), ENOENT);
	int fd = open("/tmp/files-r/f", O_RDWR | O_CREAT | O_TRUNC, 0644);
	int read_only = open("/tmp/files-r/f", O_RDONLY);
	CHECK(fd >= 0 && read_only >= 0);
	CHECK_ERROR(ftruncate(fd, -1), EINVAL);
	CHECK_ERROR(truncate("/tmp/files-r/f", -1), EINVAL);
	CHECK_ERROR(ftruncate(read_only, 0), EINVAL);
	// posix_fallocate returns its error.
	CHECK(posix_fallocate(fd, -1, 10) == EINVAL);
	CHECK(posix_fallocate(fd, 0, 0) == EINVAL);
	CHECK(posix_fallocate(fd, 0, -5) == EINVAL);
	CHECK(posix_fallocate(read_only, 0, 10) == EBADF);
	CHECK(posix_fallocate(fd, 1, INT64_MAX) == EFBIG);
	int ends[2];
	CHECK(pipe(ends) == 0);
	CHECK(posix_fallocate(ends[0], 0, 10) == EBADF);
	CHECK(posix_fallocate(ends[1], 0, 10) == ESPIPE);
	close(ends[0]);
	close(ends[1]);
	int dir = open("/tmp/files-r", O_RDONLY);
	CHECK(posix_fallocate(dir, 0, 10) == EBADF);
	close(dir);
	struct stat st;
	CHECK(posix_fallocate(fd, 0, 100) == 0 && fstat(fd, &st) == 0 && st.st_size == 100);
	CHECK(posix_fallocate(fd, 10, 20) == 0 && fstat(fd, &st) == 0 && st.st_size == 100);
	// More vectors than IOV_MAX, or fewer than none; a sum past
	// SSIZE_MAX fails rather than overrunning anything.
	static struct iovec many[IOV_MAX + 1];
	char byte;
	for(int i = 0; i <= IOV_MAX; i++)
		many[i] = (struct iovec){&byte, 1};
	CHECK_ERROR(readv(fd, many, IOV_MAX + 1), EINVAL);
	CHECK_ERROR(writev(fd, many, IOV_MAX + 1), EINVAL);
	CHECK_ERROR(readv(fd, many, -1), EINVAL);
	char small[4];
	struct iovec huge[2] = {{small, SSIZE_MAX}, {small, SSIZE_MAX}};
	errno = 0;
	CHECK(readv(fd, huge, 2) == -1 && (errno == EINVAL || errno == EFAULT));
	errno = 0;
	CHECK(writev(fd, huge, 2) == -1 && (errno == EINVAL || errno == EFAULT));
	close(read_only);
	close(fd);
	// statvfs of a file tells of its filesystem (and of a named pipe it
	// would not wait for a writer, but Radix has none yet).
	struct statvfs vfs;
	CHECK(statvfs("/tmp/files-r/f", &vfs) == 0 && vfs.f_bsize > 0);
	CHECK(unlink("/tmp/files-r/f") == 0);
	CHECK(rmdir("/tmp/files-r") == 0);
}

int main(void) {
	check_start("files");
	RUN(basics);
	RUN(descriptions);
	RUN(saved_and_restored);
	RUN(standard_io);
	RUN(directories);
	RUN(directory_descriptors);
	RUN(temporary_files);
	RUN(times);
	RUN(links);
	RUN(hard_links);
	RUN(special_files);
	RUN(climbing);
	RUN(mapped);
	RUN(folder_names);
	RUN(named_descriptors);
	RUN(modes);
	RUN(attributes);
	RUN(refusals);
	return check_end();
}
