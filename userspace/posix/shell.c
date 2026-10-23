// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The shell, Nukesh (userspace/programs/nukesh), on Radix: the scripts of
// its test suite, run as its host test runs them, each giving what bash
// gives (or dash, for the posix-* ones, run as /bin/sh); then the shell
// at a terminal, both with its plain line reader (TERM=dumb) and with its
// line editor, typed at through a pseudo-terminal: commands, job control
// with ^Z and ^C, completion, history and the exit status.

#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <termios.h>

#include "check.h"

// Where the scripts and their .out files are (ob puts them there), and
// the shell by its two names; other places can be given, to try this
// program on another system.
#ifndef CASES
#define CASES "/system/tests/shell"
#endif
#ifndef NUKESH
#define NUKESH "/system/bin/nukesh"
#endif
#ifndef SH
#define SH "/bin/sh"
#endif

// The environment the scripts run with, as on the host.
static char *const ENVIRONMENT[] = {"PATH=/bin", "HOME=/root", "TERM=dumb", NULL};

// What a script wrote and its status, as its .out file has them.
static char output[65536];

// Runs `shell` on `script` in /tmp and puts what it wrote to its standard
// output, then "status=N", in `output`.
static void run_script(const char *shell, const char *script) {
	int out[2];
	CHECK(pipe(out) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		int null = open("/dev/null", O_RDWR);
		dup2(null, STDIN_FILENO);
		dup2(out[1], STDOUT_FILENO);
		dup2(null, STDERR_FILENO);
		close(out[0]);
		close(out[1]);
		close(null);
		// Its own group, so that the jobs it starts and kills are its own.
		setpgid(0, 0);
		if(chdir("/tmp") < 0)
			_exit(126);
		char *const argv[] = {(char *)shell, (char *)script, NULL};
		execve(shell, argv, ENVIRONMENT);
		_exit(127);
	}
	close(out[1]);
	size_t len = 0;
	ssize_t got;
	while(len < sizeof(output) - 32 &&
			(got = read(out[0], output + len, sizeof(output) - 32 - len)) != 0) {
		if(got < 0 && errno == EINTR)
			continue;
		if(got < 0)
			break;
		len += (size_t)got;
	}
	close(out[0]);
	int status = check_wait(pid);
	int code = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
	snprintf(output + len, 32, "status=%d\n", code);
}

// The contents of `path`, NUL-ended, in a buffer the caller frees.
static char *read_file(const char *path) {
	int fd = open(path, O_RDONLY);
	if(fd < 0)
		return NULL;
	size_t size = 0, cap = 4096;
	char *text = malloc(cap);
	ssize_t got;
	while(text && (got = read(fd, text + size, cap - size - 1)) > 0) {
		size += (size_t)got;
		if(cap - size < 2) {
			cap *= 2;
			text = realloc(text, cap);
		}
	}
	close(fd);
	if(text)
		text[size] = 0;
	return text;
}

// Says where `got` first differs from `expected`, by line.
static void show_difference(const char *name, const char *expected, const char *got) {
	int line = 1;
	const char *e = expected, *g = got;
	while(*e && *e == *g) {
		if(*e == '\n')
			line++;
		e++;
		g++;
	}
	// Back to the start of the line.
	while(e > expected && e[-1] != '\n') {
		e--;
		g--;
	}
	printf("shell: %s: line %d: expected \"%.*s\", got \"%.*s\"\n", name, line,
			(int)strcspn(e, "\n"), e, (int)strcspn(g, "\n"), g);
}

static int by_name(const struct dirent **a, const struct dirent **b) {
	return strcmp((*a)->d_name, (*b)->d_name);
}

static int is_script(const struct dirent *entry) {
	size_t len = strlen(entry->d_name);
	return len > 3 && !strcmp(entry->d_name + len - 3, ".sh");
}

// Each script of the suite.
static void scripts(void) {
	struct dirent **entries;
	int count = scandir(CASES, &entries, is_script, by_name);
	CHECK(count > 0);
	for(int i = 0; i < count; i++) {
		const char *name = entries[i]->d_name;
		char script[512], expected_path[512];
		snprintf(script, sizeof(script), CASES "/%s", name);
		snprintf(expected_path, sizeof(expected_path), CASES "/%.*s.out",
				(int)strlen(name) - 3, name);
		const char *shell = strncmp(name, "posix-", 6) ? NUKESH : SH;
		char *expected = read_file(expected_path);
		CHECK(expected != NULL);
		run_script(shell, script);
		if(expected && strcmp(expected, output)) {
			show_difference(name, expected, output);
			check_failed(__FILE__, __LINE__, name, 0);
		} else {
			printf("shell: %s matches\n", name);
		}
		free(expected);
		free(entries[i]);
	}
	free(entries);
}

// A shell at a terminal, and what it has shown.
struct session {
	int master;
	pid_t pid;
	char shown[65536];
	size_t len;
	// Where the next expect looks from.
	size_t from;
};

// Starts the shell interactively on a new pseudo-terminal with `term`.
static int start(struct session *s, const char *term) {
	s->len = s->from = 0;
	s->master = posix_openpt(O_RDWR | O_NOCTTY);
	if(s->master < 0 || grantpt(s->master) < 0 || unlockpt(s->master) < 0)
		return -1;
	struct winsize size = {.ws_row = 24, .ws_col = 80};
	ioctl(s->master, TIOCSWINSZ, &size);
	char name[64];
	if(ptsname_r(s->master, name, sizeof(name)))
		return -1;
	s->pid = check_fork();
	if(s->pid == 0) {
		setsid();
		// Without O_NOCTTY: it becomes the controlling terminal.
		int tty = open(name, O_RDWR);
		if(tty < 0)
			_exit(126);
		dup2(tty, STDIN_FILENO);
		dup2(tty, STDOUT_FILENO);
		dup2(tty, STDERR_FILENO);
		close(tty);
		close(s->master);
		signal(SIGTTOU, SIG_DFL);
		signal(SIGTTIN, SIG_DFL);
		signal(SIGTSTP, SIG_DFL);
		if(chdir("/tmp") < 0)
			_exit(126);
		char termvar[32];
		snprintf(termvar, sizeof(termvar), "TERM=%s", term);
		char *const env[] = {"PATH=/bin", "HOME=/tmp", "PS1=$ ", termvar, NULL};
		char *const argv[] = {NUKESH, "--norc", "-i", NULL};
		execve(argv[0], argv, env);
		_exit(127);
	}
	return 0;
}

// Reads what the shell shows for up to `ms` milliseconds, answering the
// line editor's questions for the cursor's place.
static void pump(struct session *s, int ms) {
	struct pollfd entry = {.fd = s->master, .events = POLLIN};
	while(s->len < sizeof(s->shown) - 1 && poll(&entry, 1, ms) == 1 &&
			(entry.revents & POLLIN)) {
		ssize_t n = read(s->master, s->shown + s->len, sizeof(s->shown) - 1 - s->len);
		if(n <= 0)
			break;
		s->shown[s->len + n] = 0;
		for(char *q = s->shown + s->len; (q = strstr(q, "\033[6n")); q += 4)
			CHECK(write(s->master, "\033[1;1R", 6) == 6);
		s->len += (size_t)n;
		ms = 50;
	}
	s->shown[s->len] = 0;
}

// Waits up to five seconds for the shell to show `text` after what the
// last expect found; true if it did.
static int wait_for(struct session *s, const char *text) {
	for(int i = 0; i < 100; i++) {
		char *found = strstr(s->shown + s->from, text);
		if(found) {
			s->from = (size_t)(found - s->shown) + strlen(text);
			return 1;
		}
		pump(s, 50);
	}
	return 0;
}

static void send(struct session *s, const char *text) {
	CHECK(write(s->master, text, strlen(text)) == (ssize_t)strlen(text));
}

// Sends `key` every 200 ms until the shell shows `text`: for keys meant
// for a job, which reach it only once it is in the foreground.
static int press_until(struct session *s, const char *key, const char *text) {
	for(int i = 0; i < 25; i++) {
		send(s, key);
		char *found = NULL;
		for(int j = 0; j < 4 && !(found = strstr(s->shown + s->from, text)); j++)
			pump(s, 50);
		if(found) {
			s->from = (size_t)(found - s->shown) + strlen(text);
			return 1;
		}
	}
	return 0;
}

// Checks that the shell shows `text`; if not, says what it showed last.
#define EXPECT(s, text) \
	do { \
		if(!wait_for(s, text)) { \
			size_t tail_ = (s)->len > 300 ? (s)->len - 300 : 0; \
			printf("shell: never saw \"%s\"; the terminal shows \"%s\"\n", text, \
					(s)->shown + tail_); \
			check_failed(__FILE__, __LINE__, "the shell showed " #text, 0); \
		} \
	} while(0)

// Ends the session: the shell must exit with `code`.
static void finish(struct session *s, int code) {
	pump(s, 200);
	int status = check_wait(s->pid);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == code);
	close(s->master);
}

// Job control, as at any terminal: the same with the plain reader and
// with the line editor, whose Enter is a carriage return. Each key comes
// once what it is for has shown: a terminal throws away what was typed
// ahead when ^C or ^Z makes a signal.
static void jobs(struct session *s, const char *enter) {
	char line[128];
	snprintf(line, sizeof(line), "sleep 30 &%s", enter);
	send(s, line);
	EXPECT(s, "[1] ");
	snprintf(line, sizeof(line), "jobs%s", enter);
	send(s, line);
	EXPECT(s, "Running");
	snprintf(line, sizeof(line), "kill %%1; wait%s", enter);
	send(s, line);
	EXPECT(s, "Terminated");
	EXPECT(s, "$ ");
	// ^Z stops a foreground job, and fg brings it back for ^C to end.
	snprintf(line, sizeof(line), "sleep 31%s", enter);
	send(s, line);
	EXPECT(s, "sleep 31");
	pump(s, 300);
	if(!press_until(s, "\032", "Stopped"))
		check_failed(__FILE__, __LINE__, "^Z stopped the job", 0);
	EXPECT(s, "$ ");
	snprintf(line, sizeof(line), "fg%s", enter);
	send(s, line);
	EXPECT(s, "sleep 31");
	pump(s, 300);
	if(!press_until(s, "\003", "$ "))
		check_failed(__FILE__, __LINE__, "^C ended the job", 0);
	pump(s, 300);
	snprintf(line, sizeof(line), "echo status-$?%s", enter);
	send(s, line);
	EXPECT(s, "status-130");
}

// The plain reader, with no line editing: TERM=dumb.
static void plain(void) {
	struct session *s = calloc(1, sizeof(*s));
	CHECK(start(s, "dumb") == 0);
	EXPECT(s, "$ ");
	send(s, "echo $((6*7))x\n");
	EXPECT(s, "42x");
	// A command over two lines, with the second prompt.
	send(s, "for i in 1 2; do\n");
	EXPECT(s, "> ");
	send(s, "echo loop-$i; done\n");
	EXPECT(s, "loop-1");
	EXPECT(s, "loop-2");
	jobs(s, "\n");
	send(s, "exit 7\n");
	finish(s, 7);
	free(s);
}

// The line editor: completion, history and the same job control.
static void editor(void) {
	struct session *s = calloc(1, sizeof(*s));
	CHECK(start(s, "xterm") == 0);
	EXPECT(s, "$ ");
	send(s, "echo $((2*3))ed\r");
	EXPECT(s, "6ed");
	// Tab completes the command name.
	send(s, "ech\t");
	EXPECT(s, "echo");
	send(s, " $((1+1))tab\r");
	EXPECT(s, "2tab");
	// The up arrow brings the last command back.
	send(s, "echo $((3+4))up\r");
	EXPECT(s, "7up");
	send(s, "\033[A");
	EXPECT(s, "echo $((3+4))up");
	send(s, "\r");
	EXPECT(s, "7up");
	// ^C drops the line being typed.
	send(s, "echo dropped");
	EXPECT(s, "dropped");
	send(s, "\003");
	EXPECT(s, "$ ");
	pump(s, 300);
	send(s, "echo after-$?\r");
	EXPECT(s, "after-130");
	jobs(s, "\r");
	send(s, "exit 5\r");
	finish(s, 5);
	free(s);
}

int main(void) {
	check_start("shell");
	RUN(scripts);
	RUN(plain);
	RUN(editor);
	return check_end();
}
