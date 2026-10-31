// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The root half of the sign-in boot test (ADR 0250), beside what `ob
// test` types at the consoles. Spark starts it as root with the
// services. At first the machine has no account, so the console asks
// for its owner's; the test types alice there, an administrator. Once
// she is made, this makes bob, a user's account, as an administrator
// will with user(8), so that the test can sign him in on the second
// console. It then waits for both sessions to start, and checks from
// root's side that each acts for its own user, on its own terminal, as
// the leader of its own session.
//
// `signintest mount`, which bob runs from his shell, checks that his
// session's root holds no control right: he mounts and unmounts nothing.

#include <dirent.h>
#include <pwd.h>
#include <radix/radix.h>
#include <unistd.h>

#include "authd.h"
#include "check.h"

// How long the test may take to type alice's account and both sign-ins.
#define TYPING_MS (10 * 60 * 1000)

// Waits for `name` to have an account, as the passwd file says.
static int has_account(const char *name) {
	long long deadline = check_now_ns() + TYPING_MS * 1000000LL;
	while(check_now_ns() < deadline) {
		if(getpwnam(name))
			return 1;
		check_sleep_ms(200);
	}
	return 0;
}

// The session leaders of user `uid` with a terminal, from `/processes`:
// how many, and the terminal of the last.
static int sessions_of(unsigned uid, char *terminal, size_t size) {
	DIR *dir = opendir("/processes");
	if(!dir)
		return 0;
	int found = 0;
	struct dirent *entry;
	while((entry = readdir(dir))) {
		char path[300], text[2048];
		snprintf(path, sizeof(path), "/processes/%s/status", entry->d_name);
		FILE *file = fopen(path, "r");
		if(!file)
			continue;
		size_t len = fread(text, 1, sizeof(text) - 1, file);
		fclose(file);
		text[len] = 0;
		unsigned user = ~0u;
		long pid = atol(entry->d_name), session = -1;
		char tty[64] = "";
		for(char *line = strtok(text, "\n"); line; line = strtok(NULL, "\n")) {
			sscanf(line, "user: %u", &user);
			sscanf(line, "session: %ld", &session);
			sscanf(line, "terminal: %63s", tty);
		}
		if(user == uid && pid == session && tty[0] && strcmp(tty, "none")) {
			found++;
			snprintf(terminal, size, "%s", tty);
		}
	}
	closedir(dir);
	return found;
}

// Waits for user `uid`'s session to start; its terminal in `terminal`.
static int session_started(unsigned uid, char *terminal, size_t size) {
	long long deadline = check_now_ns() + TYPING_MS * 1000000LL;
	while(check_now_ns() < deadline) {
		if(sessions_of(uid, terminal, size) == 1)
			return 1;
		check_sleep_ms(500);
	}
	return 0;
}

// Run by a person: neither a mount on /tmp, which anyone may walk to and
// write, nor taking /processes away, which the walk would let them do
// with the control right.
static int mount_nothing(void) {
	check_start("signintest-mount");
	CHECK(radix_directory_mount(RADIX_STARTUP_SLOT_ROOT, "tmp", 3, "tmpfs", 5, 0, 0) != 0);
	CHECK(radix_directory_unmount(RADIX_STARTUP_SLOT_ROOT, "processes", 9) != 0);
	CHECK(access("/processes/1/status", R_OK) == 0);
	return check_end();
}

int main(int argc, char **argv) {
	if(argc > 1 && !strcmp(argv[1], "mount"))
		return mount_nothing();
	check_start("signintest");
	authd_answers();
	CHECK(has_account("alice"));
	char answer[256];
	read_answer(send_request(REQUEST("add\0bob\0Bob\0user\0white rabbit\0")), answer,
			sizeof(answer));
	CHECK(!strcmp(answer, "ok made bob, number 1001, home /users/bob\n"));
	printf("signintest: bob is made\n");
	fflush(stdout);
	char alice_tty[64], bob_tty[64];
	CHECK(session_started(1000, alice_tty, sizeof(alice_tty)));
	CHECK(session_started(1001, bob_tty, sizeof(bob_tty)));
	CHECK(strcmp(alice_tty, bob_tty) != 0);
	printf("signintest: alice on %s and bob on %s\n", alice_tty, bob_tty);
	return check_end();
}
