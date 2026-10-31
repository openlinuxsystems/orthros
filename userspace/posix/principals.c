// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The principals boot test (ADR 0214): names on processes and
// capabilities, and the second gate. Spark starts it as root with the
// names capability as descriptor 3 (`--names`). It makes a tree in /tmp
// for two users, alice and bob, then starts itself as bob, who waits, and
// as alice, each with a root that acts for them and has no bypass right,
// as authd will start a user's shell (13b.5). Alice checks that the
// owners, groups and modes of what she meets narrow what her root
// reaches, as on Linux: she cannot read bob's 0700 home, give her files
// away, remove bob's files from a sticky directory, run a file without an
// execute bit, or signal, trace or read more than the status of bob's
// processes. Then root, whose root has the bypass right, checks that it
// reaches everything, and that a capability stamped for bob acts for bob.

#include <dirent.h>
#include <fcntl.h>
#include <grp.h>
#include <radix/radix.h>
#include <stdint.h>
#include <sys/stat.h>

#include "check.h"

#define TREE "/tmp/principals"
// This program, beneath the root.
#define PROGRAM "system/tests/posix/principals"
#define ALICE 1000
#define BOB 1001
// A group alice is in besides her own.
#define STAFF 100
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

// Starts this program with `argv` as `user` in `group` and `groups`, as
// Spark starts a POSIX program, with the test's descriptors 0 to 2, but
// with a root that acts for that name and has no bypass right: made with
// `stamp`, then derived without it. Returns its number.
static pid_t start_as(uint32_t user, uint32_t group, const uint32_t *groups, size_t count,
		char *const argv[]) {
	uint32_t s[4];
	spare(s, 4);
	uint32_t stamped = s[0], root = s[1], program = s[2], child = s[3];
	uint64_t rights = rights_of(RADIX_STARTUP_SLOT_ROOT);
	CHECK(rights & RADIX_RIGHTS_BYPASS);
	CHECK(radix_names_stamp(NAMES, RADIX_STARTUP_SLOT_ROOT, stamped, user, group, groups,
				  count * sizeof(*groups)) == 0);
	CHECK(rights_of(stamped) == rights);
	CHECK(radix_capability_derive(stamped, root, rights & ~RADIX_RIGHTS_BYPASS, 0) == 0);
	struct radix_directory_open_results opened;
	CHECK(radix_directory_open(RADIX_STARTUP_SLOT_ROOT, PROGRAM, strlen(PROGRAM), 0,
				  RADIX_RIGHTS_READ, program, 0, &opened) == 0);
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
	size_t len = exec_block(block, argv);
	CHECK(radix_process_exec(child, program, block, len) == 0);
	// A process that has started keeps its name.
	CHECK_STATUS(radix_names_name_process(NAMES, child, 0, 0, NULL, 0), EINVAL);
	struct radix_process_info_results info;
	CHECK(radix_process_info(child, &info) == 0);
	for(int i = 0; i < 4; i++)
		radix_capability_delete(s[i]);
	return (pid_t)info.number;
}

// Makes the file `path`, holding `text`, with `mode` and the owner and
// group given.
static void make(const char *path, mode_t mode, uid_t owner, gid_t group, const char *text) {
	int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, mode);
	CHECK(fd >= 0);
	CHECK(write(fd, text, strlen(text)) == (ssize_t)strlen(text));
	CHECK(fchown(fd, owner, group) == 0);
	CHECK(fchmod(fd, mode) == 0);
	close(fd);
}

// Makes the directory `path` with `mode` and the owner and group given.
static void make_dir(const char *path, mode_t mode, uid_t owner, gid_t group) {
	CHECK(mkdir(path, 0700) == 0);
	CHECK(chown(path, owner, group) == 0);
	CHECK(chmod(path, mode) == 0);
}

// Whether `path` has the owner, group and permission bits given.
static int has(const char *path, uid_t owner, gid_t group, mode_t mode) {
	struct stat st;
	if(stat(path, &st) != 0)
		return 0;
	if(st.st_uid != owner || st.st_gid != group || (st.st_mode & 07777) != mode) {
		printf("principals: %s is %u:%u %04o\n", path, (unsigned)st.st_uid, (unsigned)st.st_gid,
				(unsigned)(st.st_mode & 07777));
		return 0;
	}
	return 1;
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

// The tree the users meet:
//   alice/          0700 alice    her home
//     foreign       0644 alice:bob's group
//   bob/            0700 bob      his home
//     secret        0600 bob
//   shared/        02775 root:staff
//   sticky/        01777 root
//     bobs          0644 bob
//     open          0666 bob
//   noexec          0644 root     a script without an execute bit
static void setup(void) {
	umask(0);
	make_dir(TREE, 0755, 0, 0);
	make_dir(TREE "/alice", 0700, ALICE, ALICE);
	make(TREE "/alice/foreign", 0644, ALICE, BOB, "");
	make_dir(TREE "/bob", 0700, BOB, BOB);
	make(TREE "/bob/secret", 0600, BOB, BOB, "bob's\n");
	make_dir(TREE "/shared", 02775, 0, STAFF);
	make_dir(TREE "/sticky", 01777, 0, 0);
	make(TREE "/sticky/bobs", 0644, BOB, BOB, "bob's\n");
	make(TREE "/sticky/open", 0666, BOB, BOB, "anyone's\n");
	make(TREE "/noexec", 0644, 0, 0, "#!/bin/sh\nexit 0\n");
	CHECK(has(TREE "/shared", 0, STAFF, 02775));
	CHECK(has(TREE "/bob/secret", BOB, BOB, 0600));
}

// Alice's name: getuid and its kin, and that it cannot change.
static void alice_ids(void) {
	CHECK(getuid() == ALICE && geteuid() == ALICE);
	CHECK(getgid() == ALICE && getegid() == ALICE);
	gid_t groups[4];
	CHECK(getgroups(0, NULL) == 1);
	CHECK(getgroups(4, groups) == 1 && groups[0] == STAFF);
	CHECK(setuid(ALICE) == 0 && setgid(ALICE) == 0 && seteuid(ALICE) == 0);
	CHECK(setgroups(1, groups) == 0);
	CHECK_ERROR(setuid(0), EPERM);
	CHECK_ERROR(setgid(0), EPERM);
	CHECK_ERROR(seteuid(BOB), EPERM);
	CHECK_ERROR(setgroups(0, NULL), EPERM);
	// Her root acts for her, and has no bypass right; the names
	// capability stayed with root.
	CHECK(!(rights_of(RADIX_STARTUP_SLOT_ROOT) & RADIX_RIGHTS_BYPASS));
	CHECK_ERROR(fcntl(3, F_GETFD), EBADF);
}

// What bob keeps to himself.
static void alice_privacy(void) {
	CHECK_ERROR(open(TREE "/bob/secret", O_RDONLY), EACCES);
	struct stat st;
	CHECK_ERROR(stat(TREE "/bob/secret", &st), EACCES);
	CHECK_ERROR(access(TREE "/bob/secret", F_OK), EACCES);
	// Listing his home, or going in, fails as on Linux.
	errno = 0;
	CHECK(opendir(TREE "/bob") == NULL && errno == EACCES);
	CHECK_ERROR(chdir(TREE "/bob"), EACCES);
	// Naming it only, it may be opened, as statvfs does.
	int named = open(TREE "/bob", O_PATH | O_DIRECTORY);
	CHECK(named >= 0);
	close(named);
	CHECK_ERROR(open(TREE "/mine", O_CREAT | O_WRONLY, 0644), EACCES);
	CHECK_ERROR(mkdir(TREE "/bob/mine", 0755), EACCES);
	// Root's tree is hers to read.
	CHECK(stat(TREE "/noexec", &st) == 0 && st.st_uid == 0);
}

// What she may change about her files, and not about bob's.
static void alice_ownership(void) {
	umask(0);
	int fd = open(TREE "/alice/own", O_CREAT | O_EXCL | O_WRONLY, 0640);
	CHECK(fd >= 0);
	close(fd);
	CHECK(has(TREE "/alice/own", ALICE, ALICE, 0640));
	CHECK_ERROR(chown(TREE "/alice/own", BOB, -1), EPERM);
	CHECK_ERROR(chown(TREE "/alice/own", -1, BOB), EPERM);
	CHECK(chown(TREE "/alice/own", ALICE, STAFF) == 0);
	CHECK(chown(TREE "/alice/own", -1, -1) == 0);
	CHECK(chmod(TREE "/alice/own", 0600) == 0);
	CHECK(has(TREE "/alice/own", ALICE, STAFF, 0600));
	// Not a group of hers: the set-group-ID bit goes, as on Linux.
	CHECK(chmod(TREE "/alice/foreign", 02755) == 0);
	CHECK(has(TREE "/alice/foreign", ALICE, BOB, 0755));
	CHECK_ERROR(chmod(TREE "/sticky/bobs", 0666), EPERM);
	CHECK_ERROR(chown(TREE "/sticky/bobs", ALICE, -1), EPERM);
	// A chown that changes nothing changes nothing for anyone, as on Linux.
	CHECK(chown(TREE "/sticky/bobs", -1, -1) == 0);
}

// A sticky directory, and hard links to others' files.
static void alice_sticky(void) {
	CHECK_ERROR(unlink(TREE "/sticky/bobs"), EPERM);
	CHECK_ERROR(unlink(TREE "/sticky/open"), EPERM);
	CHECK_ERROR(rename(TREE "/sticky/open", TREE "/sticky/mine"), EPERM);
	CHECK_ERROR(rename(TREE "/sticky/open", TREE "/alice/open"), EPERM);
	int fd = open(TREE "/sticky/alices", O_CREAT | O_EXCL | O_WRONLY, 0644);
	CHECK(fd >= 0);
	close(fd);
	CHECK(rename(TREE "/sticky/alices", TREE "/sticky/hers") == 0);
	CHECK(unlink(TREE "/sticky/hers") == 0);
	// fs.protected_hardlinks: a link to a file she may not write fails.
	CHECK_ERROR(link(TREE "/sticky/bobs", TREE "/alice/link"), EPERM);
	CHECK(link(TREE "/sticky/open", TREE "/alice/link") == 0);
	CHECK(unlink(TREE "/alice/link") == 0);
}

// utimensat: now with write permission, given times only as the owner.
static void alice_times(void) {
	CHECK(utimensat(AT_FDCWD, TREE "/sticky/open", NULL, 0) == 0);
	struct timespec now[2] = {{0, UTIME_NOW}, {0, UTIME_NOW}};
	CHECK(utimensat(AT_FDCWD, TREE "/sticky/open", now, 0) == 0);
	struct timespec given[2] = {{1000, 0}, {1000, 0}};
	CHECK_ERROR(utimensat(AT_FDCWD, TREE "/sticky/open", given, 0), EPERM);
	CHECK_ERROR(utimensat(AT_FDCWD, TREE "/sticky/bobs", NULL, 0), EACCES);
	CHECK(utimensat(AT_FDCWD, TREE "/alice/own", given, 0) == 0);
	struct stat st;
	CHECK(stat(TREE "/alice/own", &st) == 0 && st.st_mtim.tv_sec == 1000);
}

// access(2) and faccessat(2) ask the same gate.
static void alice_access(void) {
	CHECK(access(TREE "/sticky/bobs", R_OK) == 0);
	CHECK_ERROR(access(TREE "/sticky/bobs", W_OK), EACCES);
	CHECK(access(TREE "/sticky/open", R_OK | W_OK) == 0);
	CHECK_ERROR(access(TREE "/noexec", X_OK), EACCES);
	CHECK(access(TREE "/alice/own", R_OK | W_OK) == 0);
	CHECK(access(TREE "/sticky", W_OK | X_OK) == 0);
	CHECK_ERROR(access(TREE, W_OK), EACCES);
	CHECK_ERROR(faccessat(AT_FDCWD, TREE, 8, 0), EINVAL);
}

// A directory with the set-group-ID bit gives what is made in it its
// group, and its directories that bit.
static void alice_shared(void) {
	umask(0);
	int fd = open(TREE "/shared/notes", O_CREAT | O_EXCL | O_WRONLY, 0664);
	CHECK(fd >= 0);
	close(fd);
	CHECK(has(TREE "/shared/notes", ALICE, STAFF, 0664));
	CHECK(mkdir(TREE "/shared/plans", 0775) == 0);
	CHECK(has(TREE "/shared/plans", ALICE, STAFF, 02775));
}

// exec needs an execute bit; a program others may run runs for her.
static void alice_running(void) {
	pid_t child = check_fork();
	if(!child) {
		execl(TREE "/noexec", "noexec", (char *)NULL);
		CHECK(errno == EACCES);
		check_exit_child();
	}
	CHECK_CHILD(child);
	child = check_fork();
	if(!child) {
		execl("/system/commands/true", "true", (char *)NULL);
		_exit(127);
	}
	CHECK_CHILD(child);
}

// Bob's processes and root's: not hers to signal or trace, and only their
// status hers to read.
static void alice_processes(pid_t bob, pid_t root) {
	CHECK_ERROR(kill(bob, 0), EPERM);
	CHECK_ERROR(kill(bob, SIGTERM), EPERM);
	CHECK_ERROR(kill(root, 0), EPERM);
	pid_t child = check_fork();
	if(!child) {
		for(;;)
			pause();
	}
	CHECK(kill(child, SIGKILL) == 0);
	int status = check_wait(child);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
	uint32_t trace;
	spare(&trace, 1);
	CHECK_STATUS(radix_process_trace(RADIX_STARTUP_SLOT_PROCESS, (uint64_t)bob, 0, trace), EPERM);
	CHECK_STATUS(radix_process_trace(RADIX_STARTUP_SLOT_PROCESS, (uint64_t)root, 0, trace),
			EPERM);

	char path[64], text[2048];
	snprintf(path, sizeof(path), "/processes/%d/status", (int)bob);
	CHECK(read_text(path, text, sizeof(text)) > 0);
	CHECK(strstr(text, "\nuser: 1001\n") != NULL);
	snprintf(path, sizeof(path), "/processes/%d/cmdline", (int)bob);
	CHECK_ERROR(read_text(path, text, sizeof(text)), EACCES);
	snprintf(path, sizeof(path), "/processes/%d/files", (int)bob);
	CHECK_ERROR(read_text(path, text, sizeof(text)), EACCES);
	snprintf(path, sizeof(path), "/processes/%d", (int)bob);
	CHECK(has(path, BOB, BOB, 0555));
	snprintf(path, sizeof(path), "/processes/%d/cmdline", (int)getpid());
	CHECK(read_text(path, text, sizeof(text)) > 0 && strcmp(text, "principals") == 0);
	snprintf(path, sizeof(path), "/processes/%d", (int)getpid());
	CHECK(has(path, ALICE, ALICE, 0555));
}

static int alice(pid_t bob, pid_t root) {
	RUN(alice_ids);
	RUN(alice_privacy);
	RUN(alice_ownership);
	RUN(alice_sticky);
	RUN(alice_times);
	RUN(alice_access);
	RUN(alice_shared);
	RUN(alice_running);
	alice_processes(bob, root);
	fflush(stdout);
	return check_failures > 125 ? 125 : check_failures;
}

static void on_term(int signal) {
	(void)signal;
	_exit(check_failures > 125 ? 125 : check_failures);
}

// Bob checks his name, leaves a note in his home for root to see that he
// is ready, and waits for root's SIGTERM.
static int bob(void) {
	struct sigaction action;
	memset(&action, 0, sizeof(action));
	action.sa_handler = on_term;
	CHECK(sigaction(SIGTERM, &action, NULL) == 0);
	CHECK(getuid() == BOB && getgid() == BOB && getgroups(0, NULL) == 0);
	umask(022);
	int fd = open(TREE "/bob/notes", O_CREAT | O_EXCL | O_WRONLY, 0666);
	CHECK(fd >= 0);
	close(fd);
	fflush(stdout);
	for(;;)
		pause();
}

// Root's root has the bypass right: it reaches what the others cannot.
// A capability stamped for bob acts for him: with the bypass right too it
// reaches alice's files, and what it makes is his; without, it does not.
static void root_reaches(void) {
	CHECK(has(TREE "/bob/notes", BOB, BOB, 0644));
	char text[64];
	CHECK(read_text(TREE "/bob/secret", text, sizeof(text)) > 0);
	CHECK(read_text(TREE "/alice/own", text, sizeof(text)) == 0);
	CHECK(unlink(TREE "/sticky/bobs") == 0);

	uint32_t s[3];
	spare(s, 3);
	uint32_t as_bob = s[0], plain = s[1], file = s[2];
	CHECK(radix_names_stamp(NAMES, RADIX_STARTUP_SLOT_ROOT, as_bob, BOB, BOB, NULL, 0) == 0);
	struct radix_directory_open_results opened;
	const char *own = TREE "/alice/own";
	CHECK(radix_directory_open(as_bob, own + 1, strlen(own + 1), 0, RADIX_RIGHTS_READ, file, 0,
				  &opened) == 0);
	radix_capability_delete(file);
	const char *made = TREE "/made";
	CHECK(radix_directory_open(as_bob, made + 1, strlen(made + 1), RADIX_OPEN_FLAGS_CREATE,
				  RADIX_RIGHTS_READ | RADIX_RIGHTS_WRITE, file, 0640, &opened) == 0);
	radix_capability_delete(file);
	CHECK(has(made, BOB, BOB, 0640));
	CHECK(radix_capability_derive(as_bob, plain, rights_of(as_bob) & ~RADIX_RIGHTS_BYPASS, 0) ==
			0);
	CHECK_STATUS(radix_directory_open(plain, own + 1, strlen(own + 1), 0, RADIX_RIGHTS_READ,
						 file, 0, &opened),
			EACCES);
	const char *secret = TREE "/bob/secret";
	CHECK(radix_directory_open(plain, secret + 1, strlen(secret + 1), 0, RADIX_RIGHTS_READ, file,
				  0, &opened) == 0);
	// A name has GROUPS_MAX other groups at most.
	uint32_t groups[RADIX_NAMES_GROUPS_MAX + 1] = {0};
	CHECK_STATUS(radix_names_stamp(NAMES, RADIX_STARTUP_SLOT_ROOT, file, 1, 1, groups,
						 sizeof(groups)),
			EINVAL);
	for(int i = 0; i < 3; i++)
		radix_capability_delete(s[i]);
}

int main(int argc, char **argv) {
	check_start("principals");
	if(argc == 4 && strcmp(argv[1], "alice") == 0)
		return alice((pid_t)atoi(argv[2]), (pid_t)atoi(argv[3]));
	if(argc == 2 && strcmp(argv[1], "bob") == 0)
		return bob();
	struct radix_capability_info_results info;
	if(radix_capability_info(NAMES, &info) != 0 || info.kind != RADIX_OBJECT_KIND_NAMES) {
		printf("principals: FAILED: descriptor 3 is not the names capability (spark --names)\n");
		return 1;
	}
	CHECK(getuid() == 0 && getgid() == 0 && getgroups(0, NULL) == 0);
	RUN(setup);

	char *bob_args[] = {"principals", "bob", NULL};
	pid_t bob_pid = start_as(BOB, BOB, NULL, 0, bob_args);
	// Bob is ready when his note is there.
	for(int i = 0; i < 1000 && access(TREE "/bob/notes", F_OK) != 0; i++)
		check_sleep_ms(10);
	CHECK(access(TREE "/bob/notes", F_OK) == 0);

	char bob_number[24], root_number[24];
	snprintf(bob_number, sizeof(bob_number), "%d", (int)bob_pid);
	snprintf(root_number, sizeof(root_number), "%d", (int)getpid());
	char *alice_args[] = {"principals", "alice", bob_number, root_number, NULL};
	const uint32_t staff[] = {STAFF};
	pid_t alice_pid = start_as(ALICE, ALICE, staff, 1, alice_args);
	CHECK_CHILD(alice_pid);
	// Bob was started by root, so root may stop him.
	CHECK(kill(bob_pid, SIGTERM) == 0);
	CHECK_CHILD(bob_pid);

	RUN(root_reaches);
	return check_end();
}
