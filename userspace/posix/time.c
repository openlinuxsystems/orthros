// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Clocks and timers: the clocks clock_gettime reads, sleeping, CPU time
// as times and getrusage count it, the interval timers, and POSIX's
// timers (timer_create), with their overruns.

#include <pthread.h>
#include <stdint.h>
#include <sys/resource.h>
#include <sys/time.h>
#include <sys/times.h>

#include "check.h"

static long long ns(const struct timespec *ts) {
	return (long long)ts->tv_sec * 1000000000LL + ts->tv_nsec;
}

// The CPU time in `usage`, user and system, in microseconds. A spin's
// time is both: each clock read is a system call on Radix, and the
// kernel splits a thread's time by where the timer finds it.
static long long cpu_us(const struct rusage *usage) {
	return (usage->ru_utime.tv_sec + usage->ru_stime.tv_sec) * 1000000LL +
			usage->ru_utime.tv_usec + usage->ru_stime.tv_usec;
}

// Spins for `ms` milliseconds of the monotonic clock.
static void spin(long ms) {
	long long end = check_now_ns() + ms * 1000000LL;
	volatile unsigned long n = 0;
	while(check_now_ns() < end)
		n++;
}

// Spins for `ms` milliseconds in user mode, reading the clock seldom.
static void spin_user(long ms) {
	long long end = check_now_ns() + ms * 1000000LL;
	volatile unsigned long n = 0;
	do {
		for(int i = 0; i < 1 << 16; i++)
			n++;
	} while(check_now_ns() < end);
}

static void clocks(void) {
	static const clockid_t ids[] = {CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID,
			CLOCK_THREAD_CPUTIME_ID, CLOCK_MONOTONIC_RAW, CLOCK_REALTIME_COARSE,
			CLOCK_MONOTONIC_COARSE, CLOCK_BOOTTIME};
	for(size_t i = 0; i < sizeof(ids) / sizeof(ids[0]); i++) {
		struct timespec ts, res;
		CHECK(clock_gettime(ids[i], &ts) == 0);
		CHECK(ts.tv_nsec >= 0 && ts.tv_nsec < 1000000000);
		CHECK(clock_getres(ids[i], &res) == 0 && ns(&res) > 0 && ns(&res) <= 10000000);
	}
	struct timespec ts;
	CHECK_ERROR(clock_gettime(12345, &ts), EINVAL);
	// The wall clock is past 2024, and agrees with gettimeofday and time.
	struct timespec wall;
	CHECK(clock_gettime(CLOCK_REALTIME, &wall) == 0 && wall.tv_sec > 1704067200);
	struct timeval tv;
	CHECK(gettimeofday(&tv, NULL) == 0);
	CHECK(tv.tv_sec - wall.tv_sec >= 0 && tv.tv_sec - wall.tv_sec <= 1);
	time_t now = time(NULL);
	CHECK(now - wall.tv_sec >= 0 && now - wall.tv_sec <= 1);
	struct tm tm;
	CHECK(gmtime_r(&now, &tm) && tm.tm_year >= 124);
	char text[64];
	CHECK(strftime(text, sizeof(text), "%Y-%m-%d %H:%M:%S", &tm) == 19);
	// The monotonic clock never goes back.
	long long last = check_now_ns(), back = 0;
	for(int i = 0; i < 10000; i++) {
		long long t = check_now_ns();
		back += t < last;
		last = t;
	}
	CHECK(back == 0);
}

static void sleeps(void) {
	static const long lengths[] = {1, 10, 50};
	for(size_t i = 0; i < sizeof(lengths) / sizeof(lengths[0]); i++) {
		long long start = check_now_ns();
		struct timespec length = {0, lengths[i] * 1000000L};
		CHECK(nanosleep(&length, NULL) == 0);
		long long took = check_now_ns() - start;
		CHECK(took >= lengths[i] * 1000000LL && took < lengths[i] * 1000000LL + 500000000LL);
	}
	long long start = check_now_ns();
	CHECK(usleep(20000) == 0);
	CHECK(check_now_ns() - start >= 20000000);
	struct timespec bad = {0, 1000000000};
	CHECK_ERROR(nanosleep(&bad, NULL), EINVAL);
	// Until a time on the monotonic clock.
	struct timespec until;
	clock_gettime(CLOCK_MONOTONIC, &until);
	until.tv_nsec += 30000000;
	if(until.tv_nsec >= 1000000000) {
		until.tv_sec++;
		until.tv_nsec -= 1000000000;
	}
	CHECK(clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &until, NULL) == 0);
	CHECK(check_now_ns() >= ns(&until));
	// clock_nanosleep returns its error, on Linux's clocks: those that
	// drive no timers are not supported, a thread's CPU time and unknown
	// clocks are invalid.
	struct timespec short_one = {0, 1000000};
	CHECK(clock_nanosleep(CLOCK_BOOTTIME, 0, &short_one, NULL) == 0);
	CHECK(clock_nanosleep(CLOCK_TAI, 0, &short_one, NULL) == 0);
	CHECK(clock_nanosleep(CLOCK_MONOTONIC_RAW, 0, &short_one, NULL) == EOPNOTSUPP);
	CHECK(clock_nanosleep(CLOCK_REALTIME_COARSE, 0, &short_one, NULL) == EOPNOTSUPP);
	CHECK(clock_nanosleep(CLOCK_THREAD_CPUTIME_ID, 0, &short_one, NULL) == EINVAL);
	CHECK(clock_nanosleep(12345, 0, &short_one, NULL) == EINVAL);
	errno = 0;
	CHECK(clock_nanosleep(CLOCK_MONOTONIC, 0, &bad, NULL) == EINVAL && errno == 0);
	struct timespec negative = {-1, 0};
	CHECK(clock_nanosleep(CLOCK_MONOTONIC, 0, &negative, NULL) == EINVAL);
	CHECK(clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &negative, NULL) == EINVAL);
}

static void on_interrupt(int signal) {
	(void)signal;
}

// A signal whose handler does not restart calls ends a sleep with EINTR,
// returned, and a relative one says what was left of it.
static void interrupted_sleeps(void) {
	struct sigaction action = {0};
	action.sa_handler = on_interrupt;
	sigemptyset(&action.sa_mask);
	sigaction(SIGALRM, &action, NULL);
	struct itimerval soon = {{0, 0}, {0, 20000}};
	CHECK(setitimer(ITIMER_REAL, &soon, NULL) == 0);
	struct timespec long_one = {10, 0}, left = {0, 0};
	CHECK(clock_nanosleep(CLOCK_MONOTONIC, 0, &long_one, &left) == EINTR);
	CHECK(left.tv_sec >= 8 && left.tv_sec <= 10);
	CHECK(setitimer(ITIMER_REAL, &soon, NULL) == 0);
	struct timespec until;
	clock_gettime(CLOCK_MONOTONIC, &until);
	until.tv_sec += 10;
	left = (struct timespec){-1, -1};
	CHECK(clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &until, &left) == EINTR);
	CHECK(left.tv_sec == -1 && left.tv_nsec == -1);
	signal(SIGALRM, SIG_DFL);
}

static void cpu_time(void) {
	struct timespec process_before, thread_before, process_after, thread_after;
	clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &process_before);
	clock_gettime(CLOCK_THREAD_CPUTIME_ID, &thread_before);
	struct tms before;
	clock_t start = times(&before);
	CHECK(start != (clock_t)-1);
	spin(100);
	struct tms after;
	clock_t end = times(&after);
	clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &process_after);
	clock_gettime(CLOCK_THREAD_CPUTIME_ID, &thread_after);
	long ticks = sysconf(_SC_CLK_TCK);
	CHECK(ticks == 100);
	CHECK(end - start >= 9 && end - start < 100);
	CHECK(after.tms_utime + after.tms_stime - before.tms_utime - before.tms_stime >= 5);
	CHECK(ns(&process_after) - ns(&process_before) >= 50000000);
	CHECK(ns(&thread_after) - ns(&thread_before) >= 50000000);
	struct rusage usage;
	CHECK(getrusage(RUSAGE_SELF, &usage) == 0);
	CHECK(cpu_us(&usage) >= 50000);
	// Time in user mode is user time.
	long long user_before = usage.ru_utime.tv_sec * 1000000LL + usage.ru_utime.tv_usec;
	spin_user(100);
	CHECK(getrusage(RUSAGE_SELF, &usage) == 0);
	CHECK(usage.ru_utime.tv_sec * 1000000LL + usage.ru_utime.tv_usec - user_before >= 50000);
	// A child's time counts for its parent once the parent has waited.
	struct rusage children;
	CHECK(getrusage(RUSAGE_CHILDREN, &children) == 0);
	long long before_children = cpu_us(&children);
	pid_t pid = check_fork();
	if(pid == 0) {
		spin(100);
		_exit(0);
	}
	int status;
	struct rusage child;
	CHECK(wait4(pid, &status, 0, &child) == pid);
	CHECK(cpu_us(&child) >= 50000);
	CHECK(getrusage(RUSAGE_CHILDREN, &children) == 0);
	CHECK(cpu_us(&children) - before_children >= 50000);
	struct tms mine;
	times(&mine);
	CHECK(mine.tms_cutime + mine.tms_cstime >= 5);
}

static volatile sig_atomic_t alarms;

static void on_alarm(int signal) {
	(void)signal;
	alarms++;
}

static void timers(void) {
	struct sigaction action = {0};
	action.sa_handler = on_alarm;
	action.sa_flags = SA_RESTART;
	sigemptyset(&action.sa_mask);
	sigaction(SIGALRM, &action, NULL);
	sigaction(SIGVTALRM, &action, NULL);
	sigaction(SIGPROF, &action, NULL);
	// An interval timer goes off again and again.
	alarms = 0;
	struct itimerval every = {{0, 10000}, {0, 10000}};
	CHECK(setitimer(ITIMER_REAL, &every, NULL) == 0);
	long long start = check_now_ns();
	while(alarms < 5 && check_now_ns() - start < 2000000000LL)
		pause();
	struct itimerval off = {{0, 0}, {0, 0}}, old;
	CHECK(setitimer(ITIMER_REAL, &off, &old) == 0);
	CHECK(alarms >= 5);
	CHECK(old.it_interval.tv_usec == 10000);
	CHECK(check_now_ns() - start >= 40000000);
	// One that is off stays off.
	int was = alarms;
	check_sleep_ms(30);
	CHECK(alarms == was);
	// The user-time and profiling timers count CPU time.
	static const int which[] = {ITIMER_VIRTUAL, ITIMER_PROF};
	for(int i = 0; i < 2; i++) {
		alarms = 0;
		struct itimerval once = {{0, 0}, {0, 20000}};
		CHECK(setitimer(which[i], &once, NULL) == 0);
		start = check_now_ns();
		while(!alarms && check_now_ns() - start < 2000000000LL)
			;
		CHECK(alarms == 1);
	}
	// alarm() says what was left of the last one.
	CHECK(alarm(5) == 0);
	CHECK(alarm(0) >= 4);
	CHECK_ERROR(setitimer(99, &off, NULL), EINVAL);
	// Microseconds are 0 to 999,999.
	struct itimerval wrong = {{0, 0}, {0, 1000000}};
	CHECK_ERROR(setitimer(ITIMER_REAL, &wrong, NULL), EINVAL);
	wrong.it_value.tv_usec = -1;
	CHECK_ERROR(setitimer(ITIMER_REAL, &wrong, NULL), EINVAL);
	signal(SIGALRM, SIG_DFL);
	signal(SIGVTALRM, SIG_DFL);
	signal(SIGPROF, SIG_DFL);
}

// A timer on `clock` that sends `signal` carrying `value`.
static timer_t timer_for(clockid_t clock, int signal, int value) {
	struct sigevent event = {0};
	event.sigev_notify = SIGEV_SIGNAL;
	event.sigev_signo = signal;
	event.sigev_value.sival_int = value;
	timer_t timer = 0;
	CHECK(timer_create(clock, &event, &timer) == 0);
	return timer;
}

// Sets `timer` to expire in `value_ms`, then every `interval_ms`.
static int timer_ms(timer_t timer, int flags, long value_ms, long interval_ms) {
	struct itimerspec spec = {{interval_ms / 1000, interval_ms % 1000 * 1000000L},
			{value_ms / 1000, value_ms % 1000 * 1000000L}};
	return timer_settime(timer, flags, &spec, NULL);
}

static volatile int thread_calls;
static volatile int thread_value;

static void on_timer(union sigval value) {
	thread_value = value.sival_int;
	__atomic_add_fetch(&thread_calls, 1, __ATOMIC_RELAXED);
}

static void posix_timers(void) {
	int rt = SIGRTMIN + 2;
	sigset_t set;
	sigemptyset(&set);
	sigaddset(&set, rt);
	sigaddset(&set, SIGALRM);
	sigprocmask(SIG_BLOCK, &set, NULL);
	siginfo_t info;
	// One expiry: its signal carries the value and says it is a timer's.
	timer_t timer = timer_for(CLOCK_MONOTONIC, rt, 42);
	long long start = check_now_ns();
	CHECK(timer_ms(timer, 0, 20, 0) == 0);
	struct itimerspec left;
	CHECK(timer_gettime(timer, &left) == 0);
	CHECK(left.it_value.tv_sec == 0 && left.it_value.tv_nsec > 0);
	CHECK(left.it_value.tv_nsec <= 20000000);
	CHECK(sigwaitinfo(&set, &info) == rt);
	CHECK(check_now_ns() - start >= 20000000);
	CHECK(info.si_code == SI_TIMER && info.si_value.sival_int == 42 && info.si_overrun == 0);
	CHECK(timer_gettime(timer, &left) == 0 && left.it_value.tv_sec == 0);
	CHECK(left.it_value.tv_nsec == 0);
	// Expiries while its signal waits count as its overrun.
	CHECK(timer_ms(timer, 0, 1, 1) == 0);
	check_sleep_ms(50);
	CHECK(sigwaitinfo(&set, &info) == rt);
	CHECK(info.si_overrun >= 20);
	CHECK(timer_getoverrun(timer) == info.si_overrun);
	CHECK(timer_gettime(timer, &left) == 0 && left.it_interval.tv_nsec == 1000000);
	// A deleted timer's signal goes with it.
	CHECK(timer_ms(timer, 0, 1, 0) == 0);
	check_sleep_ms(10);
	sigset_t pending;
	CHECK(sigpending(&pending) == 0 && sigismember(&pending, rt));
	CHECK(timer_delete(timer) == 0);
	CHECK(sigpending(&pending) == 0 && !sigismember(&pending, rt));
	CHECK_ERROR(timer_gettime(timer, &left), EINVAL);
	// An absolute time on the wall clock.
	timer = timer_for(CLOCK_REALTIME, rt, 7);
	struct timespec wall;
	clock_gettime(CLOCK_REALTIME, &wall);
	wall.tv_nsec += 20000000;
	if(wall.tv_nsec >= 1000000000) {
		wall.tv_sec++;
		wall.tv_nsec -= 1000000000;
	}
	struct itimerspec at = {{0, 0}, wall};
	start = check_now_ns();
	CHECK(timer_settime(timer, TIMER_ABSTIME, &at, NULL) == 0);
	CHECK(sigwaitinfo(&set, &info) == rt && info.si_value.sival_int == 7);
	CHECK(check_now_ns() - start >= 15000000);
	// A time already past expires at once.
	at.it_value.tv_sec -= 10;
	CHECK(timer_settime(timer, TIMER_ABSTIME, &at, NULL) == 0);
	CHECK(sigwaitinfo(&set, &info) == rt);
	CHECK(timer_delete(timer) == 0);
	// One counting the process's CPU time.
	timer = timer_for(CLOCK_PROCESS_CPUTIME_ID, rt, 9);
	CHECK(timer_ms(timer, 0, 20, 0) == 0);
	struct timespec none = {0, 0};
	start = check_now_ns();
	int fired = 0;
	while(!fired && check_now_ns() - start < 2000000000LL)
		fired = sigtimedwait(&set, &info, &none) == rt;
	CHECK(fired && info.si_value.sival_int == 9);
	CHECK(timer_delete(timer) == 0);
	// Without a sigevent: SIGALRM, carrying the timer's number.
	CHECK(timer_create(CLOCK_MONOTONIC, NULL, &timer) == 0);
	CHECK(timer_ms(timer, 0, 5, 0) == 0);
	CHECK(sigwaitinfo(&set, &info) == SIGALRM && info.si_code == SI_TIMER);
	CHECK(info.si_value.sival_int == (int)(intptr_t)timer);
	CHECK(timer_delete(timer) == 0);
	// SIGEV_NONE: only read.
	struct sigevent quiet = {0};
	quiet.sigev_notify = SIGEV_NONE;
	CHECK(timer_create(CLOCK_MONOTONIC, &quiet, &timer) == 0);
	CHECK(timer_ms(timer, 0, 30, 10) == 0);
	check_sleep_ms(45);
	CHECK(timer_gettime(timer, &left) == 0 && left.it_value.tv_nsec > 0);
	CHECK(left.it_value.tv_nsec <= 10000000);
	CHECK(sigtimedwait(&set, &info, &none) < 0);
	CHECK(timer_delete(timer) == 0);
	// SIGEV_THREAD: a thread calls the function for each expiry.
	struct sigevent call = {0};
	call.sigev_notify = SIGEV_THREAD;
	call.sigev_notify_function = on_timer;
	call.sigev_value.sival_int = 77;
	CHECK(timer_create(CLOCK_MONOTONIC, &call, &timer) == 0);
	CHECK(timer_ms(timer, 0, 5, 5) == 0);
	start = check_now_ns();
	while(__atomic_load_n(&thread_calls, __ATOMIC_RELAXED) < 3 &&
			check_now_ns() - start < 2000000000LL)
		check_sleep_ms(5);
	CHECK(thread_calls >= 3 && thread_value == 77);
	CHECK(timer_delete(timer) == 0);
	// An expiry already handed to the helper thread may still land:
	// give it time, then no more come.
	check_sleep_ms(20);
	int calls = __atomic_load_n(&thread_calls, __ATOMIC_RELAXED);
	check_sleep_ms(30);
	CHECK(__atomic_load_n(&thread_calls, __ATOMIC_RELAXED) == calls);
	// A forked child has none of its parent's timers.
	timer = timer_for(CLOCK_MONOTONIC, rt, 1);
	pid_t child = check_fork();
	if(child == 0) {
		CHECK_ERROR(timer_gettime(timer, &left), EINVAL);
		check_exit_child();
	}
	CHECK_CHILD(child);
	CHECK(timer_delete(timer) == 0);
	CHECK_ERROR(timer_delete(timer), EINVAL);
	// Linux's clocks that drive no timers.
	CHECK_ERROR(timer_create(CLOCK_MONOTONIC_RAW, NULL, &timer), EOPNOTSUPP);
	CHECK_ERROR(timer_create(CLOCK_REALTIME_COARSE, NULL, &timer), EOPNOTSUPP);
	// A time far off stays far off, not wrapped to a near one.
	struct sigevent quiet_one = {.sigev_notify = SIGEV_NONE};
	CHECK(timer_create(CLOCK_MONOTONIC, &quiet_one, &timer) == 0);
	struct itimerspec far = {{0, 0}, {(time_t)1 << 40, 0}};
	CHECK(timer_settime(timer, 0, &far, NULL) == 0);
	CHECK(timer_gettime(timer, &left) == 0 && left.it_value.tv_sec > (time_t)1 << 32);
	CHECK(timer_delete(timer) == 0);
	sigprocmask(SIG_UNBLOCK, &set, NULL);
}

int main(void) {
	check_start("time");
	RUN(clocks);
	RUN(sleeps);
	RUN(interrupted_sleeps);
	RUN(cpu_time);
	RUN(timers);
	RUN(posix_timers);
	return check_end();
}
