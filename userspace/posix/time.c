// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Clocks and timers: the clocks clock_gettime reads, sleeping, CPU time
// as times and getrusage count it, and the interval timers.

#include <sys/resource.h>
#include <sys/time.h>
#include <sys/times.h>

#include "check.h"

static long long ns(const struct timespec *ts) {
	return (long long)ts->tv_sec * 1000000000LL + ts->tv_nsec;
}

// Spins for `ms` milliseconds of the monotonic clock.
static void spin(long ms) {
	long long end = check_now_ns() + ms * 1000000LL;
	volatile unsigned long n = 0;
	while(check_now_ns() < end)
		n++;
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
	CHECK(usage.ru_utime.tv_sec * 1000000 + usage.ru_utime.tv_usec >= 50000);
	// A child's time counts for its parent once the parent has waited.
	struct rusage children;
	CHECK(getrusage(RUSAGE_CHILDREN, &children) == 0);
	long before_children = children.ru_utime.tv_sec * 1000000 + children.ru_utime.tv_usec;
	pid_t pid = check_fork();
	if(pid == 0) {
		spin(100);
		_exit(0);
	}
	int status;
	struct rusage child;
	CHECK(wait4(pid, &status, 0, &child) == pid);
	CHECK(child.ru_utime.tv_sec * 1000000 + child.ru_utime.tv_usec >= 50000);
	CHECK(getrusage(RUSAGE_CHILDREN, &children) == 0);
	CHECK(children.ru_utime.tv_sec * 1000000 + children.ru_utime.tv_usec - before_children >= 50000);
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
	signal(SIGALRM, SIG_DFL);
	signal(SIGVTALRM, SIG_DFL);
	signal(SIGPROF, SIG_DFL);
}

int main(void) {
	check_start("time");
	RUN(clocks);
	RUN(sleeps);
	RUN(cpu_time);
	RUN(timers);
	return check_end();
}
