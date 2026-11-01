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
// change the account files. Last, root changes accounts (ADR 0220): their
// settings; disabling bob ends what runs for him at once, and enabling
// him opens his account again; renaming waits until nothing runs for it,
// and moves its home; removing it retires its number, and keeps,
// archives or deletes its home as asked.

#include <dirent.h>
#include <fcntl.h>
#include <grp.h>
#include <pwd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>

#include "authd.h"
#include "check.h"
#include "names.h"
#include "programs.h"

// This program, beneath the root.
#define PROGRAM "system/tests/posix/accounttest"
#define USER "/system/commands/user"
#define PASSWD "/system/commands/passwd"
#define ID "/system/commands/id"
#define PASSWORDS "/system/state/authd/passwords"
#define RETIRED "/system/state/authd/retired"
// Where bob says he is ready.
#define BOB_READY "/users/bob/ready"
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
			"%-16s %6s  %-13s  %-8s  %-20s %s\n%-16s %6d  %-13s  %-8s  %-20s %s\n"
			"%-16s %6d  %-13s  %-8s  %s\n",
			"NAME", "NUMBER", "KIND", "STATE", "HOME", "FULL NAME", "alice", ALICE,
			"administrator", "enabled", "/users/alice", "Alice Liddell", "bob", BOB, "user",
			"enabled", "/users/bob");
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

// As bob: he makes a child, says he is ready, and waits with it.
static int bob(void) {
	pid_t child = fork();
	CHECK(child >= 0);
	if(child == 0)
		for(;;)
			pause();
	int fd = open(BOB_READY, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0644);
	CHECK(fd >= 0);
	close(fd);
	for(;;)
		pause();
}

// The name of the archive of carol's home in /users/.removed, in `path`.
static int carol_archive(char *path, size_t size) {
	DIR *dir = opendir("/users/.removed");
	if(!dir)
		return 0;
	struct dirent *entry;
	int found = 0;
	while(!found && (entry = readdir(dir))) {
		size_t len = strlen(entry->d_name);
		if(!strncmp(entry->d_name, "carol-", 6) && len > 8 &&
				!strcmp(entry->d_name + len - 8, ".tar.zst"))
			found = snprintf(path, size, "/users/.removed/%s", entry->d_name) < (int)size;
	}
	closedir(dir);
	return found;
}

// Accounts changed, disabled, renamed and removed (ADR 0220).
static void lifecycle(void) {
	// Settings: the full name, and being an administrator, but not
	// taking away the last one's without a yes.
	OUTPUT("", "bob is changed: full name \"Bob Smith\".\n", USER, "set", "--full-name",
			"Bob Smith", "bob");
	CHECK(holds("/system/settings/passwd", "\nbob:x:1001:1001:Bob Smith:/users/bob:"));
	OUTPUT("", "bob is changed: an administrator.\n", USER, "set", "--admin", "bob");
	CHECK(holds("/system/settings/group", "\nadmins:x:10:alice,bob\n"));
	OUTPUT("", "bob is changed: not an administrator.\n", USER, "set", "--no-admin", "bob");
	CHECK(holds("/system/settings/group", "\nadmins:x:10:alice\n"));
	ACCOUNT(1, "", "", "user: nothing changed: say --yes to go on", USER, "set", "--no-admin",
			"alice");
	ACCOUNT(1, "", "", "alice is the last administrator who can sign in.", USER, "disable",
			"alice");
	ACCOUNT(1, "", "", "user: root is not a person's account", USER, "disable", "root");

	// While bob runs something, his account is not renamed; disabled, it
	// all ends at once, and nothing signs in to it until it is enabled.
	char *const bob_args[] = {"accounttest", "bob", NULL};
	pid_t pid = start_as(PROGRAM, BOB, BOB, NULL, 0, bob_args);
	for(int i = 0; i < 1000 && access(BOB_READY, F_OK) != 0; i++)
		check_sleep_ms(10);
	CHECK(access(BOB_READY, F_OK) == 0);
	ACCOUNT(1, "", "", "user: bob is running 2 processes: rename the account once bob has", USER,
			"rename", "bob", "robert");
	OUTPUT("", "bob is disabled: 2 of its processes ended. user enable bob enables it again.\n",
			USER, "disable", "bob");
	int status = 0;
	CHECK(waitpid(pid, &status, 0) == pid && WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
	CHECK(holds(PASSWORDS, "\nbob:!:\n"));
	ASKS("check\0bob\0\0",
			"no the account bob is disabled: an administrator enables it with elevate user "
			"enable bob\n",
			0);
	char line[256];
	snprintf(line, sizeof(line), "%-16s %6d  %-13s  %-8s  %-20s %s\n", "bob", BOB, "user",
			"disabled", "/users/bob", "Bob Smith");
	CONTAINS(line, USER, "list");
	OUTPUT("", "bob is enabled again.\n", USER, "enable", "bob");
	OUTPUT("", "bob was not disabled.\n", USER, "enable", "bob");
	ASKS("check\0bob\0\0", "ok\n", 0);

	// Renamed, with his group and his home.
	OUTPUT("", "Renamed bob to robert, home /users/robert.\n", USER, "rename", "bob", "robert");
	CHECK(holds("/system/settings/passwd", "\nrobert:x:1001:1001:Bob Smith:/users/robert:"));
	CHECK(holds("/system/settings/group", "\nrobert:x:1001:\n"));
	CHECK(holds(PASSWORDS, "\nrobert::\n"));
	OWNED("/users/robert", 0700, BOB);
	CHECK_ERROR(access("/users/bob", F_OK), ENOENT);
	ASKS("check\0robert\0\0", "ok\n", 0);

	// Removed: its number is never given again, and its home goes as
	// asked, which a script must say.
	ACCOUNT(1, "", "", "say what becomes of its home", USER, "remove", "robert");
	OUTPUT("", "robert is removed: its home, /users/robert, is deleted.\n", USER, "remove",
			"--delete-home", "robert");
	CHECK(!holds("/system/settings/passwd", "robert") && !holds(PASSWORDS, "robert"));
	CHECK(!holds("/system/settings/group", "robert"));
	CHECK(holds(RETIRED, "1001:robert:"));
	OWNED(RETIRED, 0600, 0);
	CHECK_ERROR(access("/users/robert", F_OK), ENOENT);
	ACCOUNT(0, "\n", "Made carol, number 1002, home /users/carol\n", "With no password", USER,
			"add", "carol");
	CONTAINS("carol is removed: its home is archived in /users/.removed/carol-", USER, "remove",
			"--archive-home", "carol");
	OWNED("/users/.removed", 0700, 0);
	char archive[256];
	CHECK(carol_archive(archive, sizeof(archive)));
	OWNED(archive, 0600, 0);
	CONTAINS("carol/documents/", "/system/commands/tar", "-tf", archive);
	CHECK_ERROR(access("/users/carol", F_OK), ENOENT);
	// A home kept is still the removed account's, and a new account of
	// the same name is told so.
	ACCOUNT(0, "\n", "Made dave, number 1003, home /users/dave\n", "", USER, "add", "dave");
	OUTPUT("",
			"dave is removed: its home, /users/dave, is kept, its files still number 1003's, "
			"which no account gets again.\n",
			USER, "remove", "--keep-home", "dave");
	OWNED("/users/dave", 0700, 1003);
	ACCOUNT(0, "\n",
			"Made dave, number 1004, home /users/dave; its home was there already, and is user "
			"1003's: `elevate chown -R dave: /users/dave` gives it to dave\n",
			"", USER, "add", "dave");
}

int main(int argc, char **argv) {
	if(argc == 2 && !strcmp(argv[1], "alice"))
		return alice();
	if(argc == 2 && !strcmp(argv[1], "bob")) {
		check_start("accounttest (bob)");
		return bob();
	}
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
	RUN(lifecycle);
	return check_end();
}
