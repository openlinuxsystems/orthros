// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Starting a program as another user, for the tests Spark starts with
// the names capability (`--names`, ADR 0214): as authd starts a user's
// shell, with a root stamped for that user and without the bypass right.

#ifndef POSIX_NAMES_H
#define POSIX_NAMES_H

#include <radix/radix.h>
#include <stdint.h>

#include "check.h"

// The descriptor Spark gives the names capability as.
#define NAMES (RADIX_STARTUP_SLOT_FD_BASE + 3)

extern char **environ;

// The errno a status of the kernel's or of a file maps to.
static int status_errno(uint64_t status) {
	if(RADIX_STATUS_DOMAIN(status) == RADIX_FILE_ERROR_DOMAIN)
		return radix_file_errno(status);
	return radix_kernel_errno(status);
}

// Checks that `call`, a Radix call, failed with `error`, an errno.
#define CHECK_STATUS(call, error) \
	do { \
		uint64_t check_status_ = (call); \
		if(check_status_ == 0 || status_errno(check_status_) != (error)) \
			check_failed(__FILE__, __LINE__, #call " fails with " #error, \
					check_status_ ? status_errno(check_status_) : 0); \
	} while(0)

// Puts `count` empty slots in `slots`: those of descriptors that are free
// now, which nothing else takes while the test holds them.
static void spare(uint32_t *slots, int count) {
	int fds[8];
	for(int i = 0; i < count; i++) {
		fds[i] = dup(0);
		CHECK(fds[i] >= 0);
		slots[i] = RADIX_STARTUP_SLOT_FD_BASE + (uint32_t)fds[i];
	}
	for(int i = 0; i < count; i++)
		close(fds[i]);
}

// The rights of the capability in `slot` (0 if it holds none).
static uint64_t rights_of(uint32_t slot) {
	struct radix_capability_info_results info;
	return radix_capability_info(slot, &info) ? 0 : info.rights;
}

// Appends `text` and its NUL to the block of `*at` bytes.
static int append(char *block, size_t *at, const char *text) {
	size_t len = strlen(text) + 1;
	if(*at + len > RADIX_PROCESS_EXEC_MAX)
		return 0;
	memcpy(block + *at, text, len);
	*at += len;
	return 1;
}

// The block `exec` takes to run `program`, a path beneath the root, with
// `argv` and this environment, in `/`, as Spark makes it; returns its
// length.
static size_t exec_block(char *block, const char *program, char *const argv[]) {
	struct radix_exec_header header = {RADIX_PROCESS_EXEC_VERSION, 0, 0, 2};
	size_t at = sizeof(header);
	int fits = 1;
	for(; argv[header.args]; header.args++)
		fits &= append(block, &at, argv[header.args]);
	for(; environ[header.env]; header.env++)
		fits &= append(block, &at, environ[header.env]);
	char path[256];
	fits &= snprintf(path, sizeof(path), "/%s", program) < (int)sizeof(path);
	fits &= append(block, &at, "/");
	fits &= append(block, &at, path);
	CHECK(fits);
	memcpy(block, &header, sizeof(header));
	return at;
}

// Starts `program`, a path beneath the root, with `argv` as `user` in
// `group` and `groups`, as Spark starts a POSIX program, with the test's
// descriptors 0 to 2, but with a root that acts for that name and has no
// bypass right: made with `stamp`, then derived without it. Returns its
// number.
static pid_t start_as(const char *program, uint32_t user, uint32_t group, const uint32_t *groups,
		size_t count, char *const argv[]) {
	uint32_t s[4];
	spare(s, 4);
	uint32_t stamped = s[0], root = s[1], file = s[2], child = s[3];
	uint64_t rights = rights_of(RADIX_STARTUP_SLOT_ROOT);
	CHECK(rights & RADIX_RIGHTS_BYPASS);
	CHECK(radix_names_stamp(NAMES, RADIX_STARTUP_SLOT_ROOT, stamped, user, group, groups,
				  count * sizeof(*groups)) == 0);
	CHECK(rights_of(stamped) == rights);
	CHECK(radix_capability_derive(stamped, root, rights & ~RADIX_RIGHTS_BYPASS, 0) == 0);
	struct radix_directory_open_results opened;
	CHECK(radix_directory_open(RADIX_STARTUP_SLOT_ROOT, program, strlen(program), 0,
				  RADIX_RIGHTS_READ, file, 0, &opened) == 0);
	CHECK(radix_process_create(RADIX_STARTUP_SLOT_PROCESS, RADIX_OBJECT_KIND_PROCESS, 128,
				  child) == 0);
	CHECK(radix_names_name_process(NAMES, child, user, group, groups, count * sizeof(*groups)) ==
			0);
	const uint32_t shares[][2] = {
			{root, RADIX_STARTUP_SLOT_ROOT},
			{RADIX_STARTUP_SLOT_RANDOM, RADIX_STARTUP_SLOT_RANDOM},
			{RADIX_STARTUP_SLOT_LOG, RADIX_STARTUP_SLOT_LOG},
			{RADIX_STARTUP_SLOT_FD_BASE, RADIX_STARTUP_SLOT_FD_BASE},
			{RADIX_STARTUP_SLOT_FD_BASE + 1, RADIX_STARTUP_SLOT_FD_BASE + 1},
			{RADIX_STARTUP_SLOT_FD_BASE + 2, RADIX_STARTUP_SLOT_FD_BASE + 2},
	};
	for(size_t i = 0; i < sizeof(shares) / sizeof(shares[0]); i++)
		CHECK(radix_process_share(child, shares[i][0], shares[i][1], rights_of(shares[i][0]),
					  0) == 0);
	static char block[RADIX_PROCESS_EXEC_MAX];
	size_t len = exec_block(block, program, argv);
	CHECK(radix_process_exec(child, file, block, len) == 0);
	// A process that has started keeps its name.
	CHECK_STATUS(radix_names_name_process(NAMES, child, 0, 0, NULL, 0), EINVAL);
	struct radix_process_info_results info;
	CHECK(radix_process_info(child, &info) == 0);
	for(int i = 0; i < 4; i++)
		radix_capability_delete(s[i]);
	return (pid_t)info.number;
}

#endif
