// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Thread cancellation (pthread_cancel(3)) as POSIX has it: a deferred
// cancel ends a thread at a cancellation point, while it waits there or as
// it comes to one, before the call has done anything, and nowhere else;
// the thread's cleanup handlers run, last pushed first, then its keys'
// destructors. A cancel that comes in each wait below ends it as on glibc
// 2.39 and musl 1.2.5 (a run of this test on glibc 2.39 differs only
// where marked "as musl"); stdio, which glibc may cancel in and musl does
// not, is no cancellation point, as on musl.

#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <termios.h>

#include "check.h"

#define SOCKET_PATH "/tmp/cancel-socket"
#define LOCK_PATH "/tmp/cancel-lock"
#define NEW_PATH "/tmp/cancel-new"
#define SYSTEM_PATH "/tmp/cancel-system"

// Checks `condition` of the case called `name`.
#define CHECK_CASE(condition, name) \
	do { \
		int check_ok_ = !!(condition); \
		int check_errno_ = errno; \
		if(!check_ok_) { \
			char what_[160]; \
			snprintf(what_, sizeof(what_), "%s: %s", (name), #condition); \
			check_failed(__FILE__, __LINE__, what_, check_errno_); \
		} \
	} while(0)

static void wait_on(sem_t *sem) {
	while(sem_wait(sem) < 0 && errno == EINTR)
		;
}

static void set_blocking(int fd, int blocking) {
	int flags = fcntl(fd, F_GETFL);
	fcntl(fd, F_SETFL, blocking ? flags & ~O_NONBLOCK : flags | O_NONBLOCK);
}

// Connects socket `fd` to SOCKET_PATH.
static int connect_listener(int fd) {
	struct sockaddr_un address = {.sun_family = AF_UNIX};
	strcpy(address.sun_path, SOCKET_PATH);
	return connect(fd, (struct sockaddr *)&address, sizeof(address));
}

// A socket connected to SOCKET_PATH, or -1.
static int connect_to_listener(int nonblocking) {
	int fd = socket(AF_UNIX, SOCK_STREAM | (nonblocking ? SOCK_NONBLOCK : 0), 0);
	if(fd >= 0 && connect_listener(fd) < 0) {
		close(fd);
		return -1;
	}
	return fd;
}

// What a case's thread works on: made before the thread starts and put
// away after it ends.
static struct {
	int pipe[2];
	int pair[2];
	int listener;
	int clients[64];
	int queued;
	int lock[2];
	int file;
	pid_t child;
	pthread_t other;
	sem_t sem;
	sem_t other_go;
	pthread_mutex_t mutex;
	pthread_cond_t cond;
	sigset_t usr2;
	void *page;
} fx;

static void open_listener(int backlog) {
	unlink(SOCKET_PATH);
	fx.listener = socket(AF_UNIX, SOCK_STREAM, 0);
	struct sockaddr_un address = {.sun_family = AF_UNIX};
	strcpy(address.sun_path, SOCKET_PATH);
	CHECK(fx.listener >= 0);
	CHECK(bind(fx.listener, (struct sockaddr *)&address, sizeof(address)) == 0);
	CHECK(listen(fx.listener, backlog) == 0);
}

static void close_listener(void) {
	for(int i = 0; i < fx.queued; i++)
		close(fx.clients[i]);
	fx.queued = 0;
	close(fx.listener);
	unlink(SOCKET_PATH);
}

// Fills the write end of `fd`'s pipe or socket until it would wait.
static void fill(int fd) {
	static char block[4096];
	set_blocking(fd, 0);
	while(write(fd, block, sizeof(block)) > 0)
		;
	while(write(fd, block, 1) > 0)
		;
	set_blocking(fd, 1);
}

static void *waits_forever(void *argument) {
	(void)argument;
	wait_on(&fx.other_go);
	return NULL;
}

static void make_child(int code) {
	fflush(stdout);
	fx.child = fork();
	if(fx.child == 0) {
		if(code < 0) {
			for(;;)
				pause();
		}
		_exit(code);
	}
	CHECK(fx.child > 0);
}

// The cancellation points that wait, each with a thread that waits there
// when the cancel comes.
enum point {
	P_READ,
	P_READV,
	P_WRITE,
	P_RECV,
	P_SEND,
	P_ACCEPT,
	P_CONNECT,
	P_POLL,
	P_PPOLL,
	P_SELECT,
	P_SLEEP,
	P_NANOSLEEP,
	P_CLOCK_NANOSLEEP,
	P_PAUSE,
	P_SIGSUSPEND,
	P_SIGWAIT,
	P_SIGTIMEDWAIT,
	P_WAITPID,
	P_WAITID,
	P_SEM_WAIT,
	P_SEM_TIMEDWAIT,
	P_COND_WAIT,
	P_COND_TIMEDWAIT,
	P_JOIN,
	P_LOCK,
	P_SYSTEM,
	POINTS,
};

static const char *const POINT_NAMES[POINTS] = {
		"read", "readv", "write", "recv", "send", "accept", "connect", "poll", "ppoll",
		"select", "sleep", "nanosleep", "clock_nanosleep", "pause", "sigsuspend", "sigwait",
		"sigtimedwait", "waitpid", "waitid", "sem_wait", "sem_timedwait",
		"pthread_cond_wait", "pthread_cond_timedwait", "pthread_join", "fcntl F_OFD_SETLKW",
		"system",
};

static void prepare(enum point point) {
	switch(point) {
	case P_READ:
	case P_READV:
	case P_POLL:
	case P_PPOLL:
	case P_SELECT:
		CHECK(pipe(fx.pipe) == 0);
		break;
	case P_WRITE:
		CHECK(pipe(fx.pipe) == 0);
		fill(fx.pipe[1]);
		break;
	case P_RECV:
	case P_SEND:
		CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fx.pair) == 0);
		if(point == P_SEND)
			fill(fx.pair[0]);
		break;
	case P_ACCEPT:
		open_listener(4);
		break;
	case P_CONNECT:
		// A listener whose queue is full: a connect waits for room.
		open_listener(1);
		while(fx.queued < 64 && (fx.clients[fx.queued] = connect_to_listener(1)) >= 0)
			fx.queued++;
		CHECK(fx.queued < 64);
		fx.file = socket(AF_UNIX, SOCK_STREAM, 0);
		CHECK(fx.file >= 0);
		break;
	case P_SIGWAIT:
	case P_SIGTIMEDWAIT:
		sigemptyset(&fx.usr2);
		sigaddset(&fx.usr2, SIGUSR2);
		break;
	case P_WAITPID:
	case P_WAITID:
		make_child(-1);
		break;
	case P_SEM_WAIT:
	case P_SEM_TIMEDWAIT:
		CHECK(sem_init(&fx.sem, 0, 0) == 0);
		break;
	case P_COND_WAIT:
	case P_COND_TIMEDWAIT: {
		// An error-checking mutex, which only its holder can unlock.
		pthread_mutexattr_t attr;
		pthread_mutexattr_init(&attr);
		pthread_mutexattr_settype(&attr, PTHREAD_MUTEX_ERRORCHECK);
		CHECK(pthread_mutex_init(&fx.mutex, &attr) == 0);
		pthread_mutexattr_destroy(&attr);
		CHECK(pthread_cond_init(&fx.cond, NULL) == 0);
		break;
	}
	case P_JOIN:
		CHECK(sem_init(&fx.other_go, 0, 0) == 0);
		CHECK(pthread_create(&fx.other, NULL, waits_forever, NULL) == 0);
		break;
	case P_LOCK: {
		// Two open file descriptions of a file: the lock of one keeps
		// the other's waiting.
		fx.lock[0] = open(LOCK_PATH, O_RDWR | O_CREAT | O_TRUNC, 0644);
		fx.lock[1] = open(LOCK_PATH, O_RDWR);
		CHECK(fx.lock[0] >= 0 && fx.lock[1] >= 0);
		struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
		CHECK(fcntl(fx.lock[0], F_OFD_SETLK, &whole) == 0);
		break;
	}
	default:
		break;
	}
}

static void finish(enum point point) {
	switch(point) {
	case P_READ:
	case P_READV:
	case P_WRITE:
	case P_POLL:
	case P_PPOLL:
	case P_SELECT:
		close(fx.pipe[0]);
		close(fx.pipe[1]);
		break;
	case P_RECV:
	case P_SEND:
		close(fx.pair[0]);
		close(fx.pair[1]);
		break;
	case P_ACCEPT:
		close_listener();
		break;
	case P_CONNECT:
		close(fx.file);
		close_listener();
		break;
	case P_WAITPID:
	case P_WAITID:
		kill(fx.child, SIGKILL);
		CHECK(waitpid(fx.child, NULL, 0) == fx.child);
		break;
	case P_SEM_WAIT:
	case P_SEM_TIMEDWAIT:
		sem_destroy(&fx.sem);
		break;
	case P_COND_WAIT:
	case P_COND_TIMEDWAIT:
		pthread_cond_destroy(&fx.cond);
		pthread_mutex_destroy(&fx.mutex);
		break;
	case P_JOIN:
		// A cancelled join leaves the thread to be joined.
		sem_post(&fx.other_go);
		CHECK(pthread_join(fx.other, NULL) == 0);
		sem_destroy(&fx.other_go);
		break;
	case P_LOCK:
		close(fx.lock[0]);
		close(fx.lock[1]);
		unlink(LOCK_PATH);
		break;
	default:
		break;
	}
}

// One case's thread, and what the main thread learns of it.
struct blocked {
	enum point point;
	sem_t ready;
	int cleaned;
	int returned;
	int held;
};

static void blocked_cleanup(void *argument) {
	struct blocked *b = argument;
	// A condition's mutex is the thread's again when the cancel is acted on.
	if(b->point == P_COND_WAIT || b->point == P_COND_TIMEDWAIT)
		b->held = pthread_mutex_unlock(&fx.mutex) == 0;
	b->cleaned = 1;
}

static void in_point(enum point point) {
	char byte;
	struct timespec long_time = {100, 0};
	struct timespec deadline;
	clock_gettime(CLOCK_REALTIME, &deadline);
	deadline.tv_sec += 100;
	struct pollfd entry = {fx.pipe[0], POLLIN, 0};
	switch(point) {
	case P_READ:
		read(fx.pipe[0], &byte, 1);
		break;
	case P_READV: {
		struct iovec vector = {&byte, 1};
		readv(fx.pipe[0], &vector, 1);
		break;
	}
	case P_WRITE:
		write(fx.pipe[1], "x", 1);
		break;
	case P_RECV:
		recv(fx.pair[0], &byte, 1, 0);
		break;
	case P_SEND:
		send(fx.pair[0], "x", 1, 0);
		break;
	case P_ACCEPT:
		accept(fx.listener, NULL, NULL);
		break;
	case P_CONNECT:
		connect_listener(fx.file);
		break;
	case P_POLL:
		poll(&entry, 1, -1);
		break;
	case P_PPOLL:
		ppoll(&entry, 1, NULL, NULL);
		break;
	case P_SELECT: {
		fd_set set;
		FD_ZERO(&set);
		FD_SET(fx.pipe[0], &set);
		select(fx.pipe[0] + 1, &set, NULL, NULL, NULL);
		break;
	}
	case P_SLEEP:
		sleep(100);
		break;
	case P_NANOSLEEP:
		nanosleep(&long_time, NULL);
		break;
	case P_CLOCK_NANOSLEEP:
		clock_nanosleep(CLOCK_MONOTONIC, 0, &long_time, NULL);
		break;
	case P_PAUSE:
		pause();
		break;
	case P_SIGSUSPEND: {
		sigset_t none;
		sigemptyset(&none);
		sigsuspend(&none);
		break;
	}
	case P_SIGWAIT: {
		int signal;
		sigwait(&fx.usr2, &signal);
		break;
	}
	case P_SIGTIMEDWAIT:
		sigtimedwait(&fx.usr2, NULL, &long_time);
		break;
	case P_WAITPID:
		waitpid(fx.child, NULL, 0);
		break;
	case P_WAITID: {
		siginfo_t info;
		waitid(P_PID, (id_t)fx.child, &info, WEXITED);
		break;
	}
	case P_SEM_WAIT:
		sem_wait(&fx.sem);
		break;
	case P_SEM_TIMEDWAIT:
		sem_timedwait(&fx.sem, &deadline);
		break;
	case P_COND_WAIT:
		pthread_cond_wait(&fx.cond, &fx.mutex);
		pthread_mutex_unlock(&fx.mutex);
		break;
	case P_COND_TIMEDWAIT:
		pthread_cond_timedwait(&fx.cond, &fx.mutex, &deadline);
		pthread_mutex_unlock(&fx.mutex);
		break;
	case P_JOIN:
		pthread_join(fx.other, NULL);
		break;
	case P_LOCK: {
		struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
		fcntl(fx.lock[1], F_OFD_SETLKW, &whole);
		break;
	}
	case P_SYSTEM:
		system("exec sleep 100");
		break;
	default:
		break;
	}
}

static void *blocks(void *argument) {
	struct blocked *b = argument;
	if(b->point == P_SIGWAIT || b->point == P_SIGTIMEDWAIT)
		pthread_sigmask(SIG_BLOCK, &fx.usr2, NULL);
	if(b->point == P_COND_WAIT || b->point == P_COND_TIMEDWAIT)
		pthread_mutex_lock(&fx.mutex);
	pthread_cleanup_push(blocked_cleanup, b);
	sem_post(&b->ready);
	in_point(b->point);
	b->returned = 1;
	pthread_cleanup_pop(0);
	return NULL;
}

// The disposition of SIGINT, which system changes while its command runs.
static void (*sigint_handler(void))(int) {
	struct sigaction old;
	sigaction(SIGINT, NULL, &old);
	return old.sa_handler;
}

// A cancel that comes while a thread waits at each point ends it there.
static void waiting(void) {
	void (*sigint)(int) = sigint_handler();
	for(int p = 0; p < POINTS; p++) {
		const char *name = POINT_NAMES[p];
		prepare(p);
		struct blocked b = {.point = p};
		sem_init(&b.ready, 0, 0);
		pthread_t thread;
		CHECK_CASE(pthread_create(&thread, NULL, blocks, &b) == 0, name);
		wait_on(&b.ready);
		if(p == P_SYSTEM) {
			// Until the command runs.
			for(int i = 0; i < 1000 && sigint_handler() != SIG_IGN; i++)
				check_sleep_ms(5);
		}
		check_sleep_ms(20);
		CHECK_CASE(pthread_cancel(thread) == 0, name);
		void *result = NULL;
		CHECK_CASE(pthread_join(thread, &result) == 0, name);
		CHECK_CASE(result == PTHREAD_CANCELED, name);
		CHECK_CASE(b.cleaned && !b.returned, name);
		if(p == P_COND_WAIT || p == P_COND_TIMEDWAIT)
			CHECK_CASE(b.held, name);
		if(p == P_SYSTEM) {
			// The command was killed and waited for, and SIGINT is back.
			errno = 0;
			CHECK_CASE(waitpid(-1, NULL, WNOHANG) == -1 && errno == ECHILD, name);
			CHECK_CASE(sigint_handler() == sigint, name);
		}
		sem_destroy(&b.ready);
		finish(p);
	}
}

// The points again, with the cancel already due when the thread comes to
// each: the call does nothing, whether it would wait or not. A
// pthread_join of a thread that has ended acts on it as musl (glibc acts
// only when the join waits).
enum due {
	D_READ,
	D_WRITE,
	D_RECV,
	D_SEND,
	D_ACCEPT,
	D_POLL,
	D_SELECT,
	D_OPEN,
	D_OPENAT,
	D_CREAT,
	D_CLOSE,
	D_PREAD,
	D_PWRITE,
	D_FSYNC,
	D_FDATASYNC,
	D_MSYNC,
	D_TCDRAIN,
	D_NANOSLEEP,
	D_SLEEP,
	D_CLOCK_NANOSLEEP,
	D_SIGTIMEDWAIT,
	D_WAITPID,
	D_SEM_WAIT,
	D_COND_WAIT,
	D_JOIN,
	D_LOCK,
	D_SYSTEM,
	D_TESTCANCEL,
	DUES,
};

static const char *const DUE_NAMES[DUES] = {
		"read", "write", "recv", "send", "accept", "poll", "select", "open", "openat",
		"creat", "close", "pread", "pwrite", "fsync", "fdatasync", "msync", "tcdrain",
		"nanosleep 0", "sleep 0", "clock_nanosleep, its time past", "sigtimedwait",
		"waitpid", "sem_wait", "pthread_cond_wait", "pthread_join", "fcntl F_OFD_SETLKW",
		"system", "pthread_testcancel",
};

struct due_case {
	enum due due;
	sem_t ready;
	sem_t go;
	int cleaned;
	int returned;
	int held;
	int still_pending;
	int dup;
};

static void *returns(void *argument) {
	(void)argument;
	return NULL;
}

static void prepare_due(struct due_case *d) {
	switch(d->due) {
	case D_READ:
	case D_POLL:
	case D_SELECT:
		CHECK(pipe(fx.pipe) == 0);
		CHECK(write(fx.pipe[1], "x", 1) == 1);
		break;
	case D_WRITE:
	case D_TCDRAIN:
		CHECK(pipe(fx.pipe) == 0);
		break;
	case D_RECV:
		CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fx.pair) == 0);
		CHECK(send(fx.pair[1], "x", 1, 0) == 1);
		break;
	case D_SEND:
		CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fx.pair) == 0);
		break;
	case D_ACCEPT:
		open_listener(4);
		fx.clients[0] = connect_to_listener(0);
		CHECK(fx.clients[0] >= 0);
		fx.queued = 1;
		break;
	case D_OPEN:
	case D_OPENAT:
	case D_CREAT:
		unlink(NEW_PATH);
		break;
	case D_CLOSE:
		CHECK(pipe(fx.pipe) == 0);
		d->dup = dup(fx.pipe[0]);
		CHECK(d->dup >= 0);
		break;
	case D_PREAD:
	case D_PWRITE:
	case D_FSYNC:
	case D_FDATASYNC:
		fx.file = open(LOCK_PATH, O_RDWR | O_CREAT | O_TRUNC, 0644);
		CHECK(fx.file >= 0);
		CHECK(write(fx.file, "abc", 3) == 3);
		break;
	case D_MSYNC:
		fx.page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		CHECK(fx.page != MAP_FAILED);
		break;
	case D_SIGTIMEDWAIT:
		sigemptyset(&fx.usr2);
		sigaddset(&fx.usr2, SIGUSR2);
		break;
	case D_WAITPID: {
		// A child that has ended, not yet waited for.
		make_child(7);
		siginfo_t info;
		CHECK(waitid(P_PID, (id_t)fx.child, &info, WEXITED | WNOWAIT) == 0);
		break;
	}
	case D_SEM_WAIT:
		// A semaphore that need not be waited for.
		CHECK(sem_init(&fx.sem, 0, 1) == 0);
		break;
	case D_COND_WAIT: {
		pthread_mutexattr_t attr;
		pthread_mutexattr_init(&attr);
		pthread_mutexattr_settype(&attr, PTHREAD_MUTEX_ERRORCHECK);
		CHECK(pthread_mutex_init(&fx.mutex, &attr) == 0);
		pthread_mutexattr_destroy(&attr);
		CHECK(pthread_cond_init(&fx.cond, NULL) == 0);
		break;
	}
	case D_JOIN:
		// A thread that has returned, not yet joined.
		CHECK(pthread_create(&fx.other, NULL, returns, NULL) == 0);
		check_sleep_ms(20);
		break;
	case D_LOCK:
		fx.lock[0] = open(LOCK_PATH, O_RDWR | O_CREAT | O_TRUNC, 0644);
		fx.lock[1] = open(LOCK_PATH, O_RDWR);
		CHECK(fx.lock[0] >= 0 && fx.lock[1] >= 0);
		break;
	case D_SYSTEM:
		unlink(SYSTEM_PATH);
		break;
	default:
		break;
	}
}

static void call_due(struct due_case *d) {
	char byte;
	struct timespec zero = {0, 0};
	struct pollfd entry = {fx.pipe[0], POLLIN, 0};
	switch(d->due) {
	case D_READ:
		read(fx.pipe[0], &byte, 1);
		break;
	case D_WRITE:
		write(fx.pipe[1], "x", 1);
		break;
	case D_RECV:
		recv(fx.pair[0], &byte, 1, 0);
		break;
	case D_SEND:
		send(fx.pair[0], "x", 1, 0);
		break;
	case D_ACCEPT:
		accept(fx.listener, NULL, NULL);
		break;
	case D_POLL:
		poll(&entry, 1, -1);
		break;
	case D_SELECT: {
		fd_set set;
		FD_ZERO(&set);
		FD_SET(fx.pipe[0], &set);
		select(fx.pipe[0] + 1, &set, NULL, NULL, NULL);
		break;
	}
	case D_OPEN:
		open(NEW_PATH, O_WRONLY | O_CREAT, 0644);
		break;
	case D_OPENAT:
		openat(AT_FDCWD, NEW_PATH, O_WRONLY | O_CREAT, 0644);
		break;
	case D_CREAT:
		creat(NEW_PATH, 0644);
		break;
	case D_CLOSE:
		close(d->dup);
		break;
	case D_PREAD:
		pread(fx.file, &byte, 1, 0);
		break;
	case D_PWRITE:
		pwrite(fx.file, "z", 1, 0);
		break;
	case D_FSYNC:
		fsync(fx.file);
		break;
	case D_FDATASYNC:
		fdatasync(fx.file);
		break;
	case D_MSYNC:
		msync(fx.page, 4096, MS_SYNC);
		break;
	case D_TCDRAIN:
		tcdrain(fx.pipe[1]);
		break;
	case D_NANOSLEEP:
		nanosleep(&zero, NULL);
		break;
	case D_SLEEP:
		sleep(0);
		break;
	case D_CLOCK_NANOSLEEP:
		clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &zero, NULL);
		break;
	case D_SIGTIMEDWAIT:
		sigtimedwait(&fx.usr2, NULL, &zero);
		break;
	case D_WAITPID:
		waitpid(fx.child, NULL, 0);
		break;
	case D_SEM_WAIT:
		sem_wait(&fx.sem);
		break;
	case D_COND_WAIT:
		pthread_cond_wait(&fx.cond, &fx.mutex);
		pthread_mutex_unlock(&fx.mutex);
		break;
	case D_JOIN:
		pthread_join(fx.other, NULL);
		break;
	case D_LOCK: {
		struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
		fcntl(fx.lock[1], F_OFD_SETLKW, &whole);
		break;
	}
	case D_SYSTEM:
		system("touch " SYSTEM_PATH);
		break;
	case D_TESTCANCEL:
		pthread_testcancel();
		break;
	default:
		break;
	}
}

static void due_cleanup(void *argument) {
	struct due_case *d = argument;
	if(d->due == D_COND_WAIT)
		d->held = pthread_mutex_unlock(&fx.mutex) == 0;
	if(d->due == D_SIGTIMEDWAIT) {
		sigset_t pending;
		sigpending(&pending);
		d->still_pending = sigismember(&pending, SIGUSR2) == 1;
	}
	d->cleaned = 1;
}

static void *comes_to_point(void *argument) {
	struct due_case *d = argument;
	pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, NULL);
	if(d->due == D_SIGTIMEDWAIT)
		pthread_sigmask(SIG_BLOCK, &fx.usr2, NULL);
	if(d->due == D_COND_WAIT)
		pthread_mutex_lock(&fx.mutex);
	pthread_cleanup_push(due_cleanup, d);
	sem_post(&d->ready);
	wait_on(&d->go);
	pthread_setcancelstate(PTHREAD_CANCEL_ENABLE, NULL);
	call_due(d);
	d->returned = 1;
	pthread_cleanup_pop(0);
	return NULL;
}

// That the call that was cancelled did nothing; then puts the case away.
static void check_nothing_done(struct due_case *d) {
	const char *name = DUE_NAMES[d->due];
	char byte;
	struct stat st;
	switch(d->due) {
	case D_READ:
	case D_POLL:
	case D_SELECT:
		set_blocking(fx.pipe[0], 0);
		CHECK_CASE(read(fx.pipe[0], &byte, 1) == 1, name);
		close(fx.pipe[0]);
		close(fx.pipe[1]);
		break;
	case D_WRITE:
		set_blocking(fx.pipe[0], 0);
		CHECK_CASE(read(fx.pipe[0], &byte, 1) == -1 && errno == EAGAIN, name);
		close(fx.pipe[0]);
		close(fx.pipe[1]);
		break;
	case D_TCDRAIN:
		close(fx.pipe[0]);
		close(fx.pipe[1]);
		break;
	case D_RECV:
		CHECK_CASE(recv(fx.pair[0], &byte, 1, MSG_DONTWAIT) == 1, name);
		close(fx.pair[0]);
		close(fx.pair[1]);
		break;
	case D_SEND:
		CHECK_CASE(recv(fx.pair[1], &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN, name);
		close(fx.pair[0]);
		close(fx.pair[1]);
		break;
	case D_ACCEPT: {
		// The connection still waits to be taken.
		set_blocking(fx.listener, 0);
		int taken = accept(fx.listener, NULL, NULL);
		CHECK_CASE(taken >= 0, name);
		close(taken);
		close_listener();
		break;
	}
	case D_OPEN:
	case D_OPENAT:
	case D_CREAT:
		CHECK_CASE(stat(NEW_PATH, &st) == -1 && errno == ENOENT, name);
		unlink(NEW_PATH);
		break;
	case D_CLOSE:
		CHECK_CASE(fcntl(d->dup, F_GETFD) >= 0, name);
		close(d->dup);
		close(fx.pipe[0]);
		close(fx.pipe[1]);
		break;
	case D_PWRITE:
		CHECK_CASE(pread(fx.file, &byte, 1, 0) == 1 && byte == 'a', name);
		close(fx.file);
		unlink(LOCK_PATH);
		break;
	case D_PREAD:
	case D_FSYNC:
	case D_FDATASYNC:
		close(fx.file);
		unlink(LOCK_PATH);
		break;
	case D_MSYNC:
		munmap(fx.page, 4096);
		break;
	case D_SIGTIMEDWAIT:
		// The signal was left pending.
		CHECK_CASE(d->still_pending, name);
		break;
	case D_WAITPID: {
		int status = 0;
		CHECK_CASE(waitpid(fx.child, &status, WNOHANG) == fx.child, name);
		CHECK_CASE(WIFEXITED(status) && WEXITSTATUS(status) == 7, name);
		break;
	}
	case D_SEM_WAIT: {
		int value = -1;
		CHECK_CASE(sem_getvalue(&fx.sem, &value) == 0 && value == 1, name);
		sem_destroy(&fx.sem);
		break;
	}
	case D_COND_WAIT:
		CHECK_CASE(d->held, name);
		pthread_cond_destroy(&fx.cond);
		pthread_mutex_destroy(&fx.mutex);
		break;
	case D_JOIN:
		CHECK_CASE(pthread_join(fx.other, NULL) == 0, name);
		break;
	case D_LOCK: {
		// The lock was not taken: the other description can have it.
		struct flock whole = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
		CHECK_CASE(fcntl(fx.lock[0], F_OFD_SETLK, &whole) == 0, name);
		close(fx.lock[0]);
		close(fx.lock[1]);
		unlink(LOCK_PATH);
		break;
	}
	case D_SYSTEM:
		CHECK_CASE(stat(SYSTEM_PATH, &st) == -1 && errno == ENOENT, name);
		unlink(SYSTEM_PATH);
		break;
	default:
		break;
	}
}

static void due(void) {
	for(int i = 0; i < DUES; i++) {
		const char *name = DUE_NAMES[i];
		struct due_case d = {.due = i};
		sem_init(&d.ready, 0, 0);
		sem_init(&d.go, 0, 0);
		prepare_due(&d);
		pthread_t thread;
		CHECK_CASE(pthread_create(&thread, NULL, comes_to_point, &d) == 0, name);
		wait_on(&d.ready);
		if(i == D_SIGTIMEDWAIT)
			pthread_kill(thread, SIGUSR2);
		CHECK_CASE(pthread_cancel(thread) == 0, name);
		sem_post(&d.go);
		void *result = NULL;
		CHECK_CASE(pthread_join(thread, &result) == 0, name);
		CHECK_CASE(result == PTHREAD_CANCELED && d.cleaned && !d.returned, name);
		check_nothing_done(&d);
		sem_destroy(&d.ready);
		sem_destroy(&d.go);
	}
}

// What is no cancellation point goes on with a cancel due, and the waits
// that are none among it, the cancel coming while it waits in one, until
// the thread comes to a point; stdio, popen and pclose, opendir and
// closedir are none, as on musl.
struct going_on {
	// Whether the cancel comes in the timed lock rather than the other.
	int timed_first;
	sem_t ready;
	int locked;
	int timed;
	int other;
	int wrote;
	int read_back;
	int closed;
	int opened;
	int listed;
	int pclosed;
	int reached;
};

static pthread_mutex_t held = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t timed = PTHREAD_MUTEX_INITIALIZER;

// Locks `timed` with a deadline far off: neither EINTR nor ETIMEDOUT.
static int lock_timed(void) {
	struct timespec deadline;
	clock_gettime(CLOCK_REALTIME, &deadline);
	deadline.tv_sec += 30;
	int e = pthread_mutex_timedlock(&timed, &deadline);
	if(!e)
		pthread_mutex_unlock(&timed);
	return e == 0;
}

static int lock_held(void) {
	int e = pthread_mutex_lock(&held);
	if(!e)
		pthread_mutex_unlock(&held);
	return e == 0;
}

static void *goes_on(void *argument) {
	struct going_on *g = argument;
	sem_post(&g->ready);
	if(g->timed_first) {
		g->timed = lock_timed();
		g->locked = lock_held();
	} else {
		g->locked = lock_held();
		g->timed = lock_timed();
	}
	// Calls that make no wait.
	void *memory = malloc(64);
	free(memory);
	sigset_t mask;
	sem_t sem;
	sem_init(&sem, 0, 0);
	pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
	struct stat st;
	g->other = getpid() > 0 && pthread_sigmask(SIG_BLOCK, NULL, &mask) == 0
			&& sem_post(&sem) == 0 && sem_trywait(&sem) == 0
			&& pthread_cond_signal(&cond) == 0 && stat("/tmp", &st) == 0;
	sem_destroy(&sem);
	// stdio reads and writes, opens and closes.
	int ends[2];
	if(pipe(ends) == 0) {
		int copy = dup(ends[0]);
		g->other = g->other && copy >= 0 && fcntl(copy, F_GETFL) >= 0
				&& lseek(copy, 0, SEEK_CUR) == -1 && errno == ESPIPE;
		FILE *out = fdopen(ends[1], "w");
		FILE *in = fdopen(copy, "r");
		char line[16] = "";
		g->wrote = out && fputs("line\n", out) >= 0 && fflush(out) == 0;
		g->read_back = in && fgets(line, sizeof(line), in) && !strcmp(line, "line\n");
		g->closed = (!out || fclose(out) == 0) && (!in || fclose(in) == 0);
		// The descriptor fdopen did not take, which close would not leave
		// open (close is a point).
		g->closed = g->closed && fcntl(ends[0], F_GETFD) >= 0;
	}
	FILE *file = fopen(NEW_PATH, "w");
	g->opened = file && fputs("x", file) >= 0 && fclose(file) == 0;
	DIR *dir = opendir("/tmp");
	g->listed = dir && readdir(dir) && closedir(dir) == 0;
	// pclose waits for its command to the end.
	FILE *command = popen("exit 5", "r");
	g->pclosed = command && pclose(command) == 5 << 8;
	g->reached = 1;
	pthread_testcancel();
	return NULL;
}

static void going_on_with(int timed_first) {
	struct going_on g = {.timed_first = timed_first};
	sem_init(&g.ready, 0, 0);
	pthread_mutex_lock(&held);
	pthread_mutex_lock(&timed);
	pthread_t thread;
	CHECK(pthread_create(&thread, NULL, goes_on, &g) == 0);
	wait_on(&g.ready);
	check_sleep_ms(20);
	CHECK(pthread_cancel(thread) == 0);
	check_sleep_ms(20);
	pthread_mutex_unlock(timed_first ? &timed : &held);
	check_sleep_ms(20);
	pthread_mutex_unlock(timed_first ? &held : &timed);
	void *result = NULL;
	CHECK(pthread_join(thread, &result) == 0);
	CHECK(result == PTHREAD_CANCELED);
	CHECK(g.locked && g.timed);
	CHECK(g.other);
	CHECK(g.wrote && g.read_back && g.closed);
	CHECK(g.opened && g.listed);
	CHECK(g.pclosed);
	CHECK(g.reached);
	// pclose took its child.
	errno = 0;
	CHECK(waitpid(-1, NULL, WNOHANG) == -1 && errno == ECHILD);
	unlink(NEW_PATH);
	sem_destroy(&g.ready);
}

static void going_on(void) {
	going_on_with(0);
	going_on_with(1);
}

// With cancellation disabled, the points do their work, a cancel due or
// not, and the cancel waits until it is enabled again.
static void *disabled_thread(void *argument) {
	int *done = argument;
	pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, NULL);
	sem_post(&fx.sem);
	wait_on(&fx.other_go);
	char byte = 0;
	int fd = open(NEW_PATH, O_WRONLY | O_CREAT | O_TRUNC, 0644);
	struct timespec tick = {0, 1000000};
	done[0] = fd >= 0 && write(fd, "x", 1) == 1 && close(fd) == 0;
	done[1] = read(fx.pipe[0], &byte, 1) == 1 && byte == 'y' && nanosleep(&tick, NULL) == 0;
	pthread_testcancel();
	done[2] = 1;
	pthread_setcancelstate(PTHREAD_CANCEL_ENABLE, NULL);
	// The cancel is acted on here, at the next point.
	pthread_testcancel();
	done[3] = 1;
	return NULL;
}

static void disabled(void) {
	CHECK(pipe(fx.pipe) == 0);
	CHECK(sem_init(&fx.sem, 0, 0) == 0 && sem_init(&fx.other_go, 0, 0) == 0);
	int done[4] = {0};
	pthread_t thread;
	CHECK(pthread_create(&thread, NULL, disabled_thread, done) == 0);
	wait_on(&fx.sem);
	CHECK(pthread_cancel(thread) == 0);
	sem_post(&fx.other_go);
	// The read waits, and the cancel does not end it.
	check_sleep_ms(20);
	CHECK(write(fx.pipe[1], "y", 1) == 1);
	void *result = NULL;
	CHECK(pthread_join(thread, &result) == 0 && result == PTHREAD_CANCELED);
	CHECK(done[0] && done[1] && done[2] && !done[3]);
	struct stat st;
	CHECK(stat(NEW_PATH, &st) == 0 && st.st_size == 1);
	unlink(NEW_PATH);
	close(fx.pipe[0]);
	close(fx.pipe[1]);
	sem_destroy(&fx.sem);
	sem_destroy(&fx.other_go);
}

// Cleanup handlers run last pushed first, each to its end, though they
// call cancellation points; then keys' destructors.
static int order_pipe[2];
static pthread_key_t order_key;

static void note(void *argument) {
	write(order_pipe[1], argument, 1);
	// Cancellation points, which act on nothing now.
	struct timespec tick = {0, 1000000};
	nanosleep(&tick, NULL);
	int spare = dup(order_pipe[1]);
	close(spare);
}

static void *cleans_up(void *argument) {
	(void)argument;
	pthread_setspecific(order_key, "k");
	pthread_cleanup_push(note, "a");
	pthread_cleanup_push(note, "b");
	pthread_cleanup_push(note, "c");
	for(;;)
		pause();
	pthread_cleanup_pop(0);
	pthread_cleanup_pop(0);
	pthread_cleanup_pop(0);
	return NULL;
}

static void cleanup_order(void) {
	CHECK(pipe(order_pipe) == 0);
	CHECK(pthread_key_create(&order_key, note) == 0);
	pthread_t thread;
	CHECK(pthread_create(&thread, NULL, cleans_up, NULL) == 0);
	check_sleep_ms(20);
	CHECK(pthread_cancel(thread) == 0);
	void *result = NULL;
	CHECK(pthread_join(thread, &result) == 0 && result == PTHREAD_CANCELED);
	char order[8] = "";
	set_blocking(order_pipe[0], 0);
	CHECK(read(order_pipe[0], order, sizeof(order) - 1) == 4);
	CHECK(!strcmp(order, "cbak"));
	pthread_key_delete(order_key);
	close(order_pipe[0]);
	close(order_pipe[1]);
}

// A cancel that comes while another signal's handler runs, over a wait at
// a cancellation point that the handler's SA_RESTART makes again, ends
// the wait once the handler returns.
static atomic_int in_handler;
static atomic_int cancel_sent;

static void spins(int signal) {
	(void)signal;
	atomic_store(&in_handler, 1);
	while(!atomic_load(&cancel_sent))
		;
}

static void blocked_cleaned(void *argument) {
	*(atomic_int *)argument = 1;
}

static void *reads_forever(void *argument) {
	char byte;
	pthread_cleanup_push(blocked_cleaned, argument);
	read(fx.pipe[0], &byte, 1);
	pthread_cleanup_pop(0);
	return NULL;
}

static void nested(void) {
	struct sigaction action = {.sa_handler = spins, .sa_flags = SA_RESTART};
	sigemptyset(&action.sa_mask);
	struct sigaction old;
	CHECK(sigaction(SIGUSR1, &action, &old) == 0);
	CHECK(pipe(fx.pipe) == 0);
	atomic_int cleaned = 0;
	pthread_t thread;
	CHECK(pthread_create(&thread, NULL, reads_forever, &cleaned) == 0);
	check_sleep_ms(20);
	CHECK(pthread_kill(thread, SIGUSR1) == 0);
	while(!atomic_load(&in_handler))
		check_sleep_ms(1);
	CHECK(pthread_cancel(thread) == 0);
	atomic_store(&cancel_sent, 1);
	for(int i = 0; i < 1000 && !atomic_load(&cleaned); i++)
		check_sleep_ms(5);
	CHECK(atomic_load(&cleaned));
	// Lets the read end if the cancel did not.
	if(!atomic_load(&cleaned))
		write(fx.pipe[1], "x", 1);
	void *result = NULL;
	CHECK(pthread_join(thread, &result) == 0 && result == PTHREAD_CANCELED);
	close(fx.pipe[0]);
	close(fx.pipe[1]);
	sigaction(SIGUSR1, &old, NULL);
}

// Two threads' system: SIGINT is ignored while either's command runs, and
// back as it was once both are done, as on glibc (musl puts back what each
// found, so the first to start puts SIGINT back while the other's command
// still runs).
struct command {
	const char *line;
	int status;
};

static void *runs(void *argument) {
	struct command *c = argument;
	c->status = system(c->line);
	return NULL;
}

static void systems(void) {
	void (*sigint)(int) = sigint_handler();
	struct command first = {"sleep 1", -1};
	struct command second = {"sleep 3", -1};
	pthread_t threads[2];
	CHECK(pthread_create(&threads[0], NULL, runs, &first) == 0);
	for(int i = 0; i < 1000 && sigint_handler() != SIG_IGN; i++)
		check_sleep_ms(5);
	CHECK(pthread_create(&threads[1], NULL, runs, &second) == 0);
	CHECK(pthread_join(threads[0], NULL) == 0 && first.status == 0);
	CHECK(sigint_handler() == SIG_IGN);
	CHECK(pthread_join(threads[1], NULL) == 0 && second.status == 0);
	CHECK(sigint_handler() == sigint);
}

int main(void) {
	check_start("cancel");
	// A cancel that is never acted on leaves its thread waiting.
	alarm(120);
	RUN(waiting);
	RUN(due);
	RUN(going_on);
	RUN(disabled);
	RUN(cleanup_order);
	RUN(nested);
	RUN(systems);
	return check_end();
}
