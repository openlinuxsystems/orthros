// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Every program in /system/commands was run by the tests before this one, the
// utilities' and the administration tools': each of them notes the
// programs it runs in /tmp/posix-commands (programs.h), and this reads
// what they noted. A few run in other boot tests instead, each for the
// reason given. So the scripted suite of Phase 9's exit runs every
// command, and a new command comes with its tests.

#include <dirent.h>
#include <fcntl.h>

#include "check.h"

// The programs that other boot tests run, and why not this one.
static const char *const ELSEWHERE[][2] = {
		{"spark", "the first program, which every boot test starts"},
		{"consoled", "the consoles boot test's service: another would take the screen and keyboard"},
		{"keymap", "the consoles boot test's: the layout it sets is the one consoled types in"},
		{"volumed", "the volumes boot test's service: it mounts a stick plugged in after boot"},
		{"eject", "the volumes boot test's: it unmounts the stick volumed mounted"},
};

enum { ELSEWHERE_COUNT = sizeof(ELSEWHERE) / sizeof(ELSEWHERE[0]) };

// What the tests noted, a name a line.
static char noted[65536];

// Whether `name` is a line of `noted`.
static int was_run(const char *name) {
	size_t len = strlen(name);
	for(const char *line = noted; *line;) {
		const char *end = strchr(line, '\n');
		size_t line_len = end ? (size_t)(end - line) : strlen(line);
		if(line_len == len && !strncmp(line, name, len))
			return 1;
		if(!end)
			break;
		line = end + 1;
	}
	return 0;
}

// The reason `name` runs in another boot test, or NULL.
static const char *elsewhere(const char *name) {
	for(size_t i = 0; i < ELSEWHERE_COUNT; i++)
		if(!strcmp(ELSEWHERE[i][0], name))
			return ELSEWHERE[i][1];
	return NULL;
}

static void every_program(void) {
	int fd = open(COMMANDS_RUN, O_RDONLY | O_CLOEXEC);
	CHECK(fd >= 0);
	if(fd < 0) {
		printf("commands: nothing was noted: run utilities and admin first\n");
		return;
	}
	size_t len = 0;
	ssize_t got;
	while(len < sizeof(noted) - 1 && (got = read(fd, noted + len, sizeof(noted) - 1 - len)) > 0)
		len += got;
	noted[len] = 0;
	close(fd);
	CHECK(len < sizeof(noted) - 1);

	DIR *bin = opendir("/system/commands");
	CHECK(bin != NULL);
	if(!bin)
		return;
	int programs = 0, missed = 0;
	struct dirent *entry;
	while((entry = readdir(bin))) {
		const char *name = entry->d_name;
		if(name[0] == '.')
			continue;
		programs++;
		if(was_run(name) || elsewhere(name))
			continue;
		printf("commands: no test ran %s\n", name);
		missed++;
	}
	closedir(bin);
	CHECK(programs > 200);
	CHECK(missed == 0);
	// Each exception is a program there.
	for(size_t i = 0; i < ELSEWHERE_COUNT; i++) {
		char path[64];
		snprintf(path, sizeof(path), "/system/commands/%s", ELSEWHERE[i][0]);
		CHECK(access(path, X_OK) == 0);
	}
	printf("commands: %d programs in /system/commands, %d of them run in other boot tests\n",
			programs, (int)ELSEWHERE_COUNT);
}

int main(void) {
	check_start("commands");
	RUN(every_program);
	return check_end();
}
