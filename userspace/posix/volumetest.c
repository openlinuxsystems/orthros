// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the volumes boot test runs (ADR 0163): `volumetest <label>` waits
// for volumed(8) to mount the stick the host plugs in again after boot at
// /volumes/<label>, checks volumed --list shows it, writes a file there,
// mounts it again with remount(8) only to read, where the file reads and
// writes are refused, and then to write, ejects it with eject(1), and
// checks it is gone and eject finds it no more. The host then reads the
// file from the stick's image.

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/stat.h>

#include "check.h"
#include "programs.h"

// What the file holds.
#define TEXT "written on Dukovo, ejected safely\n"

int main(int argc, char **argv) {
	check_start("volumetest");
	if(argc != 2) {
		printf("volumetest: usage: volumetest <label>\n");
		return 1;
	}
	char path[256], file[300];
	snprintf(path, sizeof path, "/volumes/%s", argv[1]);
	snprintf(file, sizeof file, "%s/ejected.txt", path);

	printf("volumetest: waiting for %s\n", path);
	// A mount, not the directory volumed makes before it mounts there.
	struct stat volumes, st;
	CHECK(stat("/volumes", &volumes) == 0);
	long long deadline = check_now_ns() + 120ll * 1000 * 1000 * 1000;
	while(!(stat(path, &st) == 0 && st.st_dev != volumes.st_dev) && check_now_ns() < deadline)
		check_sleep_ms(100);
	int fd = open(file, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
	CHECK(fd >= 0);
	if(fd < 0)
		return check_end();
	CHECK(write(fd, TEXT, sizeof TEXT - 1) == (ssize_t)(sizeof TEXT - 1));
	CHECK(fsync(fd) == 0);
	close(fd);
	printf("volumetest: wrote %s\n", file);
	CONTAINS(path, "/system/commands/volumed", "--list");

	STATUS(0, "/system/commands/remount", "-r", argv[1]);
	char back[sizeof TEXT] = {0};
	fd = open(file, O_RDONLY | O_CLOEXEC);
	CHECK(fd >= 0 && read(fd, back, sizeof back) == (ssize_t)(sizeof TEXT - 1));
	CHECK(!strcmp(back, TEXT));
	if(fd >= 0)
		close(fd);
	errno = 0;
	CHECK(open(file, O_WRONLY | O_APPEND | O_CLOEXEC) < 0 && errno == EROFS);
	STATUS(0, "/system/commands/remount", "-w", argv[1]);
	fd = open(file, O_WRONLY | O_APPEND | O_CLOEXEC);
	CHECK(fd >= 0);
	if(fd >= 0)
		close(fd);
	printf("volumetest: mounted %s again, only to read, then to write\n", path);

	STATUS(0, "/system/commands/eject", argv[1]);
	CHECK(stat(path, &st) != 0);
	printf("volumetest: ejected %s\n", path);
	STATUS(1, "/system/commands/eject", argv[1]);
	return check_end();
}
