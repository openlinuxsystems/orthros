// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Files through the C library: open and its flags, reading, writing and
// seeking, descriptions that share an offset, stdio, directories, and
// renaming and removing, on the tmpfs at /tmp and the read-only /system.

#include <dirent.h>
#include <fcntl.h>
#include <limits.h>
#include <sys/stat.h>
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
	CHECK(access("/system/tests/posix/files", R_OK) == 0);
	CHECK_ERROR(access("/nowhere", F_OK), ENOENT);
}

int main(void) {
	check_start("files");
	RUN(basics);
	RUN(descriptions);
	RUN(standard_io);
	RUN(directories);
	return check_end();
}
