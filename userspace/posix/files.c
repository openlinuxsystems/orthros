// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Files through the C library: open and its flags, reading, writing and
// seeking, descriptions that share an offset, stdio, directories and
// their descriptors, temporary files, renaming and removing, and times,
// on the tmpfs at /tmp and the read-only /system.

#include <dirent.h>
#include <fcntl.h>
#include <limits.h>
#include <ctype.h>
#include <ftw.h>
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

int main(void) {
	check_start("files");
	RUN(basics);
	RUN(descriptions);
	RUN(standard_io);
	RUN(directories);
	RUN(directory_descriptors);
	RUN(temporary_files);
	RUN(times);
	return check_end();
}
