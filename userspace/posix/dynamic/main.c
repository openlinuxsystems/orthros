// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Dynamic linking, as a bundle uses it (ADR 0071): a program that the
// dynamic linker (/system/lib/ld.so) starts on the shared C library and a
// private library it finds next to it (lib/libgreet.so, through its run
// path, $ORIGIN/lib), with that library's constructor, data and
// thread-local storage; a library opened with dlopen, and its symbols
// found with dlsym; fork and threads in such a program.

#include <dlfcn.h>
#include <link.h>
#include <pthread.h>
#include <stdint.h>
#include <sys/auxv.h>

#include "check.h"
#include "greet.h"

// Where the test's libraries are.
#define LIBRARIES "/system/tests/posix/lib/"

// What the loaded objects' names end with, as dl_iterate_phdr finds them.
struct seen {
	int libc;
	int greet;
	int count;
};

static int ends_with(const char *name, const char *end) {
	size_t length = strlen(name), size = strlen(end);
	return length >= size && !strcmp(name + length - size, end);
}

static int note_object(struct dl_phdr_info *info, size_t size, void *argument) {
	(void)size;
	struct seen *seen = argument;
	const char *name = info->dlpi_name ? info->dlpi_name : "";
	seen->libc |= ends_with(name, "/libc.so");
	seen->greet |= ends_with(name, "/libgreet.so");
	seen->count++;
	return 0;
}

static void started(void) {
	// The kernel started the dynamic linker, and told it where.
	CHECK(getauxval(AT_BASE) != 0);
	struct seen seen = {0, 0, 0};
	dl_iterate_phdr(note_object, &seen);
	CHECK(seen.libc && seen.greet);
	CHECK(seen.count >= 3);
	// The library's constructor ran before main.
	CHECK(greet_constructed == 1);
}

static void calls(void) {
	char buffer[64];
	CHECK(greet("Duke", buffer, sizeof(buffer)) == 11);
	CHECK(!strcmp(buffer, "hello, Duke"));
	CHECK(greet_shared == 42);
	greet_shared++;
	CHECK(greet_shared == 43);
	greet_shared--;
	CHECK(greet_calls() == 1);
}

static void *other_thread(void *argument) {
	(void)argument;
	char buffer[64];
	// A thread's count starts at nothing, whatever the first thread's is.
	int ok = greet_calls() == 0;
	greet("thread", buffer, sizeof(buffer));
	greet("thread", buffer, sizeof(buffer));
	ok = ok && greet_calls() == 2 && !strcmp(buffer, "hello, thread");
	return (void *)(intptr_t)ok;
}

static void threads(void) {
	int before = greet_calls();
	pthread_t thread;
	CHECK(pthread_create(&thread, NULL, other_thread, NULL) == 0);
	void *result = NULL;
	CHECK(pthread_join(thread, &result) == 0);
	CHECK(result == (void *)1);
	CHECK(greet_calls() == before);
}

static void forks(void) {
	pid_t pid = check_fork();
	if(pid == 0) {
		char buffer[64];
		CHECK(greet("child", buffer, sizeof(buffer)) == 12);
		CHECK(!strcmp(buffer, "hello, child"));
		check_exit_child();
	}
	CHECK_CHILD(pid);
}

static void *plugin_thread(void *argument) {
	int (*use)(void) = (int (*)(void))argument;
	// A new thread's copy of the plugin's thread-local data.
	return (void *)(intptr_t)(use() == 101 && use() == 102);
}

static void opens(void) {
	void *plugin = dlopen(LIBRARIES "plugin.so", RTLD_NOW | RTLD_LOCAL);
	if(!plugin)
		printf("dynamic: dlopen: %s\n", dlerror());
	CHECK(plugin != NULL);
	if(!plugin)
		return;
	int (*answer)(void) = (int (*)(void))dlsym(plugin, "plugin_answer");
	int (*use)(void) = (int (*)(void))dlsym(plugin, "plugin_use");
	int (*plugin_greet)(const char *, char *, size_t) =
			(int (*)(const char *, char *, size_t))dlsym(plugin, "plugin_greet");
	CHECK(answer && use && plugin_greet);
	if(!answer || !use || !plugin_greet)
		return;
	// It and the library it needs are constructed, and it shares the
	// program's copy of that library.
	CHECK(answer() == 43);
	char buffer[64];
	int before = greet_calls();
	CHECK(plugin_greet("Duke", buffer, sizeof(buffer)) > 0);
	CHECK(!strcmp(buffer, "hello, Duke, from the plugin"));
	CHECK(greet_calls() == before + 1);
	CHECK(use() == 101);
	CHECK(use() == 102);
	pthread_t thread;
	CHECK(pthread_create(&thread, NULL, plugin_thread, (void *)use) == 0);
	void *result = NULL;
	CHECK(pthread_join(thread, &result) == 0);
	CHECK(result == (void *)1);
	CHECK(use() == 103);
	// Symbols of the program's libraries, found from anywhere.
	CHECK(dlsym(RTLD_DEFAULT, "greet") == (void *)greet);
	CHECK(dlsym(RTLD_DEFAULT, "plugin_answer") == NULL);
	CHECK(dlsym(plugin, "no_such_symbol") == NULL);
	CHECK(dlerror() != NULL);
	CHECK(dlclose(plugin) == 0);
	// A library that is not there.
	CHECK(dlopen(LIBRARIES "missing.so", RTLD_NOW) == NULL);
	CHECK(dlerror() != NULL);
}

int main(void) {
	check_start("dynamic");
	RUN(started);
	RUN(calls);
	RUN(threads);
	RUN(forks);
	RUN(opens);
	return check_end();
}
