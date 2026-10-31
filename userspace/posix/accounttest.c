// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The accounts boot test (ADR 0218). Spark starts it as root with the
// services, authd(8) among them, and the names capability (`--names`).
// It makes two accounts with user(8), alice an administrator and bob with
// an empty password, and checks the files authd writes, their modes and
// the homes, and what getpwnam(3), getgrnam(3), id(1) and `user list`
// read of them. Then it asks authd over its socket as sign-in will: a
// right password passes; a wrong one, or a name there is not, is
// answered the same way and only after a wait, and tries made at once
// wait their turn. Root changes alice's password with passwd(1). Then it
// starts itself as alice, with a root that acts for her and has no bypass
// right: she changes her own password, given hers, but not bob's, makes
// no account, checks no password but hers, and cannot read the hashes or
// change the account files.

#include <fcntl.h>
#include <grp.h>
#include <pwd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>

#include "check.h"
#include "names.h"
#include "programs.h"

// This program, beneath the root.
#define PROGRAM "system/tests/posix/accounttest"
#define USER "/system/commands/user"
#define PASSWD "/system/commands/passwd"
#define ID "/system/commands/id"
#define SOCKET "/system/live/authd/socket"
#define PASSWORDS "/system/state/authd/passwords"
#define ALICE 1000
#define BOB 1001
#define ADMINS 10
// What authd answers a wrong password, and a name there is not.
#define WRONG "no the name or the password is wrong\n"
// The wait before that answer, at first, in milliseconds.
#define WAIT_MS 2000

// What a program wrote to its standard error.
static char errors[4096];

// Runs `argv` given `input`, as run_program does, with what it writes to
// its standard error in `errors`.
static int run_both(const char *const argv[], const char *input) {
	char path[64];
	snprintf(path, sizeof(path), "/tmp/accounttest-%d.err", (int)getpid());
	int fd = open(path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
	int saved = dup(STDERR_FILENO);
	CHECK(fd >= 0 && saved >= 0);
	dup2(fd, STDERR_FILENO);
	int status = run_program(argv, input, strlen(input));
	dup2(saved, STDERR_FILENO);
	close(saved);
	ssize_t got = pread(fd, errors, sizeof(errors) - 1, 0);
	errors[got > 0 ? got : 0] = 0;
	close(fd);
	unlink(path);
	return status;
}

// Checks that `argv` given `input` exits with `status`, writes exactly
// `out`, and writes `err` among its errors.
static void account(const char *file, int line, int status, const char *input, const char *out,
		const char *err, const char *const argv[]) {
	int got = run_both(argv, input);
	if(got != status || strcmp(output, out) || !strstr(errors, err)) {
		char what[1024];
		snprintf(what, sizeof(what), "%s %s exited with %d, wrote \"%.300s\" and \"%.300s\"",
				argv[0], argv[1] ? argv[1] : "", got, output, errors);
		check_failed(file, line, what, 0);
	}
}

#define ACCOUNT(status, input, out, err, ...) \
	account(__FILE__, __LINE__, status, input, out, err, (const char *const[]){__VA_ARGS__, NULL})

// Connects to authd and sends it the request of `len` bytes; the
// connection.
static int send_request(const char *request, size_t len) {
	int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	struct sockaddr_un address = {.sun_family = AF_UNIX};
	strcpy(address.sun_path, SOCKET);
	CHECK(fd >= 0 && connect(fd, (struct sockaddr *)&address, sizeof(address)) == 0);
	CHECK(write(fd, request, len) == (ssize_t)len);
	CHECK(shutdown(fd, SHUT_WR) == 0);
	return fd;
}

// The answer on the connection `fd`, which it closes.
static void read_answer(int fd, char *answer, size_t size) {
	size_t len = 0;
	ssize_t got;
	while(len < size - 1 && (got = read(fd, answer + len, size - 1 - len)) > 0)
		len += got;
	answer[len] = 0;
	close(fd);
}

// A request's text and length: its fields, each ending in NUL.
#define REQUEST(text) text, sizeof(text) - 1

// Checks that authd answers `request` with `expected`, after at least
// `at_least` milliseconds.
static void asks(const char *file, int line, const char *request, size_t len, const char *expected,
		long long at_least) {
	long long start = check_now_ns();
	char answer[256];
	read_answer(send_request(request, len), answer, sizeof(answer));
	long long took = (check_now_ns() - start) / 1000000;
	if(strcmp(answer, expected) || took < at_least) {
		char what[512];
		snprintf(what, sizeof(what), "authd answered \"%.200s\" after %lld ms", answer, took);
		check_failed(file, line, what, 0);
	}
}

#define ASKS(request, expected, at_least) \
	asks(__FILE__, __LINE__, REQUEST(request), expected, at_least)

// Whether the file `path` holds `text`.
static int holds(const char *path, const char *text) {
	static char data[8192];
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if(fd < 0)
		return 0;
	ssize_t len = read(fd, data, sizeof(data) - 1);
	close(fd);
	data[len > 0 ? len : 0] = 0;
	return strstr(data, text) != NULL;
}

// Checks that `path` is a directory or file of `mode`, `owner`'s, in
// `owner`'s group.
static void owned(const char *file, int line, const char *path, mode_t mode, uid_t owner) {
	struct stat st;
	if(stat(path, &st) != 0 || (st.st_mode & 07777) != mode || st.st_uid != owner ||
			st.st_gid != owner) {
		char what[256];
		snprintf(what, sizeof(what), "%s is %o, of %d:%d", path, st.st_mode & 07777,
				(int)st.st_uid, (int)st.st_gid);
		check_failed(file, line, what, errno);
	}
}

#define OWNED(path, mode, owner) owned(__FILE__, __LINE__, path, mode, owner)

// Waits for authd's socket.
static void authd_answers(void) {
	struct stat st;
	long long deadline = check_now_ns() + 60 * 1000000000LL;
	while(stat(SOCKET, &st) != 0 && check_now_ns() < deadline)
		check_sleep_ms(100);
	CHECK(S_ISSOCK(st.st_mode) && (st.st_mode & 0777) == 0666 && st.st_uid == 0);
}

// user(8) makes accounts, and refuses names that are taken or cannot be
// names.
static void making(void) {
	ACCOUNT(0, "wonderland\n", "Made alice, number 1000, home /users/alice\n", "", USER, "add",
			"--admin", "--full-name", "Alice Liddell", "alice");
	// No rules: an empty password is taken, with a warning.
	ACCOUNT(0, "\n", "Made bob, number 1001, home /users/bob\n",
			"With no password, anyone at this machine can sign in as bob.", USER, "add", "bob");
	ACCOUNT(1, "x\n", "", "user: there is an account alice already", USER, "add", "alice");
	ACCOUNT(1, "x\n", "", "user: there is an account root already", USER, "add", "root");
	ACCOUNT(1, "x\n", "", "user: there is a group admins already", USER, "add", "admins");
	ACCOUNT(1, "x\n", "", "user: `Mallory` cannot be a name", USER, "add", "Mallory");
	ACCOUNT(1, "x\n", "", "no colon or line break", USER, "add", "--full-name", "a:b", "carol");
	ACCOUNT(1, "", "", "the standard input ended before a password", USER, "add", "carol");
}

// The account files authd wrote, and the homes it made.
static void files(void) {
	const char *passwd = "/system/settings/passwd";
	CHECK(holds(passwd, "\nalice:x:1000:1000:Alice Liddell:/users/alice:/system/commands/nukesh\n"));
	CHECK(holds(passwd, "\nbob:x:1001:1001::/users/bob:/system/commands/nukesh\n"));
	CHECK(!holds(passwd, "argon2"));
	const char *group = "/system/settings/group";
	CHECK(holds(group, "\nadmins:x:10:alice\n"));
	CHECK(holds(group, "\nalice:x:1000:\nbob:x:1001:\n"));
	OWNED(passwd, 0644, 0);
	OWNED(group, 0644, 0);
	// The hashes, which only authd reads: Argon2id at RFC 9106's second
	// recommendation, and none for an empty password.
	OWNED("/system/state/authd", 0700, 0);
	OWNED(PASSWORDS, 0600, 0);
	CHECK(holds(PASSWORDS, "alice:$argon2id$v=19$m=65536,t=3,p=4$"));
	CHECK(holds(PASSWORDS, "\nbob::\n"));
	CHECK(!holds(PASSWORDS, "wonderland"));
	OWNED("/users/alice", 0700, ALICE);
	OWNED("/users/alice/documents", 0755, ALICE);
	OWNED("/users/alice/applications", 0755, ALICE);
	OWNED("/users/alice/app-data", 0700, ALICE);
	OWNED("/users/bob", 0700, BOB);
}

// What the C library and the tools read of the accounts.
static void reading(void) {
	struct passwd *pw = getpwnam("alice");
	CHECK(pw && pw->pw_uid == ALICE && pw->pw_gid == ALICE);
	CHECK(pw && !strcmp(pw->pw_dir, "/users/alice") && !strcmp(pw->pw_gecos, "Alice Liddell"));
	CHECK(pw && !strcmp(pw->pw_shell, "/system/commands/nukesh"));
	pw = getpwuid(BOB);
	CHECK(pw && !strcmp(pw->pw_name, "bob"));
	struct group *gr = getgrnam("admins");
	CHECK(gr && gr->gr_gid == ADMINS && gr->gr_mem[0] && !strcmp(gr->gr_mem[0], "alice"));
	CHECK(gr && gr->gr_mem[0] && !gr->gr_mem[1]);
	CONTAINS("uid=1000(alice) gid=1000(alice) groups=1000(alice),10(admins)", ID, "alice");
	CONTAINS("uid=1001(bob) gid=1001(bob) groups=1001(bob)", ID, "bob");
	char expected[512];
	snprintf(expected, sizeof(expected),
			"%-16s %6s  %-13s  %-20s %s\n%-16s %6d  %-13s  %-20s %s\n%-16s %6d  %-13s  %s\n",
			"NAME", "NUMBER", "KIND", "HOME", "FULL NAME", "alice", ALICE, "administrator",
			"/users/alice", "Alice Liddell", "bob", BOB, "user", "/users/bob");
	ACCOUNT(0, "", expected, "", USER, "list");
}

// authd's answers on its socket, to root, as sign-in will ask.
static void checking(void) {
	ASKS("check\0alice\0wonderland\0", "ok\n", 0);
	ASKS("check\0bob\0\0", "ok\n", 0);
	// A wrong password, and a name there is not, are answered alike, and
	// only after a wait.
	ASKS("check\0bob\0x\0", WRONG, WAIT_MS);
	ASKS("check\0nobody\0x\0", WRONG, WAIT_MS);
	// Tries at once wait their turn: the second is answered a wait after
	// the first.
	long long start = check_now_ns();
	int first = send_request(REQUEST("check\0alice\0wrong\0"));
	int second = send_request(REQUEST("check\0alice\0wrong again\0"));
	char answer[256];
	read_answer(first, answer, sizeof(answer));
	long long took_first = (check_now_ns() - start) / 1000000;
	CHECK(!strcmp(answer, WRONG) && took_first >= WAIT_MS);
	read_answer(second, answer, sizeof(answer));
	long long took_second = (check_now_ns() - start) / 1000000;
	CHECK(!strcmp(answer, WRONG) && took_second >= 2 * WAIT_MS);
	if(took_first < WAIT_MS || took_second < 2 * WAIT_MS)
		printf("accounttest: the wrong passwords took %lld and %lld ms\n", took_first,
				took_second);
	// The right one still opens it, once its turn comes.
	ASKS("check\0alice\0wonderland\0", "ok\n", 0);
	ASKS("bogus\0", "no a request authd does not know\n", 0);
}

// Root changes a password without the one it had.
static void changing(void) {
	ACCOUNT(0, "looking-glass\n", "", "The password of alice is changed.", PASSWD, "alice");
	ASKS("check\0alice\0looking-glass\0", "ok\n", 0);
	ASKS("check\0alice\0wonderland\0", WRONG, WAIT_MS);
	ACCOUNT(1, "x\n", "", "passwd: there is no account nobody", PASSWD, "nobody");
}

// As alice, with a root that acts for her.
static int alice(void) {
	check_start("accounttest (alice)");
	CHECK(getuid() == ALICE && getgid() == ALICE);
	// Her own password, given hers: from the standard input, the one she
	// has, then the new one.
	ACCOUNT(1, "wrong\nqueen of hearts\n", "", "passwd: the name or the password is wrong",
			PASSWD);
	ACCOUNT(0, "looking-glass\nqueen of hearts\n", "", "The password of alice is changed.",
			PASSWD);
	ACCOUNT(1, "queen of hearts\nx\n", "",
			"passwd: only its owner, or an administrator inside elevate, changes a password",
			PASSWD, "bob");
	ACCOUNT(1, "x\n", "", "user: only an administrator, inside elevate, makes accounts", USER,
			"add", "carol");
	// She checks her own password, as a screen lock does, and no one
	// else's, whether the account is there or not.
	ASKS("check\0alice\0queen of hearts\0", "ok\n", 0);
	const char *only = "no a program checks only the password of the user it runs for\n";
	ASKS("check\0bob\0\0", only, 0);
	ASKS("check\0nobody\0x\0", only, 0);
	// The hashes are root's alone, and the account files only authd's to
	// change.
	CHECK_ERROR(open(PASSWORDS, O_RDONLY | O_CLOEXEC), EACCES);
	CHECK_ERROR(open("/system/settings/passwd", O_WRONLY | O_CLOEXEC), EACCES);
	CHECK(holds("/system/settings/passwd", "\nalice:x:1000:"));
	// Her home is hers, and bob's is closed to her.
	CHECK(access("/users/alice/documents", W_OK) == 0);
	CHECK_ERROR(open("/users/bob", O_RDONLY | O_DIRECTORY | O_CLOEXEC), EACCES);
	return check_end();
}

int main(int argc, char **argv) {
	if(argc == 2 && !strcmp(argv[1], "alice"))
		return alice();
	check_start("accounttest");
	authd_answers();
	RUN(making);
	RUN(files);
	RUN(reading);
	RUN(checking);
	RUN(changing);
	// She notes the commands she runs, for the `commands` test, too.
	int notes = open(COMMANDS_RUN, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
	CHECK(notes >= 0 && fchmod(notes, 0666) == 0);
	close(notes);
	char *const alice_args[] = {"accounttest", "alice", NULL};
	const uint32_t admins[] = {ADMINS};
	pid_t pid = start_as(PROGRAM, ALICE, ALICE, admins, 1, alice_args);
	CHECK_CHILD(pid);
	// What she set opens her account.
	ASKS("check\0alice\0queen of hearts\0", "ok\n", 0);
	return check_end();
}
