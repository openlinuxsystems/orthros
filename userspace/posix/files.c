// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Files through the C library: open and its flags, reading, writing and
// seeking, descriptions that share an offset, stdio, directories and
// their descriptors, temporary files, renaming and removing, times,
// symbolic links, reading into and writing from memory not made yet, and
// descriptors by their names in /dev/fd, on the tmpfs at /tmp and the
// read-only /system.

#include <dirent.h>
#include <fcntl.h>
#include <limits.h>
#include <ctype.h>
#include <ftw.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/uio.h>

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
	CHECK_ERROR(utimensat(AT_FDCWD, "/dev/null", NULL, 0), EPERM);
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
// loop, and the links to /system that Spark makes.
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
	// No hard links yet, and nothing new in /system.
	CHECK_ERROR(link("/tmp/links/a/b/file", "/tmp/links/hard"), EPERM);
	CHECK_ERROR(symlink("x", "/system/new"), EROFS);
	CHECK(nftw("/tmp/links", remove_entry, 8, FTW_DEPTH | FTW_PHYS) == 0);
	CHECK_ERROR(lstat("/tmp/links", &st), ENOENT);
	// Spark's links to /system, for Unix programs and scripts.
	CHECK(lstat("/bin", &st) == 0 && S_ISLNK(st.st_mode));
	CHECK(readlink("/usr", target, sizeof(target)) == 6 && !memcmp(target, "system", 6));
	CHECK(realpath("/usr/share/../bin", real) && !strcmp(real, "/system/bin"));
	CHECK(stat("/etc", &st) == 0 && S_ISDIR(st.st_mode));
	CHECK(stat("/var", &st) == 0 && S_ISDIR(st.st_mode));
	CHECK(realpath("/sbin/.", real) && !strcmp(real, "/system/bin"));
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
	// New memory reads as zeros, and writes them.
	char *zeros = mmap(NULL, SIZE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	CHECK(zeros != MAP_FAILED);
	CHECK(pwrite(fd, zeros, SIZE, 0) == SIZE);
	CHECK(pread(fd, fresh, SIZE, 0) == SIZE && !memcmp(fresh, zeros, SIZE));
	CHECK(!munmap(map, SIZE) && !munmap(again, SIZE));
	CHECK(!munmap(fresh, 2 * SIZE) && !munmap(zeros, SIZE));
	CHECK(!close(fd) && !unlink("/tmp/mapped"));
}

// Descriptors by their names in /dev/fd, and /dev/stdin, /dev/stdout and
// /dev/stderr: opening one copies the descriptor, as dup does, so the
// copy shares its offset, and never with more access than it has.
static void named_descriptors(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	char name[32];
	snprintf(name, sizeof(name), "/dev/fd/%d", fds[0]);
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
	// by its name, as `cat /dev/fd/N` would.
	snprintf(name, sizeof(name), "/dev/fd/%d", fds[1]);
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
	snprintf(name, sizeof(name), "/dev/fd/%d", fd);
	// O_TRUNC does nothing, as on the BSDs.
	int copy = open(name, O_RDWR | O_TRUNC);
	CHECK(copy >= 0);
	CHECK(read(copy, buffer, 3) == 3 && !memcmp(buffer, "456", 3));
	CHECK(lseek(fd, 0, SEEK_CUR) == 7);
	CHECK_ERROR(open(name, O_RDONLY | O_CREAT | O_EXCL, 0644), EEXIST);
	CHECK_ERROR(open(name, O_RDONLY | O_DIRECTORY), ENOTDIR);
	close(copy);
	// The standard ones, and relative to /dev.
	int saved = dup(0);
	CHECK(saved >= 0 && dup2(fd, 0) == 0);
	CHECK(lseek(0, 0, SEEK_SET) == 0);
	int in = open("/dev/stdin", O_RDONLY);
	CHECK(in >= 0 && read(in, buffer, 2) == 2 && !memcmp(buffer, "01", 2));
	close(in);
	CHECK(dup2(saved, 0) == 0);
	close(saved);
	CHECK(chdir("/dev") == 0);
	snprintf(name, sizeof(name), "fd/%d", fd);
	CHECK(stat(name, &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 10);
	CHECK(chdir("/") == 0);
	CHECK(stat("/dev/stdout", &st) == 0 && stat("/dev/stderr", &st) == 0);
	// A descriptor that is not open has no name; nor have others there.
	close(fd);
	snprintf(name, sizeof(name), "/dev/fd/%d", fd);
	CHECK_ERROR(open(name, O_RDONLY), ENOENT);
	CHECK_ERROR(stat("/dev/fd/999", &st), ENOENT);
	CHECK_ERROR(stat("/dev/fd/01", &st), ENOENT);
	CHECK_ERROR(stat("/dev/fd/x", &st), ENOENT);
	CHECK(unlink("/tmp/named") == 0);
}

int main(void) {
	check_start("files");
	RUN(basics);
	RUN(descriptions);
	RUN(standard_io);
	RUN(directories);
	RUN(directory_descriptors);
	RUN(temporary_files);
	RUN(times);
	RUN(links);
	RUN(mapped);
	RUN(named_descriptors);
	return check_end();
}
