// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the logging boot test runs (ADRs 0210 and 0211): logtest waits for
// logd(8), writes lines to the kernel log as a program does, and checks
// what log(1), journalctl(1) and dmesg(1) show of them and of the
// services: their levels, names and process; that a program cannot pass
// its lines off as a service's; that `log -f` shows a line written after
// it started; that logd started again keeps no record twice; that the
// audit log holds Spark's and the kernel's security events and no
// program's; and that logtrim(8) and journalctl's --vacuum-time trim the
// log.

#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sys/stat.h>

#include "check.h"
#include "programs.h"

#define LOG "/system/commands/log"
#define JOURNALCTL "/system/commands/journalctl"
#define DMESG "/system/commands/dmesg"
#define LOGTRIM "/system/commands/logtrim"
#define SERVICE "/system/commands/service"

// Writes `line` to the kernel log, as a program's.
static void kmsg(const char *line) {
	int fd = open("/devices/kmsg", O_WRONLY | O_CLOEXEC);
	CHECK(fd >= 0);
	if(fd < 0)
		return;
	CHECK(write(fd, line, strlen(line)) == (ssize_t)strlen(line));
	close(fd);
}

// Runs `argv` until what it writes holds `text`, for up to `seconds`:
// logd keeps a record a moment after it is written. Returns whether it
// came; `output` holds what the program wrote last.
static int wait_for(const char *text, const char *const argv[], int seconds) {
	long long deadline = check_now_ns() + seconds * 1000000000LL;
	for(;;) {
		if(run_program(argv, "", 0) == 0 && strstr(output, text))
			return 1;
		if(check_now_ns() >= deadline)
			return 0;
		check_sleep_ms(200);
	}
}

// How many times `text` is in `output`.
static int count(const char *text) {
	int n = 0;
	for(const char *at = output; (at = strstr(at, text)); at += strlen(text))
		n++;
	return n;
}

// Whether the records of JSON lines in `output` have rising sequence
// numbers: none of them twice, none out of order.
static int rising(void) {
	long long last = -1;
	int records = 0;
	for(const char *at = output; (at = strstr(at, "\"seq\":")); at++) {
		long long seq = strtoll(at + 6, NULL, 10);
		if(seq <= last)
			return 0;
		last = seq;
		records++;
	}
	return records > 0;
}

// Waits for logd to answer: its socket, then an answer.
static void logd_answers(void) {
	struct stat st;
	long long deadline = check_now_ns() + 60 * 1000000000LL;
	while(stat("/system/live/log/query", &st) != 0 && check_now_ns() < deadline)
		check_sleep_ms(100);
	CHECK(wait_for("spark: ", (const char *const[]){LOG, "-s", "spark", NULL}, 30));
}

// What the log says of the program's own lines: their level, name and
// process, as log(1) and journalctl(1) show them.
static void own_lines(void) {
	kmsg("<4>first line from logtest\n");
	const char *const json[] = {LOG, "logtest", "-o", "json", NULL};
	CHECK(wait_for("first line from logtest", json, 30));
	char process[32];
	snprintf(process, sizeof process, "\"process\":%d", (int)getpid());
	CHECK(strstr(output, "\"level\":\"warning\",\"from\":\"program\",\"name\":\"logtest\""));
	CHECK(strstr(output, process));
	CHECK(rising());
	CONTAINS("logtest[", LOG, "logtest");
	OUTPUT("", "first line from logtest\n", LOG, "logtest", "-g", "^first line", "-o", "cat");
	OUTPUT("", "first line from logtest\n", JOURNALCTL, "-t", "logtest", "-p", "warning", "-o",
			"cat", "-g", "first line|nothing");
	OUTPUT("", "-- No entries --\n", LOG, "logtest", "-p", "err", "-o", "cat", "-g", "first");
	OUTPUT("", "", LOG, "logtest", "-p", "err", "-o", "json", "-g", "first");
	// Control characters, in the text or the thread's name, are shown as
	// `?`: no line of the log sends a terminal an escape sequence, or
	// splits in two.
	CHECK(pthread_setname_np(pthread_self(), "lt\x1b[7m") == 0);
	kmsg("<6>escape \x1b[31m and\nvolumed: a second line\n");
	CHECK(pthread_setname_np(pthread_self(), "logtest") == 0);
	const char *const escaped[] = {LOG, "-g", "escape", "-o", "cat", NULL};
	CHECK(wait_for("escape ?[31m and?volumed: a second line\n", escaped, 30));
	CONTAINS("lt?[7m[", LOG, "-g", "escape", "--no-hostname");
}

// A service's lines are the ones Spark wrote for it: a program that
// writes a line with a service's name in front of it, or names its
// thread after the service, is still the program.
static void services(void) {
	const char *const volumed[] = {LOG, "-s", "volumed", "-o", "cat", NULL};
	CHECK(wait_for("watching for volumes", volumed, 60));
	CONTAINS("\"from\":\"service\",\"name\":\"volumed\"", LOG, "-s", "volumed", "-o", "json");
	CONTAINS("watching for volumes", JOURNALCTL, "-u", "volumed.service", "-b", "-o", "cat");
	CONTAINS("watching for volumes", JOURNALCTL, "_SYSTEMD_UNIT=volumed.service", "-o", "cat");
	kmsg("<3>volumed: forged by logtest\n");
	const char *const own[] = {LOG, "logtest", "-o", "cat", NULL};
	CHECK(wait_for("volumed: forged by logtest", own, 30));
	CHECK(pthread_setname_np(pthread_self(), "volumed") == 0);
	kmsg("<3>forged under the name volumed\n");
	CHECK(pthread_setname_np(pthread_self(), "logtest") == 0);
	char named[64];
	snprintf(named, sizeof named, "volumed[%d]: forged under the name", (int)getpid());
	CHECK(wait_for(named, (const char *const[]){LOG, "volumed", NULL}, 30));
	run_program(volumed, "", 0);
	CHECK(strstr(output, "watching for volumes") && !strstr(output, "forged"));
	run_program((const char *const[]){JOURNALCTL, "-u", "volumed", "-o", "cat", NULL}, "", 0);
	CHECK(strstr(output, "watching for volumes") && !strstr(output, "forged"));
	// The kernel's own records alone.
	CONTAINS("\"from\":\"kernel\"", LOG, "-k", "-n", "20", "-o", "json");
	run_program((const char *const[]){JOURNALCTL, "-k", "-n", "30", "-o", "json", NULL}, "", 0);
	CHECK(strstr(output, "\"from\":\"kernel\"") && !strstr(output, "\"from\":\"program\""));
	CHECK(rising());
	// dmesg(1) tells the kernel's records from programs'.
	CONTAINS("first line from logtest", DMESG, "-u");
	run_program((const char *const[]){DMESG, "-k", NULL}, "", 0);
	CHECK(!strstr(output, "first line from logtest"));
}

// `log -f` shows a line written after it started.
static void follow(void) {
	int out[2];
	CHECK(pipe(out) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		dup2(out[1], STDOUT_FILENO);
		close(out[0]);
		close(out[1]);
		execl(LOG, LOG, "-f", "logtest", "-o", "cat", (char *)NULL);
		_exit(127);
	}
	close(out[1]);
	char seen[8192] = "";
	size_t len = 0;
	int wrote = 0;
	long long deadline = check_now_ns() + 60 * 1000000000LL;
	while(!strstr(seen, "second line from logtest") && check_now_ns() < deadline) {
		seen[len] = 0;
		// It starts with the last ten, the first line among them.
		if(!wrote && strstr(seen, "first line from logtest")) {
			kmsg("<6>second line from logtest\n");
			wrote = 1;
		}
		struct pollfd entry = {out[0], POLLIN, 0};
		if(poll(&entry, 1, 500) <= 0)
			continue;
		ssize_t got = read(out[0], seen + len, sizeof seen - 1 - len);
		if(got <= 0)
			break;
		len += got;
		seen[len] = 0;
	}
	CHECK(wrote);
	CHECK(strstr(seen, "second line from logtest"));
	kill(pid, SIGTERM);
	int status = check_wait(pid);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM);
	close(out[0]);
}

// logd started again goes on after the last record it kept: none twice.
static void restarted(void) {
	STATUS(0, "/system/commands/service", "logd", "restart");
	const char *const logd[] = {LOG, "-s", "logd", "-o", "cat", NULL};
	CHECK(wait_for("keeping the log in /system/state/log, after record", logd, 60));
	kmsg("<6>third line from logtest\n");
	const char *const own[] = {LOG, "logtest", "-o", "json", NULL};
	CHECK(wait_for("third line from logtest", own, 30));
	CHECK(count("first line from logtest") == 1);
	CHECK(count("second line from logtest") == 1);
	CHECK(rising());
	run_program((const char *const[]){LOG, "-n", "30", "-o", "json", NULL}, "", 0);
	CHECK(rising());
}

// The boots and the files the log keeps.
static void kept(void) {
	char boot[64] = "";
	FILE *file = fopen("/system/live/log/boot", "r");
	CHECK(file);
	if(file) {
		CHECK(fgets(boot, sizeof boot, file));
		fclose(file);
	}
	boot[strcspn(boot, "\n")] = 0;
	CHECK(strlen(boot) == 32);
	char line[64];
	snprintf(line, sizeof line, "  0 %s ", boot);
	CONTAINS(line, LOG, "--list-boots");
	CONTAINS("The log takes up ", LOG, "--disk-usage");
	CONTAINS("Archived and active journals take up ", JOURNALCTL, "--disk-usage");
	int days = 0;
	DIR *dir = opendir("/system/state/log");
	CHECK(dir);
	for(struct dirent *entry; dir && (entry = readdir(dir));)
		days += strlen(entry->d_name) == 16 && !strcmp(entry->d_name + 10, ".jsonl");
	if(dir)
		closedir(dir);
	CHECK(days >= 1);
	STATUS(1, LOG, "--since", "soon");
	STATUS(1, LOG, "-f", "-r");
}

// Writes a day file of `name` under `dir` holding one record.
static void day_file(const char *dir, const char *name) {
	char path[128];
	snprintf(path, sizeof path, "%s/%s", dir, name);
	FILE *file = fopen(path, "w");
	CHECK(file);
	if(file) {
		fputs("{\"time\":\"2020-01-01T00:00:00.000000Z\",\"boot\":\"old\",\"seq\":0,"
		      "\"level\":\"info\",\"from\":\"kernel\",\"name\":\"kernel\",\"text\":\"old\"}\n",
				file);
		fclose(file);
	}
}

// Whether `path` is there.
static int exists(const char *path) {
	struct stat st;
	return stat(path, &st) == 0;
}

// The audit log holds the security events of Spark and the kernel, with
// who asked, and none that a program wrote itself.
static void audit(void) {
	const char *const events[] = {LOG, "--audit", "-o", "json", NULL};
	CHECK(wait_for("asked to restart logd, by process ", events, 30));
	CHECK(strstr(output, "\"from\":\"service\",\"name\":\"spark\""));
	CHECK(strstr(output, "\"audit\":true"));
	CONTAINS("asked to restart logd", JOURNALCTL, "_TRANSPORT=audit", "-o", "cat");
	kmsg("<86>forged security event from logtest\n");
	CHECK(wait_for("forged security event", (const char *const[]){LOG, "logtest", NULL}, 30));
	run_program(events, "", 0);
	CHECK(!strstr(output, "forged security event"));
	// The kernel's: its log cleared, with who cleared it.
	STATUS(0, DMESG, "-C");
	CHECK(wait_for("the kernel log was cleared by process ", events, 30));
	CONTAINS("authpriv:info  : kmsg: the kernel log was cleared", DMESG, "-t", "-x", "-f",
			"authpriv");
	CONTAINS(" of it the audit log.", LOG, "--disk-usage");
}

// logtrim and journalctl's --vacuum-time remove day files past their
// days, never today's, and each removal is a security event.
static void trimmed(void) {
	day_file("/system/state/log", "2020-01-01.jsonl");
	day_file("/system/state/log/audit", "2020-01-01.jsonl");
	CONTAINS("Would remove /system/state/log/audit/2020-01-01.jsonl", LOGTRIM, "-n");
	CHECK(strstr(output, "Would remove /system/state/log/2020-01-01.jsonl"));
	CHECK(exists("/system/state/log/2020-01-01.jsonl"));
	CONTAINS("Removed /system/state/log/2020-01-01.jsonl", LOGTRIM);
	CHECK(!exists("/system/state/log/2020-01-01.jsonl"));
	CHECK(!exists("/system/state/log/audit/2020-01-01.jsonl"));
	const char *const events[] = {LOG, "--audit", "-o", "cat", NULL};
	CHECK(wait_for("trimmed the log as its settings say, asked by process ", events, 30));
	// Once, to a limit of its own: the log's files, not the audit log's.
	day_file("/system/state/log", "2026-01-01.jsonl");
	day_file("/system/state/log/audit", "2026-01-01.jsonl");
	CONTAINS("Vacuuming done, freed ", JOURNALCTL, "--vacuum-time=1d");
	CHECK(!exists("/system/state/log/2026-01-01.jsonl"));
	CHECK(exists("/system/state/log/audit/2026-01-01.jsonl"));
	CHECK(wait_for("to the limits process ", events, 30));
	// Settings read again, saying what is wrong.
	FILE *settings = fopen("/system/settings/log", "w");
	CHECK(settings);
	if(settings) {
		fputs("keep-days = 30\nloud = 1\n", settings);
		fclose(settings);
	}
	STATUS(0, SERVICE, "logd", "reload");
	const char *const said[] = {LOG, "-s", "logd", "-o", "cat", NULL};
	CHECK(wait_for("/system/settings/log:2: no setting is called loud", said, 30));
	STATUS(1, LOGTRIM, "--size", "lots");
}

int main(void) {
	check_start("logtest");
	RUN(logd_answers);
	RUN(own_lines);
	RUN(services);
	RUN(follow);
	RUN(restarted);
	RUN(kept);
	RUN(audit);
	RUN(trimmed);
	return check_end();
}
