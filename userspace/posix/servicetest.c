// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the services boot test runs (ADR 0216): service files in TOML.
// servicetest checks every shipped file with `service check`; writes a
// file of before Phase 13 and has Spark convert it, comments and all, as
// it starts the service; writes a new one with `service new` and edits it
// with `service edit`, which puts a file in place only once it reads; and
// breaks a file under a running service, which Spark then runs from its
// last good copy. Then (ADR 0217) it gives a service a secret with
// `service secret` and starts it, as `servicetest --inside`, in the view
// Spark builds of what its file declares, which it checks from inside.
// Run as `servicetest --editor good|bad FILE`, it is the editor `service
// edit` runs.

#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

#include "check.h"
#include "programs.h"

#define SERVICE "/system/commands/service"
#define DIR "/system/settings/spark/services"
#define GOOD "/system/state/spark/good"
#define SELF "/system/tests/posix/servicetest"

// The demo service as `service edit` leaves it.
static const char DEMO[] = "service_version = 1\n"
		"description = \"A demo\"\n"
		"program = \"/system/commands/nukesh\"\n"
		"arguments = [\"-c\", \"echo demo up; while :; do sleep 1; done\"]\n"
		"restart = \"always\"\n";

// The same, broken on line 5.
static const char BROKEN[] = "service_version = 1\n"
		"description = \"A demo\"\n"
		"program = \"/system/commands/nukesh\"\n"
		"arguments = [\"-c\", \"echo demo up; while :; do sleep 1; done\"]\n"
		"restart = \"sometimes\"\n";

// A file of before Phase 13 (ADR 0101), with comments.
static const char LEGACY[] = "# The legacy service: written as Phases 9 to 12 did.\n"
		"\n"
		"description = An old one\n"
		"program = /system/commands/nukesh\n"
		"arguments = -c \"echo legacy up; while :; do sleep 1; done\"\n"
		"# Never again once it ends.\n"
		"restart = never\n"
		"memory = 8M\n";

// The service that checks its view from inside: it reads its secret and
// writes in its own folder, and nothing more.
static const char INSIDE[] = "service_version = 1\n"
		"description = \"Checks its view\"\n"
		"program = \"" SELF "\"\n"
		"arguments = [\"--inside\"]\n"
		"restart = \"never\"\n"
		"secrets = [\"token\"]\n"
		"\n"
		"[permissions]\n"
		"files.read-write = [\"/system/state/inside\"]\n";

// Its own folder, where it says how its checks went.
#define OWN "/system/state/inside"
#define SECRETS "/system/state/spark/secrets/inside"

extern char **environ;

// Writes `text` to `path`.
static void write_file(const char *path, const char *text) {
	int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
	CHECK(fd >= 0);
	if(fd < 0)
		return;
	CHECK(write(fd, text, strlen(text)) == (ssize_t)strlen(text));
	close(fd);
}

// The text of `path`, in `buffer` of `size` bytes, NUL-terminated; empty
// if it cannot be read.
static const char *read_file(const char *path, char *buffer, size_t size) {
	buffer[0] = 0;
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if(fd < 0)
		return buffer;
	ssize_t got = read(fd, buffer, size - 1);
	buffer[got > 0 ? got : 0] = 0;
	close(fd);
	return buffer;
}

// Whether `path` is there.
static int exists(const char *path) {
	struct stat st;
	return stat(path, &st) == 0;
}

// Waits up to `seconds` for `service NAME status` to say it runs.
static int running(const char *name, int seconds) {
	long long deadline = check_now_ns() + seconds * 1000000000LL;
	for(;;) {
		if(run_program((const char *const[]){SERVICE, name, "status", NULL}, "", 0) == 0)
			return 1;
		if(check_now_ns() >= deadline)
			return 0;
		check_sleep_ms(200);
	}
}

// The editor `service edit` runs: `--editor good FILE` writes DEMO to
// FILE; `--editor bad FILE` writes BROKEN; `--editor bad-then-good FILE`
// writes BROKEN the first time and DEMO the next, as a person fixing
// their mistake.
static int editor(const char *how, const char *file) {
	const char *text = DEMO;
	if(!strcmp(how, "bad")) {
		text = BROKEN;
	} else if(!strcmp(how, "bad-then-good")) {
		const char *mark = "/tmp/servicetest-edited";
		text = exists(mark) ? DEMO : BROKEN;
		write_file(mark, "");
	}
	write_file(file, text);
	return 0;
}

// Every shipped file reads, and `service check` says so.
static void shipped(void) {
	const char *names[] = {"consoled", "diskhealth", "logd", "notifyd", "volumed"};
	CONTAINS(DIR "/logd.toml: ok\n", SERVICE, "check");
	for(size_t i = 0; i < sizeof(names) / sizeof(names[0]); i++) {
		char line[128];
		snprintf(line, sizeof(line), DIR "/%s.toml: ok\n", names[i]);
		CHECK(strstr(output, line) != NULL);
		// Spark kept each as its last good copy.
		char good[128], text[4096], kept[4096];
		snprintf(good, sizeof(good), GOOD "/%s.toml", names[i]);
		snprintf(line, sizeof(line), DIR "/%s.toml", names[i]);
		CHECK(read_file(good, kept, sizeof(kept))[0] != 0);
		CHECK(!strcmp(read_file(line, text, sizeof(text)), kept));
	}
	OUTPUT("", DIR "/notifyd.toml: ok\n", SERVICE, "check", "notifyd");
	// A name is never a path.
	STATUS(1, SERVICE, "check", "../logd");
	STATUS(1, SERVICE, "check", "/system/settings/spark/services/nothing.toml");
}

// Spark converts a file of before Phase 13 as it starts its service.
static void converted(void) {
	write_file(DIR "/legacy.service", LEGACY);
	CONTAINS("legacy.service: a file of before Phase 13", SERVICE, "check");
	STATUS(0, SERVICE, "legacy", "start");
	CHECK(running("legacy", 10));
	CHECK(!exists(DIR "/legacy.service"));
	char text[4096];
	CHECK(!strcmp(read_file(DIR "/legacy.service.old", text, sizeof(text)), LEGACY));
	const char *toml = read_file(DIR "/legacy.toml", text, sizeof(text));
	CHECK(strstr(toml, "# The legacy service: written as Phases 9 to 12 did.\n\nservice_version = 1\n"));
	CHECK(strstr(toml, "description = \"An old one\"\n"));
	CHECK(strstr(toml, "arguments = [\"-c\", \"echo legacy up; while :; do sleep 1; done\"]\n"));
	CHECK(strstr(toml, "# Never again once it ends.\nrestart = \"never\"\n"));
	CHECK(strstr(toml, "\n[limits]\nmemory = \"8M\"\n"));
	OUTPUT("", DIR "/legacy.toml: ok\n", SERVICE, "check", "legacy");
	STATUS(0, SERVICE, "legacy", "stop");
}

// `service new` writes the template, once; `service edit` puts the file
// in place only once it reads.
static void written(void) {
	OUTPUT("", "wrote " DIR "/demo.toml: edit it with `service edit demo`, then start it with "
			"`service demo start`\n", SERVICE, "new", "demo");
	OUTPUT("", DIR "/demo.toml: ok\n", SERVICE, "check", "demo");
	STATUS(1, SERVICE, "new", "demo");
	STATUS(1, SERVICE, "new", "../demo");
	STATUS(1, SERVICE, "new", ".demo");
	char template[4096], text[4096];
	read_file(DIR "/demo.toml", template, sizeof(template));
	CHECK(strstr(template, "# The demo service.") == template);
	CHECK(strstr(template, "enabled = false\n"));

	// Nothing changed.
	setenv("EDITOR", "true", 1);
	OUTPUT("", DIR "/demo.toml: no change\n", SERVICE, "edit", "demo");
	// A broken edit, given up on: the file is as it was.
	setenv("EDITOR", SELF " --editor bad", 1);
	RUN_WITH(1, "x\n", "", 0, SERVICE, "edit", "demo");
	CHECK(!strcmp(read_file(DIR "/demo.toml", text, sizeof(text)), template));
	// A broken edit, edited again and fixed.
	setenv("EDITOR", SELF " --editor bad-then-good", 1);
	OUTPUT("e\n", DIR "/demo.toml: ok; Spark reads it when the service next starts: `service "
			"demo restart`\n", SERVICE, "edit", "demo");
	CHECK(!strcmp(read_file(DIR "/demo.toml", text, sizeof(text)), DEMO));
	CHECK(!exists(DIR "/.demo.toml.edit"));
	struct stat st;
	CHECK(stat(DIR "/demo.toml", &st) == 0 && (st.st_mode & 07777) == 0644);
	// An edit going on already, or of no file.
	write_file(DIR "/.demo.toml.edit", "");
	STATUS(1, SERVICE, "edit", "demo");
	unlink(DIR "/.demo.toml.edit");
	STATUS(1, SERVICE, "edit", "nothing");
	unsetenv("EDITOR");

	// Spark reads a file added since boot as its service first starts.
	STATUS(0, SERVICE, "demo", "start");
	CHECK(running("demo", 10));
	CHECK(!strcmp(read_file(GOOD "/demo.toml", text, sizeof(text)), DEMO));
	// One that does not read is not started.
	write_file(DIR "/fresh.toml", BROKEN);
	STATUS(1, SERVICE, "fresh", "start");
	// LSB: 4 is a service that is not there.
	STATUS(4, SERVICE, "fresh", "status");
}

// A file broken under a running service leaves its last good copy
// running.
static void broken(void) {
	write_file(DIR "/demo.toml", BROKEN);
	RUN_WITH(1, "", "", 0, SERVICE, "check", "demo");
	STATUS(0, SERVICE, "demo", "restart");
	check_sleep_ms(500);
	CHECK(running("demo", 10));
	char text[4096];
	CHECK(!strcmp(read_file(GOOD "/demo.toml", text, sizeof(text)), DEMO));
	// Fixed, it is the last good copy again.
	write_file(DIR "/demo.toml", DEMO);
	STATUS(0, SERVICE, "demo", "stop");
}

// What the inside service checks: that its view holds what every service
// reads, its own folder, its secret and a /tmp of its own, and nothing
// past them; and that it acts for root without the bypass right.
static void inside(void) {
	// Its secret is a file, read-only, and in no variable.
	const char *dir = getenv("SECRETS_DIRECTORY");
	CHECK(dir && !strcmp(dir, SECRETS));
	char text[64];
	CHECK(!strcmp(read_file(SECRETS "/token", text, sizeof(text)), "s3cret"));
	for(char **variable = environ; *variable; variable++)
		CHECK(!strstr(*variable, "s3cret"));
	CHECK_ERROR(open(SECRETS "/other", O_WRONLY | O_CREAT | O_CLOEXEC, 0600), EROFS);
	CHECK(!exists("/system/state/spark/good"));

	// What every service reads, read-only, and the root's links and
	// /system's, so that #!/usr/bin/env finds env.
	CHECK(exists("/system/settings/passwd"));
	CHECK_ERROR(open("/system/settings/inside", O_WRONLY | O_CREAT | O_CLOEXEC, 0644), EROFS);
	CHECK(access("/usr/bin/env", X_OK) == 0);
	CHECK(access("/bin/sh", X_OK) == 0);
	char path[64];
	snprintf(path, sizeof(path), "/processes/%d/status", getpid());
	CHECK(exists(path));
	CHECK(exists("/kernel/block"));
	CHECK(exists("/devices/null") && exists("/devices/urandom"));
	CHECK(exists("/system/live/notify"));

	// Nothing past them.
	const char *gone[] = {"/users", "/shared", "/volumes", "/applications", "/devices/kmsg",
			"/system/state/log", "/system/live/spark", "/system/live/log", "/system/boot"};
	for(size_t i = 0; i < sizeof(gone) / sizeof(gone[0]); i++) {
		if(exists(gone[i]))
			printf("inside: %s is in its view\n", gone[i]);
		CHECK(!exists(gone[i]));
	}

	// Its own folder, and a /tmp of its own.
	write_file(OWN "/written", "yes");
	write_file("/tmp/mark", "inside");
	CHECK(exists("/tmp/mark"));

	// Root's file of mode 000 is closed to it: it has no bypass right.
	CHECK_ERROR(open(OWN "/locked", O_RDONLY | O_CLOEXEC), EACCES);
}

// Waits up to `seconds` for `path` to be there.
static int appears(const char *path, int seconds) {
	long long deadline = check_now_ns() + seconds * 1000000000LL;
	while(!exists(path)) {
		if(check_now_ns() >= deadline)
			return 0;
		check_sleep_ms(200);
	}
	return 1;
}

// A service in the view its file declares, with a secret.
static void viewed(void) {
	write_file(DIR "/inside.toml", INSIDE);
	CHECK(mkdir(OWN, 0755) == 0);
	write_file(OWN "/locked", "closed");
	CHECK(chmod(OWN "/locked", 0) == 0);
	// Its secret is not set yet, so it does not start.
	STATUS(1, SERVICE, "inside", "start");
	CHECK(!exists(OWN "/done"));

	OUTPUT("s3cret\n", "set inside's secret token; inside reads it when it next starts: `service "
			"inside restart`\n", SERVICE, "secret", "set", "inside", "token");
	OUTPUT("", "token\n", SERVICE, "secret", "list", "inside");
	struct stat st;
	CHECK(stat(SECRETS "/token", &st) == 0 && (st.st_mode & 07777) == 0600);
	CHECK(stat(SECRETS, &st) == 0 && (st.st_mode & 07777) == 0700);
	char text[64];
	CHECK(!strcmp(read_file(SECRETS "/token", text, sizeof(text)), "s3cret"));
	STATUS(1, SERVICE, "secret", "set", "inside", "../token");
	STATUS(1, SERVICE, "secret", "remove", "inside", "nothing");

	STATUS(0, SERVICE, "inside", "start");
	CHECK(appears(OWN "/done", 20));
	CHECK(!strcmp(read_file(OWN "/done", text, sizeof(text)), "every check passed"));
	CHECK(!strcmp(read_file(OWN "/written", text, sizeof(text)), "yes"));
	// Its /tmp is its own.
	CHECK(!strcmp(read_file("/system/live/spark/tmp/inside/mark", text, sizeof(text)), "inside"));
	CHECK(!exists("/tmp/mark"));

	OUTPUT("", "", SERVICE, "secret", "remove", "inside", "token");
	OUTPUT("", "", SERVICE, "secret", "list", "inside");
}

int main(int argc, char **argv) {
	if(argc == 4 && !strcmp(argv[1], "--editor"))
		return editor(argv[2], argv[3]);
	if(argc == 2 && !strcmp(argv[1], "--inside")) {
		check_start("inside");
		RUN(inside);
		int status = check_end();
		write_file(OWN "/done", status ? "failed" : "every check passed");
		return status;
	}
	check_start("servicetest");
	RUN(shipped);
	RUN(converted);
	RUN(written);
	RUN(broken);
	RUN(viewed);
	return check_end();
}
