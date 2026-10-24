// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Terminals: the one the test runs on, and pseudo-terminals it makes:
// line editing, echo, the end of the file, raw mode with VMIN and VTIME,
// output processing, flow control both ways, window sizes, signals from
// typed characters, and what happens when the master goes.

#include <fcntl.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <termios.h>

#include "check.h"

// Opens a pseudo-terminal: its master in *master and the terminal in
// *terminal, which does not become the caller's controlling terminal.
static int open_pty(int *master, int *terminal) {
	*master = posix_openpt(O_RDWR | O_NOCTTY);
	if(*master < 0 || grantpt(*master) < 0 || unlockpt(*master) < 0)
		return -1;
	const char *name = ptsname(*master);
	if(!name)
		return -1;
	*terminal = open(name, O_RDWR | O_NOCTTY);
	return *terminal < 0 ? -1 : 0;
}

// Reads what is there from `fd` within `ms` milliseconds into `buffer`
// (NUL-ended); returns how many bytes.
static ssize_t read_within(int fd, char *buffer, size_t size, int ms) {
	size_t got = 0;
	struct pollfd entry = {.fd = fd, .events = POLLIN};
	while(got < size - 1 && poll(&entry, 1, ms) == 1 && (entry.revents & POLLIN)) {
		ssize_t n = read(fd, buffer + got, size - 1 - got);
		if(n <= 0)
			break;
		got += (size_t)n;
		ms = 20;
	}
	buffer[got] = 0;
	return (ssize_t)got;
}

// The terminal the runner gave the test.
static void own(void) {
	CHECK(isatty(STDIN_FILENO) && isatty(STDOUT_FILENO));
	char name[64];
	CHECK(ttyname_r(STDIN_FILENO, name, sizeof(name)) == 0);
	CHECK(!strncmp(name, "/dev/pts/", 9));
	int fds[2];
	CHECK(pipe(fds) == 0);
	CHECK(!isatty(fds[0]) && errno == ENOTTY);
	struct termios attr;
	CHECK_ERROR(tcgetattr(fds[0], &attr), ENOTTY);
	close(fds[0]);
	close(fds[1]);
	// It is the controlling terminal, and the test is in the foreground.
	int tty = open("/dev/tty", O_RDWR);
	CHECK(tty >= 0 && isatty(tty));
	close(tty);
	CHECK(tcgetpgrp(STDIN_FILENO) == getpgrp());
	CHECK(tcgetsid(STDIN_FILENO) == getsid(0));
	// The settings it starts with.
	CHECK(tcgetattr(STDIN_FILENO, &attr) == 0);
	CHECK((attr.c_lflag & (ICANON | ECHO | ISIG)) == (ICANON | ECHO | ISIG));
	CHECK((attr.c_oflag & (OPOST | ONLCR)) == (OPOST | ONLCR));
	CHECK(attr.c_cc[VINTR] == 3 && attr.c_cc[VEOF] == 4);
}

static void lines(void) {
	int master, terminal;
	CHECK(open_pty(&master, &terminal) == 0);
	CHECK(isatty(terminal));
	char buffer[128];
	// A line, echoed back to the master with CR LF.
	CHECK(write(master, "hello\n", 6) == 6);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 6 && !memcmp(buffer, "hello\n", 6));
	CHECK(read_within(master, buffer, sizeof(buffer), 200) == 7);
	CHECK(!strcmp(buffer, "hello\r\n"));
	// Erasing a character, and the whole line.
	CHECK(write(master, "helx\x7flo\n", 8) == 8);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 6 && !memcmp(buffer, "hello\n", 6));
	CHECK(write(master, "junk\x15ok\n", 8) == 8);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 3 && !memcmp(buffer, "ok\n", 3));
	// Nothing until the line ends.
	CHECK(write(master, "part", 4) == 4);
	struct pollfd entry = {.fd = terminal, .events = POLLIN};
	CHECK(poll(&entry, 1, 30) == 0);
	CHECK(write(master, "\n", 1) == 1);
	CHECK(poll(&entry, 1, 1000) == 1);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 5);
	// A short read takes part of a line; the rest stays.
	CHECK(write(master, "abcdef\n", 7) == 7);
	CHECK(read(terminal, buffer, 3) == 3 && !memcmp(buffer, "abc", 3));
	CHECK(read(terminal, buffer, sizeof(buffer)) == 4 && !memcmp(buffer, "def\n", 4));
	// The end-of-file character: a read of 0 at the start of a line, and
	// the line so far otherwise.
	CHECK(write(master, "\x04", 1) == 1);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 0);
	CHECK(write(master, "xy\x04", 3) == 3);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 2 && !memcmp(buffer, "xy", 2));
	// Carriage returns become newlines (ICRNL).
	CHECK(write(master, "cr\r", 3) == 3);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 3 && !memcmp(buffer, "cr\n", 3));
	read_within(master, buffer, sizeof(buffer), 50);
	// Without echo, the master reads nothing back.
	struct termios attr;
	CHECK(tcgetattr(terminal, &attr) == 0);
	attr.c_lflag &= ~ECHO;
	CHECK(tcsetattr(terminal, TCSANOW, &attr) == 0);
	CHECK(write(master, "quiet\n", 6) == 6);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 6);
	CHECK(read_within(master, buffer, sizeof(buffer), 30) == 0);
	close(terminal);
	close(master);
}

static void raw(void) {
	int master, terminal;
	CHECK(open_pty(&master, &terminal) == 0);
	struct termios attr;
	CHECK(tcgetattr(terminal, &attr) == 0);
	cfmakeraw(&attr);
	attr.c_cc[VMIN] = 1;
	attr.c_cc[VTIME] = 0;
	CHECK(tcsetattr(terminal, TCSAFLUSH, &attr) == 0);
	struct termios now;
	CHECK(tcgetattr(terminal, &now) == 0 && !(now.c_lflag & (ICANON | ECHO | ISIG)));
	char buffer[64];
	// Each byte as it comes, with no editing, no echo and no signals.
	CHECK(write(master, "a\x7f\x03", 3) == 3);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 3 && !memcmp(buffer, "a\x7f\x03", 3));
	CHECK(read_within(master, buffer, sizeof(buffer), 30) == 0);
	// Output as written: no CR before LF.
	CHECK(write(terminal, "x\ny", 3) == 3);
	CHECK(read_within(master, buffer, sizeof(buffer), 200) == 3 && !strcmp(buffer, "x\ny"));
	// VMIN 0 and VTIME 2: a read waits up to 0.2 s, then returns 0.
	attr.c_cc[VMIN] = 0;
	attr.c_cc[VTIME] = 2;
	CHECK(tcsetattr(terminal, TCSANOW, &attr) == 0);
	long long start = check_now_ns();
	CHECK(read(terminal, buffer, sizeof(buffer)) == 0);
	long long took = check_now_ns() - start;
	CHECK(took >= 150000000 && took < 2000000000);
	// VMIN 0 and VTIME 0: at once.
	attr.c_cc[VTIME] = 0;
	CHECK(tcsetattr(terminal, TCSANOW, &attr) == 0);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 0);
	// VMIN 4: until four bytes have come.
	attr.c_cc[VMIN] = 4;
	CHECK(tcsetattr(terminal, TCSANOW, &attr) == 0);
	CHECK(write(master, "12", 2) == 2);
	// Nor is it ready for poll, as on Linux.
	struct pollfd entry = {.fd = terminal, .events = POLLIN};
	CHECK(poll(&entry, 1, 0) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		check_sleep_ms(30);
		write(master, "34", 2);
		_exit(0);
	}
	CHECK(read(terminal, buffer, sizeof(buffer)) == 4 && !memcmp(buffer, "1234", 4));
	CHECK_CHILD(pid);
	// tcflush drops what was typed.
	CHECK(write(master, "gone", 4) == 4);
	CHECK(tcflush(terminal, TCIFLUSH) == 0);
	attr.c_cc[VMIN] = 0;
	CHECK(tcsetattr(terminal, TCSANOW, &attr) == 0);
	CHECK(read(terminal, buffer, sizeof(buffer)) == 0);
	CHECK(tcdrain(terminal) == 0);
	close(terminal);
	close(master);
}

// VMIN with VTIME, typing past what the terminal holds, stopped output,
// and typeahead when canonical mode comes back, as on Linux.
static void flow(void) {
	int master, terminal;
	CHECK(open_pty(&master, &terminal) == 0);
	struct termios cooked, attr;
	CHECK(tcgetattr(terminal, &cooked) == 0);
	attr = cooked;
	cfmakeraw(&attr);
	attr.c_cc[VMIN] = 4;
	attr.c_cc[VTIME] = 1;
	CHECK(tcsetattr(terminal, TCSANOW, &attr) == 0);
	char buffer[64];
	// VMIN 4 and VTIME 1: a byte makes it ready for poll, and a read ends
	// 0.1 s after the last byte came.
	CHECK(write(master, "12", 2) == 2);
	struct pollfd entry = {.fd = terminal, .events = POLLIN};
	CHECK(poll(&entry, 1, 1000) == 1 && (entry.revents & POLLIN));
	long long start = check_now_ns();
	CHECK(read(terminal, buffer, sizeof(buffer)) == 2 && !memcmp(buffer, "12", 2));
	long long took = check_now_ns() - start;
	CHECK(took >= 50000000 && took < 2000000000);
	// A read asking for fewer than VMIN bytes ends once it has them.
	attr.c_cc[VTIME] = 0;
	CHECK(tcsetattr(terminal, TCSANOW, &attr) == 0);
	CHECK(write(master, "345", 3) == 3);
	CHECK(read(terminal, buffer, 2) == 2 && !memcmp(buffer, "34", 2));
	CHECK(read(terminal, buffer, 1) == 1 && buffer[0] == '5');
	// Typing past what the terminal holds waits for a read: a master
	// that does not block takes what fits, then says EAGAIN, and polls
	// writable again once a read made room.
	int flags = fcntl(master, F_GETFL);
	CHECK(fcntl(master, F_SETFL, flags | O_NONBLOCK) == 0);
	static char typing[6000];
	memset(typing, 'k', sizeof(typing));
	ssize_t took_in = write(master, typing, sizeof(typing));
	CHECK(took_in == 4096);
	CHECK_ERROR(write(master, "k", 1), EAGAIN);
	struct pollfd out = {.fd = master, .events = POLLOUT};
	CHECK(poll(&out, 1, 0) == 0);
	CHECK(read(terminal, buffer, sizeof(buffer)) == sizeof(buffer));
	CHECK(poll(&out, 1, 1000) == 1 && (out.revents & POLLOUT));
	CHECK(write(master, typing, sizeof(typing)) == sizeof(buffer));
	CHECK(fcntl(master, F_SETFL, flags) == 0);
	CHECK(tcflush(terminal, TCIFLUSH) == 0);
	// Stopped output: the terminal side is not writable, and a write
	// that does not block says EAGAIN, until output starts again.
	CHECK(tcflow(terminal, TCOOFF) == 0);
	struct pollfd side = {.fd = terminal, .events = POLLOUT};
	CHECK(poll(&side, 1, 0) == 0);
	flags = fcntl(terminal, F_GETFL);
	CHECK(fcntl(terminal, F_SETFL, flags | O_NONBLOCK) == 0);
	CHECK_ERROR(write(terminal, "x", 1), EAGAIN);
	CHECK(tcflow(terminal, TCOON) == 0);
	CHECK(poll(&side, 1, 1000) == 1 && (side.revents & POLLOUT));
	CHECK(write(terminal, "x", 1) == 1);
	CHECK(fcntl(terminal, F_SETFL, flags) == 0);
	CHECK(read_within(master, buffer, sizeof(buffer), 200) == 1 && buffer[0] == 'x');
	// Typeahead from raw mode is a line, readable at once, once
	// canonical mode comes back.
	CHECK(write(master, "ls", 2) == 2);
	CHECK(tcsetattr(terminal, TCSANOW, &cooked) == 0);
	CHECK(read_within(master, buffer, sizeof(buffer), 30) == 0);
	entry.revents = 0;
	CHECK(poll(&entry, 1, 0) == 1 && (entry.revents & POLLIN));
	CHECK(read(terminal, buffer, sizeof(buffer)) == 2 && !memcmp(buffer, "ls", 2));
	close(terminal);
	close(master);
}

static void sizes(void) {
	int master, terminal;
	CHECK(open_pty(&master, &terminal) == 0);
	struct winsize size = {.ws_row = 40, .ws_col = 132};
	CHECK(ioctl(master, TIOCSWINSZ, &size) == 0);
	struct winsize got = {0};
	CHECK(ioctl(terminal, TIOCGWINSZ, &got) == 0 && got.ws_row == 40 && got.ws_col == 132);
	CHECK(tcgetwinsize(terminal, &got) == 0 && got.ws_col == 132);
	int number = -1;
	CHECK(ioctl(master, TIOCGPTN, &number) == 0 && number >= 0);
	char name[32];
	CHECK(ptsname_r(master, name, sizeof(name)) == 0);
	char expected[32];
	snprintf(expected, sizeof(expected), "/dev/pts/%d", number);
	CHECK(!strcmp(name, expected));
	CHECK(ttyname_r(terminal, name, sizeof(name)) == 0 && !strcmp(name, expected));
	CHECK(ttyname_r(terminal, name, 4) == ERANGE);
	close(terminal);
	close(master);
}

// A child in a session of its own whose controlling terminal is a new
// pseudo-terminal: what the master types signals it.
static void signals(void) {
	static const struct {
		char typed;
		int signal;
	} keys[] = {{0x03, SIGINT}, {0x1c, SIGQUIT}};
	for(size_t i = 0; i < sizeof(keys) / sizeof(keys[0]); i++) {
		int master, terminal;
		CHECK(open_pty(&master, &terminal) == 0);
		int ready[2];
		CHECK(pipe(ready) == 0);
		pid_t pid = check_fork();
		if(pid == 0) {
			setsid();
			// Opening it without O_NOCTTY makes it the controlling terminal.
			int tty = open(ptsname(master), O_RDWR);
			write(ready[1], "r", 1);
			char buffer[16];
			while(tty >= 0)
				read(tty, buffer, sizeof(buffer));
			_exit(1);
		}
		char byte;
		CHECK(read(ready[0], &byte, 1) == 1);
		CHECK(write(master, &keys[i].typed, 1) == 1);
		int status = check_wait(pid);
		CHECK(WIFSIGNALED(status) && WTERMSIG(status) == keys[i].signal);
		close(ready[0]);
		close(ready[1]);
		close(terminal);
		close(master);
	}
	// A new window size: SIGWINCH for the foreground group.
	int master, terminal;
	CHECK(open_pty(&master, &terminal) == 0);
	int ready[2];
	CHECK(pipe(ready) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		setsid();
		signal(SIGWINCH, SIG_DFL);
		sigset_t block;
		sigemptyset(&block);
		sigaddset(&block, SIGWINCH);
		sigprocmask(SIG_BLOCK, &block, NULL);
		int tty = open(ptsname(master), O_RDWR);
		write(ready[1], "r", 1);
		struct timespec timeout = {5, 0};
		_exit(tty >= 0 && sigtimedwait(&block, NULL, &timeout) == SIGWINCH ? 0 : 1);
	}
	char byte;
	CHECK(read(ready[0], &byte, 1) == 1);
	struct winsize size = {.ws_row = 10, .ws_col = 20};
	CHECK(ioctl(master, TIOCSWINSZ, &size) == 0);
	CHECK_CHILD(pid);
	close(ready[0]);
	close(ready[1]);
	close(terminal);
	close(master);
}

// When the master goes, the terminal hangs up: the session's foreground
// group gets SIGHUP, and reads find the end of the file.
static void hangups(void) {
	int master, terminal;
	CHECK(open_pty(&master, &terminal) == 0);
	int ready[2];
	CHECK(pipe(ready) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		close(master);
		setsid();
		// Asking for it makes it the controlling terminal too, and
		// asking again changes nothing, as on Linux.
		CHECK(ioctl(terminal, TIOCSCTTY, 0) == 0);
		CHECK(ioctl(terminal, TIOCSCTTY, 0) == 0);
		// The leader of another session cannot take it.
		pid_t other = check_fork();
		if(other == 0) {
			setsid();
			CHECK_ERROR(ioctl(terminal, TIOCSCTTY, 0), EPERM);
			check_exit_child();
		}
		CHECK_CHILD(other);
		write(ready[1], "r", 1);
		pause();
		_exit(1);
	}
	char byte;
	CHECK(read(ready[0], &byte, 1) == 1);
	close(master);
	int status = check_wait(pid);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGHUP);
	char buffer[8];
	CHECK(read(terminal, buffer, sizeof(buffer)) == 0);
	CHECK_ERROR(open("/dev/pts/1000", O_RDWR | O_NOCTTY), ENOENT);
	close(ready[0]);
	close(ready[1]);
	close(terminal);
	// A terminal whose master is open but the terminal is closed: the
	// master reads fail with EIO.
	CHECK(open_pty(&master, &terminal) == 0);
	close(terminal);
	CHECK_ERROR(read(master, buffer, sizeof(buffer)), EIO);
	close(master);
}

int main(void) {
	check_start("tty");
	RUN(own);
	RUN(lines);
	RUN(raw);
	RUN(flow);
	RUN(sizes);
	RUN(signals);
	RUN(hangups);
	return check_end();
}
