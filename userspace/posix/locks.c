// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Advisory file locks: flock's locks on whole files, POSIX's record locks
// (fcntl's F_SETLK, F_SETLKW and F_GETLK) and Linux's open file
// description locks (F_OFD_...): who holds them, what stands in whose way,
// waiting and its deadlocks, and when they go. Uses /tmp.

#include <fcntl.h>
#include <sys/file.h>

#include "check.h"

static const char *const PATH = "/tmp/locks-file";

// Opens the test's file anew, with `flags`.
static int open_file(int flags) {
	int fd = open(PATH, flags | O_CREAT, 0644);
	CHECK(fd >= 0);
	return fd;
}

// A record lock request of `type` on `length` bytes from `start`.
static struct flock range(short type, off_t start, off_t length) {
	struct flock lock = {0};
	lock.l_type = type;
	lock.l_whence = SEEK_SET;
	lock.l_start = start;
	lock.l_len = length;
	return lock;
}

// Takes (or lets go of) a record lock with `request` on `fd`; returns
// fcntl's result, with errno as it left it.
static int set(int fd, int request, short type, off_t start, off_t length) {
	struct flock lock = range(type, start, length);
	return fcntl(fd, request, &lock);
}

// What F_GETLK (or F_OFD_GETLK) says stands in the way of a `type` lock.
static struct flock holder(int fd, int request, short type, off_t start, off_t length) {
	struct flock lock = range(type, start, length);
	CHECK(fcntl(fd, request, &lock) == 0);
	return lock;
}

// Waits for one byte on `fd`, which says the other side is ready.
static void await(int fd) {
	char byte;
	CHECK(read(fd, &byte, 1) == 1);
}

static void tell(int fd) {
	CHECK(write(fd, "x", 1) == 1);
}

static void whole_files(void) {
	int a = open_file(O_RDWR), b = open_file(O_RDONLY);
	// An exclusive lock keeps every other description's out.
	CHECK(flock(a, LOCK_EX) == 0);
	CHECK_ERROR(flock(b, LOCK_EX | LOCK_NB), EWOULDBLOCK);
	CHECK_ERROR(flock(b, LOCK_SH | LOCK_NB), EWOULDBLOCK);
	// A copy of the descriptor is the same description, which holds it.
	int copy = dup(a);
	CHECK(flock(copy, LOCK_EX | LOCK_NB) == 0);
	// Turned shared, it lets other shared locks in, but not exclusive ones.
	CHECK(flock(a, LOCK_SH) == 0);
	CHECK(flock(b, LOCK_SH | LOCK_NB) == 0);
	int c = open_file(O_RDONLY);
	CHECK_ERROR(flock(c, LOCK_EX | LOCK_NB), EWOULDBLOCK);
	CHECK(flock(b, LOCK_UN) == 0);
	CHECK(flock(a, LOCK_UN) == 0);
	CHECK(flock(c, LOCK_EX | LOCK_NB) == 0);
	// A description's lock goes with its last descriptor.
	CHECK(close(c) == 0);
	CHECK(flock(b, LOCK_EX | LOCK_NB) == 0);
	CHECK_ERROR(flock(copy, LOCK_SH | LOCK_NB), EWOULDBLOCK);
	CHECK(close(b) == 0);
	// A fork's child shares the description, and so its lock.
	CHECK(flock(a, LOCK_EX) == 0);
	pid_t child = check_fork();
	if(child == 0) {
		CHECK(flock(a, LOCK_EX | LOCK_NB) == 0);
		int other = open_file(O_RDONLY);
		CHECK_ERROR(flock(other, LOCK_SH | LOCK_NB), EWOULDBLOCK);
		check_exit_child();
	}
	CHECK_CHILD(child);
	// Directories lock too.
	int dir = open("/tmp", O_RDONLY | O_DIRECTORY);
	CHECK(dir >= 0 && flock(dir, LOCK_EX | LOCK_NB) == 0);
	int dir2 = open("/tmp", O_RDONLY | O_DIRECTORY);
	CHECK(dir2 >= 0);
	CHECK_ERROR(flock(dir2, LOCK_SH | LOCK_NB), EWOULDBLOCK);
	close(dir);
	CHECK(flock(dir2, LOCK_SH | LOCK_NB) == 0);
	close(dir2);
	CHECK_ERROR(flock(a, LOCK_SH | LOCK_EX), EINVAL);
	CHECK_ERROR(flock(-1, LOCK_SH), EBADF);
	close(a);
	close(copy);
}

static void whole_file_waits(void) {
	int a = open_file(O_RDWR);
	int ready[2];
	CHECK(pipe(ready) == 0);
	CHECK(flock(a, LOCK_EX) == 0);
	pid_t child = check_fork();
	if(child == 0) {
		int mine = open_file(O_RDWR);
		tell(ready[1]);
		long long before = check_now_ns();
		// Waits until the parent lets go, 100 ms on.
		CHECK(flock(mine, LOCK_EX) == 0);
		CHECK(check_now_ns() - before >= 50000000LL);
		check_exit_child();
	}
	await(ready[0]);
	check_sleep_ms(100);
	CHECK(flock(a, LOCK_UN) == 0);
	CHECK_CHILD(child);
	close(ready[0]);
	close(ready[1]);
	close(a);
}

static void on_alarm(int signal) {
	(void)signal;
}

static void interrupted_waits(void) {
	int a = open_file(O_RDWR), b = open_file(O_RDWR);
	CHECK(flock(a, LOCK_EX) == 0);
	// A signal whose handler does not restart calls cuts a wait short.
	struct sigaction action = {0}, old;
	sigemptyset(&action.sa_mask);
	action.sa_handler = on_alarm;
	CHECK(sigaction(SIGALRM, &action, &old) == 0);
	alarm(1);
	CHECK_ERROR(flock(b, LOCK_EX), EINTR);
	// So it does for a record lock.
	CHECK(set(a, F_SETLK, F_WRLCK, 0, 1) == 0);
	pid_t child = check_fork();
	if(child == 0) {
		CHECK(sigaction(SIGALRM, &action, NULL) == 0);
		alarm(1);
		int mine = open_file(O_RDWR);
		CHECK_ERROR(set(mine, F_SETLKW, F_WRLCK, 0, 1), EINTR);
		check_exit_child();
	}
	CHECK_CHILD(child);
	CHECK(sigaction(SIGALRM, &old, NULL) == 0);
	close(a);
	close(b);
}

static void records(void) {
	int a = open_file(O_RDWR), b = open_file(O_RDWR);
	// The process's own locks never stand in its way, whichever
	// descriptor takes them.
	CHECK(set(a, F_SETLK, F_WRLCK, 0, 10) == 0);
	CHECK(set(b, F_SETLK, F_RDLCK, 5, 10) == 0);
	CHECK(holder(b, F_GETLK, F_WRLCK, 0, 0).l_type == F_UNLCK);
	CHECK(set(b, F_SETLK, F_WRLCK, 5, 10) == 0);
	pid_t parent = getpid();
	int ready[2], done[2];
	CHECK(pipe(ready) == 0 && pipe(done) == 0);
	pid_t child = check_fork();
	if(child == 0) {
		// The parent's locks, [0, 15) exclusive, are not the child's.
		int mine = open_file(O_RDWR);
		CHECK_ERROR(set(mine, F_SETLK, F_WRLCK, 5, 1), EAGAIN);
		CHECK_ERROR(set(mine, F_SETLK, F_RDLCK, 14, 1), EAGAIN);
		CHECK(set(mine, F_SETLK, F_WRLCK, 15, 5) == 0);
		struct flock in_way = holder(mine, F_GETLK, F_RDLCK, 3, 1);
		CHECK(in_way.l_type == F_WRLCK && in_way.l_whence == SEEK_SET);
		CHECK(in_way.l_start == 0 && in_way.l_len == 15 && in_way.l_pid == parent);
		CHECK(holder(mine, F_GETLK, F_WRLCK, 15, 100).l_type == F_UNLCK);
		tell(ready[1]);
		// The parent lets go of [3, 5), then closes another descriptor
		// of the file, which lets go of all of its locks.
		await(done[0]);
		CHECK(set(mine, F_SETLK, F_WRLCK, 3, 2) == 0);
		CHECK_ERROR(set(mine, F_SETLK, F_WRLCK, 5, 1), EAGAIN);
		tell(ready[1]);
		await(done[0]);
		CHECK(set(mine, F_SETLK, F_WRLCK, 0, 0) == 0);
		check_exit_child();
	}
	await(ready[0]);
	CHECK(set(a, F_SETLK, F_UNLCK, 3, 2) == 0);
	tell(done[1]);
	await(ready[0]);
	// The child's lock on [3, 5) and [15, 20) are in the way now.
	CHECK_ERROR(set(a, F_SETLK, F_WRLCK, 0, 20), EAGAIN);
	struct flock in_way = holder(a, F_GETLK, F_WRLCK, 16, 1);
	CHECK(in_way.l_type == F_WRLCK && in_way.l_start == 15 && in_way.l_len == 5);
	CHECK(in_way.l_pid == child);
	int c = open_file(O_RDONLY);
	close(c);
	tell(done[1]);
	CHECK_CHILD(child);
	// The child's locks went when it exited.
	CHECK(set(a, F_SETLK, F_WRLCK, 0, 0) == 0);
	// A negative length counts back from the start, and the offset and the
	// end of the file are places to count from.
	CHECK(ftruncate(a, 100) == 0 && lseek(a, 40, SEEK_SET) == 40);
	CHECK(set(a, F_SETLK, F_UNLCK, 0, 0) == 0);
	CHECK(set(a, F_SETLK, F_RDLCK, 10, -5) == 0);
	struct flock lock = range(F_WRLCK, -10, 5);
	lock.l_whence = SEEK_CUR;
	CHECK(fcntl(a, F_SETLK, &lock) == 0);
	lock = range(F_WRLCK, -1, 1);
	lock.l_whence = SEEK_END;
	CHECK(fcntl(a, F_SETLK, &lock) == 0);
	child = check_fork();
	if(child == 0) {
		int mine = open_file(O_RDWR);
		in_way = holder(mine, F_GETLK, F_WRLCK, 0, 20);
		CHECK(in_way.l_type == F_RDLCK && in_way.l_start == 5 && in_way.l_len == 5);
		in_way = holder(mine, F_GETLK, F_RDLCK, 20, 50);
		CHECK(in_way.l_type == F_WRLCK && in_way.l_start == 30 && in_way.l_len == 5);
		in_way = holder(mine, F_GETLK, F_RDLCK, 50, 0);
		CHECK(in_way.l_type == F_WRLCK && in_way.l_start == 99 && in_way.l_len == 1);
		check_exit_child();
	}
	CHECK_CHILD(child);
	// What a lock guards, its descriptor must be open for.
	int reading = open_file(O_RDONLY), writing = open_file(O_WRONLY);
	CHECK_ERROR(set(reading, F_SETLK, F_WRLCK, 0, 1), EBADF);
	CHECK_ERROR(set(writing, F_SETLK, F_RDLCK, 0, 1), EBADF);
	CHECK(set(reading, F_SETLK, F_RDLCK, 0, 1) == 0);
	CHECK(set(writing, F_SETLK, F_WRLCK, 0, 1) == 0);
	// Requests that make no sense.
	CHECK_ERROR(set(a, F_SETLK, 7, 0, 1), EINVAL);
	CHECK_ERROR(set(a, F_SETLK, F_WRLCK, -1, 1), EINVAL);
	lock = range(F_UNLCK, 0, 1);
	CHECK_ERROR(fcntl(a, F_GETLK, &lock), EINVAL);
	lock = range(F_WRLCK, 0, 1);
	lock.l_whence = 9;
	CHECK_ERROR(fcntl(a, F_SETLK, &lock), EINVAL);
	CHECK_ERROR(set(-1, F_SETLK, F_WRLCK, 0, 1), EBADF);
	int fds[] = {a, b, reading, writing, ready[0], ready[1], done[0], done[1]};
	for(size_t i = 0; i < sizeof(fds) / sizeof(fds[0]); i++)
		close(fds[i]);
}

static void record_waits(void) {
	int a = open_file(O_RDWR);
	int ready[2];
	CHECK(pipe(ready) == 0);
	CHECK(set(a, F_SETLK, F_WRLCK, 0, 1) == 0);
	pid_t child = check_fork();
	if(child == 0) {
		int mine = open_file(O_RDWR);
		// Waits for the parent's [0, 1) holding [1, 2), which the parent
		// then asks for: it would wait forever, and fails with EDEADLK.
		CHECK(set(mine, F_SETLK, F_WRLCK, 1, 1) == 0);
		tell(ready[1]);
		CHECK(set(mine, F_SETLKW, F_WRLCK, 0, 1) == 0);
		check_exit_child();
	}
	await(ready[0]);
	// Give the child time to start waiting.
	check_sleep_ms(100);
	CHECK_ERROR(set(a, F_SETLKW, F_WRLCK, 1, 1), EDEADLK);
	CHECK(set(a, F_SETLK, F_UNLCK, 0, 1) == 0);
	CHECK_CHILD(child);
	close(ready[0]);
	close(ready[1]);
	close(a);
}

static void descriptions(void) {
	int a = open_file(O_RDWR), b = open_file(O_RDWR);
	struct flock lock = range(F_WRLCK, 0, 10);
	CHECK(fcntl(a, F_OFD_SETLK, &lock) == 0);
	// A description's range locks stand in the way of other descriptions'
	// and of process locks, even the same process's.
	lock = range(F_RDLCK, 5, 1);
	CHECK_ERROR(fcntl(b, F_OFD_SETLK, &lock), EAGAIN);
	CHECK_ERROR(set(b, F_SETLK, F_RDLCK, 5, 1), EAGAIN);
	lock = range(F_RDLCK, 5, 1);
	CHECK(fcntl(b, F_OFD_GETLK, &lock) == 0);
	CHECK(lock.l_type == F_WRLCK && lock.l_start == 0 && lock.l_len == 10 && lock.l_pid == -1);
	// They do not meet flock's.
	CHECK(flock(b, LOCK_EX | LOCK_NB) == 0);
	CHECK(flock(b, LOCK_UN) == 0);
	// Their process number must be 0.
	lock = range(F_RDLCK, 20, 1);
	lock.l_pid = 1;
	CHECK_ERROR(fcntl(b, F_OFD_SETLK, &lock), EINVAL);
	// Closing another descriptor of the file leaves them; the last of
	// their description lets go of them.
	int c = open_file(O_RDONLY);
	close(c);
	lock = range(F_RDLCK, 5, 1);
	CHECK_ERROR(fcntl(b, F_OFD_SETLK, &lock), EAGAIN);
	close(a);
	CHECK(fcntl(b, F_OFD_SETLK, &lock) == 0);
	close(b);
}

int main(void) {
	check_start("locks");
	RUN(whole_files);
	RUN(whole_file_waits);
	RUN(interrupted_waits);
	RUN(records);
	RUN(record_waits);
	RUN(descriptions);
	unlink(PATH);
	return check_end();
}
