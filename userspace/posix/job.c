// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Process groups and sessions, as a shell uses them for job control:
// groups and their leaders, sessions without a terminal, the foreground
// group, background jobs that read or write the terminal, signals to a
// whole group, and orphaned groups.

#include <fcntl.h>
#include <sys/ioctl.h>
#include <termios.h>

#include "check.h"

static void groups(void) {
	// The runner made the test the leader of its own group.
	CHECK(getpgrp() == getpid() && getpgid(0) == getpid());
	CHECK(getpgid(getpid()) == getpid());
	CHECK(getsid(0) == getsid(getppid()));
	// A group leader cannot start a session.
	CHECK_ERROR(setsid(), EPERM);
	// A child joins a group of its own, or its parent's again.
	int fds[2];
	CHECK(pipe(fds) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		char byte;
		read(fds[0], &byte, 1);
		_exit(0);
	}
	CHECK(getpgid(pid) == getpgrp());
	CHECK(setpgid(pid, pid) == 0 && getpgid(pid) == pid);
	CHECK(setpgid(pid, getpgrp()) == 0 && getpgid(pid) == getpgrp());
	CHECK_ERROR(setpgid(pid, 99999), EPERM);
	CHECK_ERROR(setpgid(99999, 0), ESRCH);
	CHECK_ERROR(getpgid(99999), ESRCH);
	write(fds[1], "x", 1);
	CHECK_CHILD(pid);
	close(fds[0]);
	close(fds[1]);
	// Once it has run exec, its parent cannot move it: the end of a pipe
	// that closes on exec says when.
	CHECK(pipe2(fds, O_CLOEXEC) == 0);
	pid = check_fork();
	if(pid == 0) {
		close(fds[0]);
		execl("/system/tests/posix/hello", "hello", (char *)NULL);
		_exit(126);
	}
	close(fds[1]);
	char byte;
	CHECK(read(fds[0], &byte, 1) == 0);
	CHECK_ERROR(setpgid(pid, pid), EACCES);
	CHECK_CHILD(pid);
	close(fds[0]);
}

static void sessions(void) {
	pid_t pid = check_fork();
	if(pid == 0) {
		pid_t session = setsid();
		CHECK(session == getpid());
		CHECK(getsid(0) == getpid() && getpgrp() == getpid());
		// A new session has no controlling terminal.
		CHECK_ERROR(open("/dev/tty", O_RDWR), ENXIO);
		CHECK_ERROR(tcgetpgrp(STDIN_FILENO), ENOTTY);
		// Its leader cannot start another.
		CHECK_ERROR(setsid(), EPERM);
		// Nor join a group of another session.
		CHECK_ERROR(setpgid(0, getppid()), EPERM);
		check_exit_child();
	}
	CHECK_CHILD(pid);
}

// The foreground group, and jobs in the background.
static void foreground(void) {
	CHECK(tcgetpgrp(STDIN_FILENO) == getpgrp());
	// A background job that reads the terminal stops with SIGTTIN.
	pid_t pid = check_fork();
	if(pid == 0) {
		setpgid(0, 0);
		char byte;
		read(STDIN_FILENO, &byte, 1);
		_exit(1);
	}
	setpgid(pid, pid);
	int status;
	CHECK(waitpid(pid, &status, WUNTRACED) == pid);
	CHECK(WIFSTOPPED(status) && WSTOPSIG(status) == SIGTTIN);
	kill(pid, SIGKILL);
	status = check_wait(pid);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
	// One that ignores SIGTTIN fails with EIO instead.
	pid = check_fork();
	if(pid == 0) {
		setpgid(0, 0);
		signal(SIGTTIN, SIG_IGN);
		char byte;
		CHECK_ERROR(read(STDIN_FILENO, &byte, 1), EIO);
		check_exit_child();
	}
	setpgid(pid, pid);
	CHECK_CHILD(pid);
	// Writing is allowed until TOSTOP is set; then SIGTTOU stops it.
	struct termios attr, old;
	CHECK(tcgetattr(STDIN_FILENO, &old) == 0);
	pid = check_fork();
	if(pid == 0) {
		setpgid(0, 0);
		CHECK(write(STDOUT_FILENO, "", 0) == 0);
		check_exit_child();
	}
	setpgid(pid, pid);
	CHECK_CHILD(pid);
	attr = old;
	attr.c_lflag |= TOSTOP;
	CHECK(tcsetattr(STDIN_FILENO, TCSANOW, &attr) == 0);
	pid = check_fork();
	if(pid == 0) {
		setpgid(0, 0);
		write(STDOUT_FILENO, "x\n", 2);
		_exit(1);
	}
	setpgid(pid, pid);
	CHECK(waitpid(pid, &status, WUNTRACED) == pid);
	CHECK(WIFSTOPPED(status) && WSTOPSIG(status) == SIGTTOU);
	kill(pid, SIGKILL);
	check_wait(pid);
	CHECK(tcsetattr(STDIN_FILENO, TCSANOW, &old) == 0);
	// Changing the settings from the background stops with SIGTTOU too.
	pid = check_fork();
	if(pid == 0) {
		setpgid(0, 0);
		tcsetattr(STDIN_FILENO, TCSANOW, &old);
		_exit(1);
	}
	setpgid(pid, pid);
	CHECK(waitpid(pid, &status, WUNTRACED) == pid);
	CHECK(WIFSTOPPED(status) && WSTOPSIG(status) == SIGTTOU);
	kill(pid, SIGKILL);
	check_wait(pid);
	// Handing the terminal to another group, and taking it back as a
	// shell does: with SIGTTOU ignored.
	int fds[2];
	CHECK(pipe(fds) == 0);
	pid = check_fork();
	if(pid == 0) {
		setpgid(0, 0);
		char byte;
		read(fds[0], &byte, 1);
		_exit(tcgetpgrp(STDIN_FILENO) == getpgrp() ? 0 : 1);
	}
	setpgid(pid, pid);
	CHECK(tcsetpgrp(STDIN_FILENO, pid) == 0 && tcgetpgrp(STDIN_FILENO) == pid);
	write(fds[1], "x", 1);
	CHECK_CHILD(pid);
	signal(SIGTTOU, SIG_IGN);
	CHECK(tcsetpgrp(STDIN_FILENO, getpgrp()) == 0);
	signal(SIGTTOU, SIG_DFL);
	CHECK(tcgetpgrp(STDIN_FILENO) == getpgrp());
	CHECK_ERROR(tcsetpgrp(STDIN_FILENO, 99999), EPERM);
	close(fds[0]);
	close(fds[1]);
}

// kill(-group) signals every process of the group.
static void whole_groups(void) {
	enum { MEMBERS = 3 };
	pid_t pids[MEMBERS];
	for(int i = 0; i < MEMBERS; i++) {
		pids[i] = check_fork();
		if(pids[i] == 0) {
			setpgid(0, i == 0 ? 0 : pids[0]);
			for(;;)
				pause();
		}
		setpgid(pids[i], pids[0]);
	}
	CHECK(kill(-pids[0], SIGTERM) == 0);
	for(int i = 0; i < MEMBERS; i++) {
		int status = check_wait(pids[i]);
		CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM);
	}
	CHECK_ERROR(kill(-pids[0], SIGTERM), ESRCH);
}

// A group that is orphaned with a stopped member gets SIGHUP and SIGCONT:
// here the member dies of SIGHUP, which closes the pipe it holds.
static void orphaned(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		setpgid(0, 0);
		pid_t member = fork();
		if(member == 0) {
			close(fds[0]);
			raise(SIGSTOP);
			_exit(0);
		}
		int status;
		waitpid(member, &status, WUNTRACED);
		_exit(WIFSTOPPED(status) ? 0 : 1);
	}
	close(fds[1]);
	CHECK_CHILD(pid);
	char byte;
	CHECK(read(fds[0], &byte, 1) == 0);
	close(fds[0]);
}

int main(void) {
	check_start("job");
	RUN(groups);
	RUN(sessions);
	RUN(foreground);
	RUN(whole_groups);
	RUN(orphaned);
	return check_end();
}
