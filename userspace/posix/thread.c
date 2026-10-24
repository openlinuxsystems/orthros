// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// POSIX threads: creating and joining them, mutexes and condition
// variables, thread-local storage, detached threads, signals to a
// thread, and thread names.

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>

#include "check.h"

enum { THREADS = 8, ROUNDS = 2000 };

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static long counter;
static atomic_long atomic_counter;
static _Thread_local int mine = 7;

static void *count(void *argument) {
	for(int i = 0; i < ROUNDS; i++) {
		pthread_mutex_lock(&lock);
		counter++;
		pthread_mutex_unlock(&lock);
		atomic_fetch_add(&atomic_counter, 1);
	}
	// Each thread has its own copy, which starts as the program's.
	int ok = mine == 7;
	mine = (int)(intptr_t)argument;
	sched_yield();
	ok = ok && mine == (int)(intptr_t)argument;
	return (void *)(intptr_t)(ok ? 100 + (intptr_t)argument : -1);
}

static void counts(void) {
	pthread_t threads[THREADS];
	for(intptr_t i = 0; i < THREADS; i++)
		CHECK(pthread_create(&threads[i], NULL, count, (void *)i) == 0);
	for(intptr_t i = 0; i < THREADS; i++) {
		void *result = NULL;
		CHECK(pthread_join(threads[i], &result) == 0);
		CHECK((intptr_t)result == 100 + i);
	}
	CHECK(counter == THREADS * ROUNDS);
	CHECK(atomic_load(&atomic_counter) == THREADS * ROUNDS);
	CHECK(mine == 7);
}

static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static int turn;

static void *ping(void *argument) {
	int me = (int)(intptr_t)argument;
	for(int i = 0; i < 1000; i++) {
		pthread_mutex_lock(&lock);
		while(turn % 2 != me)
			pthread_cond_wait(&changed, &lock);
		turn++;
		pthread_cond_broadcast(&changed);
		pthread_mutex_unlock(&lock);
	}
	return NULL;
}

static void conditions(void) {
	pthread_t threads[2];
	turn = 0;
	for(intptr_t i = 0; i < 2; i++)
		CHECK(pthread_create(&threads[i], NULL, ping, (void *)i) == 0);
	for(int i = 0; i < 2; i++)
		CHECK(pthread_join(threads[i], NULL) == 0);
	CHECK(turn == 2000);
	// A timed wait that times out.
	struct timespec until;
	clock_gettime(CLOCK_REALTIME, &until);
	until.tv_nsec += 20000000;
	if(until.tv_nsec >= 1000000000) {
		until.tv_sec++;
		until.tv_nsec -= 1000000000;
	}
	pthread_mutex_lock(&lock);
	CHECK(pthread_cond_timedwait(&changed, &lock, &until) == ETIMEDOUT);
	pthread_mutex_unlock(&lock);
	CHECK(pthread_mutex_trylock(&lock) == 0);
	CHECK(pthread_mutex_trylock(&lock) == EBUSY);
	pthread_mutex_unlock(&lock);
}

static atomic_int detached_ran;

static void *detached(void *argument) {
	(void)argument;
	atomic_store(&detached_ran, 1);
	pthread_exit(NULL);
}

static void *self(void *argument) {
	*(pthread_t *)argument = pthread_self();
	return NULL;
}

static void lives(void) {
	pthread_attr_t attr;
	pthread_attr_init(&attr);
	pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
	pthread_t thread;
	CHECK(pthread_create(&thread, &attr, detached, NULL) == 0);
	pthread_attr_destroy(&attr);
	for(int i = 0; i < 1000 && !atomic_load(&detached_ran); i++)
		check_sleep_ms(1);
	CHECK(atomic_load(&detached_ran));
	pthread_t seen;
	CHECK(pthread_create(&thread, NULL, self, &seen) == 0);
	CHECK(pthread_join(thread, NULL) == 0);
	CHECK(pthread_equal(seen, thread) && !pthread_equal(seen, pthread_self()));
	// A thread with a stack size of its own.
	pthread_attr_init(&attr);
	CHECK(pthread_attr_setstacksize(&attr, 1 << 20) == 0);
	CHECK(pthread_create(&thread, &attr, self, &seen) == 0);
	CHECK(pthread_join(thread, NULL) == 0);
	pthread_attr_destroy(&attr);
	// The first thread has the process's number, as on Linux.
	CHECK(gettid() == getpid());
}

static volatile pthread_t caught_on;
static atomic_int caught;

static void on_signal(int signal) {
	(void)signal;
	caught_on = pthread_self();
	atomic_store(&caught, 1);
}

static atomic_int stop;

static void *waits(void *argument) {
	(void)argument;
	while(!atomic_load(&stop))
		check_sleep_ms(1);
	return NULL;
}

// A signal sent to a thread runs its handler on that thread.
static void signals(void) {
	signal(SIGUSR1, on_signal);
	pthread_t thread;
	CHECK(pthread_create(&thread, NULL, waits, NULL) == 0);
	CHECK(pthread_kill(thread, SIGUSR1) == 0);
	for(int i = 0; i < 1000 && !atomic_load(&caught); i++)
		check_sleep_ms(1);
	CHECK(atomic_load(&caught) && pthread_equal(caught_on, thread));
	atomic_store(&stop, 1);
	CHECK(pthread_join(thread, NULL) == 0);
	// For the next one that waits (names' waiter, which must not end
	// while it is named).
	atomic_store(&stop, 0);
	signal(SIGUSR1, SIG_DFL);
}

// fork in a program with threads: the child has only the thread that
// called fork.
static void *forks(void *argument) {
	(void)argument;
	pid_t pid = fork();
	if(pid == 0)
		_exit(counter == THREADS * ROUNDS ? 0 : 1);
	int status;
	waitpid(pid, &status, 0);
	return (void *)(intptr_t)(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

static void forking(void) {
	pthread_t thread;
	fflush(stdout);
	CHECK(pthread_create(&thread, NULL, forks, NULL) == 0);
	void *result = NULL;
	CHECK(pthread_join(thread, &result) == 0 && result == (void *)1);
}

static void *named(void *argument) {
	char name[16];
	// A new thread has its process's name, until it names itself.
	int ok = pthread_getname_np(pthread_self(), name, sizeof(name)) == 0 && !strcmp(name, "thread");
	ok = ok && pthread_setname_np(pthread_self(), (const char *)argument) == 0;
	return (void *)(intptr_t)ok;
}

// pthread_setname_np and pthread_getname_np, with Linux's limit of 15
// bytes.
static void names(void) {
	char name[16];
	CHECK(pthread_getname_np(pthread_self(), name, sizeof(name)) == 0);
	CHECK(!strcmp(name, "thread"));
	CHECK(pthread_setname_np(pthread_self(), "main thread") == 0);
	CHECK(pthread_getname_np(pthread_self(), name, sizeof(name)) == 0);
	CHECK(!strcmp(name, "main thread"));
	CHECK(pthread_getname_np(pthread_self(), name, 11) == ERANGE);
	CHECK(pthread_getname_np(pthread_self(), name, 12) == 0 && !strcmp(name, "main thread"));
	CHECK(pthread_setname_np(pthread_self(), "fifteen bytes!!") == 0);
	CHECK(pthread_setname_np(pthread_self(), "sixteen bytes!!!") == ERANGE);
	CHECK(pthread_setname_np(pthread_self(), "\xff") == EINVAL);
	CHECK(pthread_setname_np(pthread_self(), "naïve") == 0);
	CHECK(pthread_getname_np(pthread_self(), name, sizeof(name)) == 0 && !strcmp(name, "naïve"));
	// A thread names itself, and this one names another.
	pthread_t thread;
	CHECK(pthread_create(&thread, NULL, named, "worker") == 0);
	void *result = NULL;
	CHECK(pthread_join(thread, &result) == 0 && result == (void *)1);
	CHECK(pthread_create(&thread, NULL, waits, NULL) == 0);
	CHECK(pthread_setname_np(thread, "waiter") == 0);
	CHECK(pthread_getname_np(thread, name, sizeof(name)) == 0 && !strcmp(name, "waiter"));
	atomic_store(&stop, 1);
	CHECK(pthread_join(thread, NULL) == 0);
	atomic_store(&stop, 0);
}

int main(void) {
	check_start("thread");
	RUN(counts);
	RUN(conditions);
	RUN(lives);
	RUN(signals);
	RUN(forking);
	RUN(names);
	return check_end();
}
