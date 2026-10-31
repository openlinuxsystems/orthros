// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The views boot test (ADR 0215): a directory made of attached
// capabilities. Spark starts it as root with the names capability as
// descriptor 3 (`--names`). It builds a view for alice as authd will
// build a session's (13b.5): her home, the system's files, the devices,
// links with Unix's names, and a /tmp of the session's own in place of
// the system's, each through a root stamped for her. Then it starts
// itself as alice with that view as her root, without the right to add
// to it. Alice checks that she sees the view's names and nothing past
// them; that `..`, getcwd, realpath and links work across its entries
// as in any tree; that what was attached read-only, and the view's own
// directories, take no files; that she can neither mount nor attach in
// it; and that she can make a view of her own from what she holds. Root
// then checks that what alice made in her /tmp is in the session's
// directory, and what a view refuses to take.

#include <dirent.h>
#include <fcntl.h>
#include <limits.h>
#include <radix/radix.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/wait.h>

#include "check.h"

// The test's tree in the system's /tmp, from the root.
#define REL "tmp/views"
#define TREE "/" REL
// The session's own /tmp, in the system's.
#define SESSION_REL REL "/private/session"
#define SESSION_TMP "/" SESSION_REL
// This program, beneath the root.
#define PROGRAM "system/tests/posix/views"
#define ALICE 1000
// The descriptor Spark gives the names capability as.
#define NAMES (RADIX_STARTUP_SLOT_FD_BASE + 3)
#define MOTD "the view's message of the day\n"

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

// Opens `path` beneath the directory in `dir` with `rights` into `to`.
static uint64_t open_in(uint32_t dir, const char *path, uint64_t rights, uint32_t to) {
	struct radix_directory_open_results opened;
	return radix_directory_open(dir, path, strlen(path), 0, rights, to, 0, &opened);
}

// Attaches what `what` holds at `path` in the view in `view`.
static uint64_t attach(uint32_t view, const char *path, uint32_t what) {
	return radix_directory_attach(view, path, strlen(path), what);
}

// Opens `path` beneath `dir` with `rights` and attaches it at `at` in
// `view`, then lets go of it: the view keeps what it attached.
static void attach_path(uint32_t view, const char *at, uint32_t dir, const char *path,
		uint64_t rights) {
	uint32_t slot;
	spare(&slot, 1);
	CHECK(open_in(dir, path, rights, slot) == 0);
	CHECK(attach(view, at, slot) == 0);
	radix_capability_delete(slot);
}

// Makes a symbolic link at `path` in the view in `view` to `target`.
static uint64_t attach_link(uint32_t view, const char *path, const char *target) {
	return radix_directory_attach_link(view, path, strlen(path), target, strlen(target));
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

// The block `exec` takes to run this program with `argv` and this
// environment, in `/`, as Spark makes it; returns its length.
static size_t exec_block(char *block, char *const argv[]) {
	struct radix_exec_header header = {RADIX_PROCESS_EXEC_VERSION, 0, 0, 2};
	size_t at = sizeof(header);
	int fits = 1;
	for(; argv[header.args]; header.args++)
		fits &= append(block, &at, argv[header.args]);
	for(; environ[header.env]; header.env++)
		fits &= append(block, &at, environ[header.env]);
	fits &= append(block, &at, "/");
	fits &= append(block, &at, "/" PROGRAM);
	CHECK(fits);
	memcpy(block, &header, sizeof(header));
	return at;
}

// Starts this program with `argv` as alice, with the test's descriptors
// 0 to 2 and the directory in `root` as its root, as Spark starts a POSIX
// program. Returns its number.
static pid_t start_as_alice(uint32_t root, char *const argv[]) {
	uint32_t s[2];
	spare(s, 2);
	uint32_t program = s[0], child = s[1];
	CHECK(open_in(RADIX_STARTUP_SLOT_ROOT, PROGRAM, RADIX_RIGHTS_READ, program) == 0);
	CHECK(radix_process_create(RADIX_STARTUP_SLOT_PROCESS, RADIX_OBJECT_KIND_PROCESS, 128,
				  child) == 0);
	CHECK(radix_names_name_process(NAMES, child, ALICE, ALICE, NULL, 0) == 0);
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
	size_t len = exec_block(block, argv);
	CHECK(radix_process_exec(child, program, block, len) == 0);
	struct radix_process_info_results info;
	CHECK(radix_process_info(child, &info) == 0);
	radix_capability_delete(program);
	radix_capability_delete(child);
	return (pid_t)info.number;
}

// Makes the file `path`, holding `text`, with `mode` and the owner given.
static void make(const char *path, mode_t mode, uid_t owner, const char *text) {
	int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, mode);
	CHECK(fd >= 0);
	CHECK(write(fd, text, strlen(text)) == (ssize_t)strlen(text));
	CHECK(fchown(fd, owner, owner) == 0);
	CHECK(fchmod(fd, mode) == 0);
	close(fd);
}

// Makes the directory `path` with `mode` and the owner given.
static void make_dir(const char *path, mode_t mode, uid_t owner) {
	CHECK(mkdir(path, 0700) == 0);
	CHECK(chown(path, owner, owner) == 0);
	CHECK(chmod(path, mode) == 0);
}

// Reads the file at `path` into `text`; returns its length, or -1 with
// errno.
static ssize_t read_text(const char *path, char *text, size_t room) {
	int fd = open(path, O_RDONLY);
	if(fd < 0)
		return -1;
	ssize_t got = read(fd, text, room - 1);
	int error = errno;
	close(fd);
	if(got >= 0)
		text[got] = '\0';
	errno = error;
	return got;
}

// Whether the working directory is `path`.
static int in(const char *path) {
	char here[PATH_MAX];
	if(!getcwd(here, sizeof(here)))
		return 0;
	if(strcmp(here, path) != 0) {
		printf("views: in %s, not %s\n", here, path);
		return 0;
	}
	return 1;
}

static int by_name(const void *a, const void *b) {
	return strcmp(*(const char *const *)a, *(const char *const *)b);
}

// The names in the directory `path` but `.` and `..`, sorted and joined
// by spaces, into `out`.
static void listing(const char *path, char *out, size_t room) {
	char *names[32];
	size_t count = 0;
	out[0] = '\0';
	DIR *dir = opendir(path);
	CHECK(dir);
	if(!dir)
		return;
	struct dirent *entry;
	while((entry = readdir(dir)) && count < 32) {
		if(strcmp(entry->d_name, ".") && strcmp(entry->d_name, ".."))
			names[count++] = strdup(entry->d_name);
	}
	closedir(dir);
	qsort(names, count, sizeof(*names), by_name);
	for(size_t i = 0; i < count; i++) {
		if(i)
			strncat(out, " ", room - strlen(out) - 1);
		strncat(out, names[i], room - strlen(out) - 1);
		free(names[i]);
	}
}

// Alice, in the view: she sees its names, at their real paths, and
// nothing past them.
static void as_alice(void) {
	CHECK(getuid() == ALICE);
	char text[256];
	listing("/", text, sizeof(text));
	CHECK(strcmp(text, "bin devices etc system tmp users") == 0);
	if(strcmp(text, "bin devices etc system tmp users") != 0)
		printf("views: / holds %s\n", text);
	listing("/users", text, sizeof(text));
	CHECK(strcmp(text, "alice") == 0);
	// The session's /tmp, not the system's.
	errno = 0;
	CHECK(access(TREE, F_OK) == -1 && errno == ENOENT);
	CHECK(access("/tmp/marker", F_OK) == -1 && errno == ENOENT);
	int fd = open("/tmp/made-by-alice", O_CREAT | O_EXCL | O_WRONLY, 0644);
	CHECK(fd >= 0);
	close(fd);
	struct stat st;
	CHECK(stat("/tmp/made-by-alice", &st) == 0 && st.st_uid == ALICE);
	// `..` and getcwd across the view's entries.
	CHECK(chdir("/users/alice") == 0 && in("/users/alice"));
	CHECK(chdir("..") == 0 && in("/users"));
	CHECK(chdir("../tmp") == 0 && in("/tmp"));
	CHECK(read_text("../etc/motd", text, sizeof(text)) > 0 && strcmp(text, MOTD) == 0);
	// The kernel's own `..`, from what is attached back to the view.
	uint32_t slot;
	spare(&slot, 1);
	CHECK(open_in(RADIX_STARTUP_SLOT_ROOT, "tmp/../etc/motd", RADIX_RIGHTS_READ, slot) == 0);
	radix_capability_delete(slot);
	struct stat root, above;
	CHECK(stat("/", &root) == 0);
	CHECK(open_in(RADIX_STARTUP_SLOT_ROOT, "..", RADIX_RIGHTS_READ, slot) == 0);
	CHECK(fstat((int)(slot - RADIX_STARTUP_SLOT_FD_BASE), &above) == 0);
	CHECK(above.st_dev == root.st_dev && above.st_ino == root.st_ino);
	radix_capability_delete(slot);
	CHECK(chdir("/") == 0 && in("/"));
	// Links: the view's own, and one of hers to a path from `/`.
	char real[PATH_MAX];
	CHECK(realpath("/bin", real) && strcmp(real, "/system/commands") == 0);
	CHECK(access("/bin/true", X_OK) == 0);
	CHECK(symlink("/etc/motd", "/users/alice/motd") == 0);
	CHECK(read_text("/users/alice/motd", text, sizeof(text)) > 0 && strcmp(text, MOTD) == 0);
	CHECK(realpath("/users/alice/motd", real) && strcmp(real, "/etc/motd") == 0);
	// What was attached read-only, and the view's own directories, take
	// nothing; what was attached to write, as anywhere else.
	errno = 0;
	CHECK(open("/etc/motd", O_WRONLY) == -1 && errno == EROFS);
	CHECK_ERROR(mkdir("/new", 0755), EROFS);
	CHECK_ERROR(mkdir("/users/bob", 0755), EROFS);
	struct statvfs fs;
	CHECK(statvfs("/etc/motd", &fs) == 0 && (fs.f_flag & ST_RDONLY));
	CHECK(statvfs("/users/alice", &fs) == 0 && !(fs.f_flag & ST_RDONLY));
	fd = open("/users/alice/note", O_CREAT | O_EXCL | O_WRONLY, 0600);
	CHECK(fd >= 0);
	CHECK(write(fd, "hers", 4) == 4);
	close(fd);
	sync();
	// Her root cannot add to the view, nor mount in it.
	CHECK(!(rights_of(RADIX_STARTUP_SLOT_ROOT) & RADIX_RIGHTS_CONTROL));
	CHECK(open_in(RADIX_STARTUP_SLOT_ROOT, "users/alice", RADIX_RIGHTS_READ, slot) == 0);
	CHECK_STATUS(attach(RADIX_STARTUP_SLOT_ROOT, "more", slot), EACCES);
	CHECK(radix_directory_mount(RADIX_STARTUP_SLOT_ROOT, "tmp", 3, "tmpfs", 5, 0, 0) != 0);
	// A view of her own, from what she holds: her home, as `home`.
	uint32_t mine;
	spare(&mine, 1);
	CHECK(radix_directory_make_view(RADIX_STARTUP_SLOT_ROOT, mine) == 0);
	CHECK(rights_of(mine) & RADIX_RIGHTS_CONTROL);
	CHECK(attach(mine, "home", slot) == 0);
	// Not her view itself, nor a capability opened in it with control.
	CHECK_STATUS(attach(mine, "again", RADIX_STARTUP_SLOT_ROOT), EOPNOTSUPP);
	uint32_t inner;
	spare(&inner, 1);
	CHECK(open_in(mine, "home", RADIX_RIGHTS_READ | RADIX_RIGHTS_CONTROL, inner) == 0);
	CHECK(!(rights_of(inner) & RADIX_RIGHTS_CONTROL));
	radix_capability_delete(inner);
	char note[16] = {0};
	CHECK(open_in(mine, "home/note", RADIX_RIGHTS_READ, inner) == 0);
	CHECK(read((int)(inner - RADIX_STARTUP_SLOT_FD_BASE), note, sizeof(note) - 1) == 4);
	CHECK(strcmp(note, "hers") == 0);
	radix_capability_delete(inner);
	radix_capability_delete(mine);
	radix_capability_delete(slot);
}

// Root: builds alice's view, starts her in it, and checks what she left
// and what a view refuses.
static void as_root(void) {
	uint64_t all = rights_of(RADIX_STARTUP_SLOT_ROOT);
	CHECK(all & RADIX_RIGHTS_BYPASS);
	CHECK(mkdir(TREE, 0755) == 0);
	make_dir(TREE "/private", 0700, 0);
	make_dir(SESSION_TMP, 01777, 0);
	make_dir(TREE "/alice", 0700, ALICE);
	make(TREE "/motd", 0666, 0, MOTD);
	make("/tmp/marker", 0644, 0, "the system's /tmp\n");

	// A root stamped for alice, without the bypass right, as authd's for
	// her session will be.
	uint32_t s[4];
	spare(s, 4);
	uint32_t stamped = s[0], hers = s[1], view = s[2], given = s[3];
	CHECK(radix_names_stamp(NAMES, RADIX_STARTUP_SLOT_ROOT, stamped, ALICE, ALICE, NULL, 0) ==
			0);
	CHECK(radix_capability_derive(stamped, hers, all & ~RADIX_RIGHTS_BYPASS, 0) == 0);
	radix_capability_delete(stamped);
	CHECK(radix_directory_make_view(hers, view) == 0);
	const uint64_t made = RADIX_RIGHTS_READ | RADIX_RIGHTS_WRITE | RADIX_RIGHTS_CONTROL |
			RADIX_RIGHTS_COPY | RADIX_RIGHTS_TRANSFER;
	CHECK(rights_of(view) == made);
	const uint64_t rw = RADIX_RIGHTS_READ | RADIX_RIGHTS_WRITE;
	attach_path(view, "system", hers, "system", RADIX_RIGHTS_READ);
	attach_path(view, "devices", hers, "devices", rw);
	attach_path(view, "users/alice", hers, REL "/alice", rw);
	// The session's /tmp is in a directory only root may search: opened
	// through root's root and stamped for alice, as authd will.
	uint32_t tmp[2];
	spare(tmp, 2);
	CHECK(open_in(RADIX_STARTUP_SLOT_ROOT, SESSION_REL, rw, tmp[0]) == 0);
	CHECK(radix_names_stamp(NAMES, tmp[0], tmp[1], ALICE, ALICE, NULL, 0) == 0);
	CHECK(attach(view, "tmp", tmp[1]) == 0);
	radix_capability_delete(tmp[0]);
	radix_capability_delete(tmp[1]);
	// A file, read-only: writable as the system's, but not in the view.
	attach_path(view, "etc/motd", hers, REL "/motd", RADIX_RIGHTS_READ);
	CHECK(attach_link(view, "bin", "system/commands") == 0);

	// What the view refuses.
	uint32_t slot;
	spare(&slot, 1);
	CHECK(open_in(RADIX_STARTUP_SLOT_ROOT, "tmp", rw, slot) == 0);
	// Another name's, and one with the bypass right is no excuse.
	CHECK_STATUS(attach(view, "roots", slot), EPERM);
	radix_capability_delete(slot);
	CHECK(open_in(hers, "tmp", rw, slot) == 0);
	CHECK_STATUS(attach(view, "tmp", slot), EEXIST);
	CHECK_STATUS(attach(view, "tmp/inner", slot), EBUSY);
	CHECK(attach(view, "etc/../x", slot) != 0);
	CHECK_STATUS(attach(view, "v", view), EOPNOTSUPP);
	CHECK_STATUS(attach(hers, "x", slot), EOPNOTSUPP);
	CHECK_STATUS(attach_link(hers, "x", "y"), EOPNOTSUPP);
	int pipes[2];
	CHECK(pipe(pipes) == 0);
	CHECK_STATUS(attach(view, "pipe", RADIX_STARTUP_SLOT_FD_BASE + (uint32_t)pipes[0]),
			EOPNOTSUPP);
	close(pipes[0]);
	close(pipes[1]);
	// A view with the bypass right takes only capabilities with it.
	uint32_t bypassing;
	spare(&bypassing, 1);
	CHECK(radix_directory_make_view(RADIX_STARTUP_SLOT_ROOT, bypassing) == 0);
	CHECK(rights_of(bypassing) == (made | RADIX_RIGHTS_BYPASS));
	uint32_t roots;
	spare(&roots, 1);
	CHECK(radix_names_stamp(NAMES, slot, roots, 0, 0, NULL, 0) == 0);
	CHECK_STATUS(attach(bypassing, "tmp", roots), EPERM);
	radix_capability_delete(roots);
	CHECK(open_in(RADIX_STARTUP_SLOT_ROOT, "tmp", rw, roots) == 0);
	CHECK(attach(bypassing, "tmp", roots) == 0);
	// Nothing is mounted beneath a view; what is opened in it cannot.
	CHECK_STATUS(radix_directory_mount(bypassing, "tmp", 3, "tmpfs", 5, 0, 0), EOPNOTSUPP);
	CHECK_STATUS(radix_directory_unmount(bypassing, "tmp", 3), EOPNOTSUPP);
	uint32_t inner;
	spare(&inner, 1);
	CHECK(open_in(bypassing, "tmp", rw | RADIX_RIGHTS_CONTROL, inner) == 0);
	CHECK(rights_of(inner) == (rw | RADIX_RIGHTS_COPY | RADIX_RIGHTS_TRANSFER |
					  RADIX_RIGHTS_BYPASS));
	radix_capability_delete(inner);
	radix_capability_delete(roots);
	radix_capability_delete(bypassing);
	radix_capability_delete(slot);

	// Alice, in her view, without the right to add to it.
	CHECK(radix_capability_derive(view, given, made & ~RADIX_RIGHTS_CONTROL, 0) == 0);
	char *const argv[] = {"views", "alice", NULL};
	pid_t alice = start_as_alice(given, argv);
	radix_capability_delete(given);
	radix_capability_delete(view);
	radix_capability_delete(hers);
	int status;
	CHECK(waitpid(alice, &status, 0) == alice);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);

	// What she made went where the view put it.
	struct stat st;
	CHECK(stat(SESSION_TMP "/made-by-alice", &st) == 0 && st.st_uid == ALICE);
	CHECK(stat(TREE "/alice/note", &st) == 0 && st.st_uid == ALICE && st.st_size == 4);
	char target[64];
	ssize_t len = readlink(TREE "/alice/motd", target, sizeof(target) - 1);
	CHECK(len == 9 && memcmp(target, "/etc/motd", 9) == 0);
	CHECK(access("/tmp/made-by-alice", F_OK) == -1);
}

int main(int argc, char **argv) {
	if(argc > 1 && strcmp(argv[1], "alice") == 0) {
		check_start("views");
		as_alice();
		return check_end();
	}
	check_start("views");
	as_root();
	return check_end();
}
