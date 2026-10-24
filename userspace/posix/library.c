// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The C library's corners that the post-Phase-9 audit fixed, each as
// glibc 2.39 has it on Linux: allocations near the top of the address
// space and on large alignments, a null environ, popen, message
// catalogs, the functions that were "Not implemented", and threads'
// stacks and signal masks.

#include <fcntl.h>
#include <limits.h>
#include <malloc.h>
#include <netinet/ether.h>
#include <nl_types.h>
#include <pthread.h>
#include <stdint.h>
#include <sys/stat.h>
#include <sys/utsname.h>

#include "check.h"

extern char **environ;

// `size`, which the compiler cannot see through, so that it neither
// warns of nor folds an allocation it knows must fail.
static size_t unknown(size_t size) {
	volatile size_t hidden = size;
	return hidden;
}

// The allocation functions, called through pointers the compiler cannot
// see through either: it takes an allocation nobody reads as one it may
// leave out, and that it then gave a block (clang -O2).
static void *(*volatile malloc_)(size_t) = malloc;
static void *(*volatile calloc_)(size_t, size_t) = calloc;
static void *(*volatile realloc_)(void *, size_t) = realloc;
static void *(*volatile aligned_alloc_)(size_t, size_t) = aligned_alloc;
static void *(*volatile memalign_)(size_t, size_t) = memalign;

static int aligned(const void *p, size_t alignment) {
	return ((uintptr_t)p & (alignment - 1)) == 0;
}

static void allocations(void) {
	// What cannot be had fails with ENOMEM, and never wraps to a small
	// block handed out as a large one.
	static const size_t huge[] = {SIZE_MAX, SIZE_MAX - 4096, SIZE_MAX - 300000,
			(size_t)PTRDIFF_MAX + 1};
	for(size_t i = 0; i < sizeof(huge) / sizeof(huge[0]); i++) {
		size_t size = unknown(huge[i]);
		errno = 0;
		CHECK(malloc_(size) == NULL && errno == ENOMEM);
		// A failed realloc leaves the block as it was.
		char *p = malloc_(16);
		CHECK(p != NULL);
		strcpy(p, "kept");
		errno = 0;
		char *bigger = realloc_(p, size);
		CHECK(bigger == NULL && errno == ENOMEM);
		if(bigger)
			p = bigger;
		CHECK(strcmp(p, "kept") == 0);
		free(p);
		void *q = &q;
		CHECK(posix_memalign(&q, 4096, size) == ENOMEM);
	}
	errno = 0;
	CHECK(calloc_(unknown(SIZE_MAX / 2), 4) == NULL && errno == ENOMEM);
	// Every power-of-two alignment, small blocks and large.
	static const size_t sizes[] = {1, 100, 4096, 65536, 300000};
	for(size_t alignment = sizeof(void *); alignment <= (1 << 20); alignment <<= 1) {
		for(size_t i = 0; i < sizeof(sizes) / sizeof(sizes[0]); i++) {
			void *p = NULL;
			CHECK(posix_memalign(&p, alignment, sizes[i]) == 0 && p && aligned(p, alignment));
			if(p) {
				memset(p, 0x5a, sizes[i]);
				CHECK(malloc_usable_size(p) >= sizes[i]);
				// realloc keeps what was there.
				unsigned char *bigger = realloc_(p, sizes[i] * 2);
				CHECK(bigger && bigger[sizes[i] - 1] == 0x5a);
				free(bigger);
			}
			void *a = aligned_alloc_(alignment, sizes[i]);
			CHECK(a && aligned(a, alignment));
			free(a);
			void *m = memalign_(alignment, sizes[i]);
			CHECK(m && aligned(m, alignment));
			free(m);
		}
	}
	void *p = &p;
	CHECK(posix_memalign(&p, 3, 8) == EINVAL && p == &p);
	CHECK(posix_memalign(&p, sizeof(void *) / 2, 8) == EINVAL);
	errno = 0;
	CHECK(aligned_alloc_(unknown(3), 8) == NULL && errno == EINVAL);
	// memalign rounds an alignment up to a power of two.
	void *m = memalign_(48, 100);
	CHECK(m && aligned(m, 64));
	free(m);
	// C17: any size, not only a multiple of the alignment.
	void *odd = aligned_alloc_(64, 100);
	CHECK(odd && aligned(odd, 64));
	free(odd);
}

static void environment(void) {
	CHECK(setenv("LIBRARY_TEST", "1", 1) == 0);
	// A copy of the environment, to put back.
	size_t count = 0;
	while(environ[count])
		count++;
	char **saved = calloc(count + 1, sizeof(char *));
	for(size_t i = 0; saved && i < count; i++)
		saved[i] = strdup(environ[i]);
	// clearenv(3) names this as the other way to clear the environment.
	environ = NULL;
	CHECK(getenv("LIBRARY_TEST") == NULL);
	CHECK(getenv("PATH") == NULL);
	CHECK(unsetenv("LIBRARY_TEST") == 0);
	CHECK(setenv("LIBRARY_ALONE", "2", 1) == 0);
	CHECK(getenv("LIBRARY_ALONE") && strcmp(getenv("LIBRARY_ALONE"), "2") == 0);
	CHECK(environ && environ[0] && strcmp(environ[0], "LIBRARY_ALONE=2") == 0 && !environ[1]);
	CHECK(clearenv() == 0);
	for(size_t i = 0; saved && i < count; i++)
		putenv(saved[i]);
	free(saved);
	CHECK(unsetenv("LIBRARY_TEST") == 0);
	size_t now = 0;
	while(environ[now])
		now++;
	CHECK(now == count - 1);
	// realpath with no buffer gives one to free; without a path it is
	// EINVAL.
	char *real = realpath("/tmp/../tmp/.", NULL);
	CHECK(real && strcmp(real, "/tmp") == 0);
	free(real);
	char buffer[PATH_MAX];
	CHECK_ERROR(realpath(NULL, buffer) ? 0 : -1, EINVAL);
}

// Runs `command` in sh, as popen does; says what it printed, up to
// `room` bytes.
static int output_of(const char *command, char *text, size_t room) {
	FILE *f = popen(command, "r");
	if(!f)
		return -1;
	size_t n = fread(text, 1, room - 1, f);
	text[n] = '\0';
	return pclose(f);
}

static void popens(void) {
	// glibc's modes: 'r' or 'w', and 'e' for close-on-exec, in any order.
	static const char *const good[] = {"r", "w", "re", "we", "er", "rr", "ree"};
	static const int cloexec[] = {0, 0, 1, 1, 1, 0, 1};
	for(size_t i = 0; i < sizeof(good) / sizeof(good[0]); i++) {
		FILE *f = popen("true", good[i]);
		CHECK(f != NULL);
		if(!f)
			continue;
		int flags = fcntl(fileno(f), F_GETFD);
		CHECK(flags >= 0 && !!(flags & FD_CLOEXEC) == cloexec[i]);
		CHECK(pclose(f) == 0);
	}
	static const char *const bad[] = {"rw", "wr", "rx", "", "r+", "w+", "rb", "e", "W"};
	for(size_t i = 0; i < sizeof(bad) / sizeof(bad[0]); i++)
		CHECK_ERROR(popen("true", bad[i]) ? 0 : -1, EINVAL);

	char text[256];
	CHECK(output_of("echo hello; exit 3", text, sizeof(text)) == 3 << 8);
	CHECK(strcmp(text, "hello\n") == 0);

	// POSIX: a later popen's child has none of the streams earlier ones
	// opened, so a command reading from one sees its end when it is
	// closed, whatever else runs.
	FILE *writer = popen("cat > /dev/null", "w");
	CHECK(writer != NULL);
	if(writer) {
		char command[128];
		snprintf(command, sizeof(command),
				"if { true >&%d; } 2>/dev/null; then echo open; else echo closed; fi",
				fileno(writer));
		CHECK(output_of(command, text, sizeof(text)) == 0);
		CHECK(strcmp(text, "closed\n") == 0);
		CHECK(pclose(writer) == 0);
	}

	// With its standard input closed, the pipe's end the command reads
	// from is born as descriptor 0, where it stays.
	pid_t child = check_fork();
	if(child == 0) {
		close(STDIN_FILENO);
		FILE *in = popen("cat > /tmp/library-popen", "w");
		CHECK(in != NULL);
		if(in) {
			fputs("through stdin\n", in);
			CHECK(pclose(in) == 0);
		}
		FILE *back = fopen("/tmp/library-popen", "r");
		CHECK(back && fgets(text, sizeof(text), back) && strcmp(text, "through stdin\n") == 0);
		if(back)
			fclose(back);
		unlink("/tmp/library-popen");
		check_exit_child();
	}
	CHECK_CHILD(child);
}

// A catalog as gencat writes one: a header, a set, its messages and
// their texts.
static void write_catalog(const char *path, const unsigned char *bytes, size_t size) {
	int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
	CHECK(fd >= 0 && write(fd, bytes, size) == (ssize_t)size);
	close(fd);
}

static void be32(unsigned char *at, uint32_t value) {
	at[0] = value >> 24;
	at[1] = value >> 16;
	at[2] = value >> 8;
	at[3] = value;
}

// In musl's and the BSDs' format, which glibc does not read.
static void catalogs(void) {
	// One set, 1, with one message, 1: "hello".
	unsigned char good[50] = {0};
	be32(good, 0xff88ff89);
	be32(good + 4, 1);
	be32(good + 8, 30);
	be32(good + 12, 12);
	be32(good + 16, 24);
	be32(good + 20, 1);
	be32(good + 24, 1);
	be32(good + 28, 0);
	be32(good + 32, 1);
	be32(good + 36, 5);
	be32(good + 40, 0);
	memcpy(good + 44, "hello", 6);
	const char *path = "/tmp/library.cat";
	write_catalog(path, good, sizeof(good));
	nl_catd catd = catopen(path, 0);
	CHECK(catd != (nl_catd)-1);
	if(catd != (nl_catd)-1) {
		CHECK(strcmp(catgets(catd, 1, 1, "none"), "hello") == 0);
		CHECK(strcmp(catgets(catd, 1, 2, "none"), "none") == 0);
		CHECK(strcmp(catgets(catd, 2, 1, "none"), "none") == 0);
		CHECK(catclose(catd) == 0);
	}
	// A corrupt catalog has no messages, and is never read past its end:
	// a set whose messages start past it, a message whose text does, and
	// a last text with no end.
	unsigned char bad[50];
	static const struct {
		size_t at;
		uint32_t value;
	} corruptions[] = {{28, 1000000}, {24, 1000000}, {40, 1000000}, {40, 3}};
	for(size_t i = 0; i < sizeof(corruptions) / sizeof(corruptions[0]); i++) {
		memcpy(bad, good, sizeof(bad));
		be32(bad + corruptions[i].at, corruptions[i].value);
		if(corruptions[i].value == 3)
			bad[49] = '!';
		write_catalog(path, bad, sizeof(bad));
		catd = catopen(path, 0);
		CHECK(catd != (nl_catd)-1);
		if(catd != (nl_catd)-1) {
			CHECK(strcmp(catgets(catd, 1, 1, "none"), "none") == 0);
			catclose(catd);
		}
	}
	// Tables that do not fit the file, or a size that is not its own,
	// make no catalog.
	static const struct {
		size_t at;
		uint32_t value;
	} headers[] = {{4, 0x10000000}, {8, 31}, {12, 1000}, {16, 1000}};
	for(size_t i = 0; i < sizeof(headers) / sizeof(headers[0]); i++) {
		memcpy(bad, good, sizeof(bad));
		be32(bad + headers[i].at, headers[i].value);
		write_catalog(path, bad, sizeof(bad));
		CHECK(catopen(path, 0) == (nl_catd)-1);
	}
	write_catalog(path, good, 10);
	CHECK(catopen(path, 0) == (nl_catd)-1);
	unlink(path);
}

static void finished(void) {
	// Names: Linux's domain of a machine that has none, and ether_ntoa's
	// lower case without leading zeros.
	struct utsname name;
	CHECK(uname(&name) == 0 && strcmp(name.domainname, "(none)") == 0);
	char domain[64];
	CHECK(getdomainname(domain, sizeof(domain)) == 0 && strcmp(domain, "(none)") == 0);
	CHECK_ERROR(setdomainname("example", 7), EPERM);
	struct ether_addr address = {{0x00, 0x1a, 0x2b, 0x0c, 0xff, 0x01}};
	CHECK(strcmp(ether_ntoa(&address), "0:1a:2b:c:ff:1") == 0);
	struct ether_addr *read_back = ether_aton("0:1A:2b:c:ff:01");
	CHECK(read_back && memcmp(read_back, &address, sizeof(address)) == 0);
	CHECK(ether_aton("0:1a:2b:c:ff") == NULL);

	// tmpnam gives a name nothing has; tempnam takes the directory and
	// prefix asked for.
	char buffer[L_tmpnam];
	char *fresh = tmpnam(buffer);
	struct stat st;
	CHECK(fresh == buffer && strncmp(fresh, P_tmpdir "/", strlen(P_tmpdir) + 1) == 0);
	CHECK_ERROR(stat(fresh, &st), ENOENT);
	char *saved = getenv("TMPDIR") ? strdup(getenv("TMPDIR")) : NULL;
	unsetenv("TMPDIR");
	char *named = tempnam("/tmp", "lib");
	CHECK(named && strncmp(named, "/tmp/lib", 8) == 0);
	free(named);
	named = tempnam("/no/such/directory", "toolongprefix");
	CHECK(named && strncmp(named, P_tmpdir "/toolo", strlen(P_tmpdir) + 6) == 0);
	free(named);
	if(saved) {
		setenv("TMPDIR", saved, 1);
		free(saved);
	}
}

static void bsd_lines(void) {
	// fgetln: each line with its newline, then what is left.
	FILE *f = fmemopen("one\ntwo", 7, "r");
	CHECK(f != NULL);
	if(f) {
		size_t length;
		char *line = fgetln(f, &length);
		CHECK(line && length == 4 && memcmp(line, "one\n", 4) == 0);
		line = fgetln(f, &length);
		CHECK(line && length == 3 && memcmp(line, "two", 3) == 0);
		CHECK(fgetln(f, &length) == NULL);
		fclose(f);
	}
}

static void *stack_user(void *argument) {
	// Formatting a double uses aligned vector stores on x86-64, which a
	// stack misaligned by its creator breaks.
	char text[32];
	snprintf(text, sizeof(text), "%.2f", *(double *)argument);
	return strcmp(text, "1.50") == 0 ? argument : NULL;
}

static void stacks(void) {
	// A stack of the caller's whose end is not 16-byte aligned.
	enum { SIZE = 1 << 16 };
	static _Alignas(16) unsigned char memory[SIZE + 24];
	pthread_attr_t attr;
	CHECK(pthread_attr_init(&attr) == 0);
	CHECK(pthread_attr_setstack(&attr, memory + 8, SIZE) == 0);
	double value = 1.5;
	pthread_t thread;
	void *result = NULL;
	CHECK(pthread_create(&thread, &attr, stack_user, &value) == 0);
	CHECK(pthread_join(thread, &result) == 0 && result == &value);
	pthread_attr_destroy(&attr);
	// Sizes that are not whole pages.
	CHECK(pthread_attr_init(&attr) == 0);
	CHECK(pthread_attr_setstacksize(&attr, PTHREAD_STACK_MIN + 100) == 0);
	CHECK(pthread_attr_setguardsize(&attr, 100) == 0);
	CHECK(pthread_create(&thread, &attr, stack_user, &value) == 0);
	CHECK(pthread_join(thread, &result) == 0 && result == &value);
	pthread_attr_destroy(&attr);
}

static sigset_t thread_mask;

static void *report_mask(void *argument) {
	(void)argument;
	pthread_sigmask(SIG_BLOCK, NULL, &thread_mask);
	return NULL;
}

static int ends[2];

static void *read_forever(void *argument) {
	(void)argument;
	char byte;
	// read is a cancellation point.
	ssize_t n = read(ends[0], &byte, 1);
	(void)n;
	return NULL;
}

static void *read_all_blocked(void *argument) {
	// Every bit set by hand, as some programs make "every signal": the C
	// library's own signals stay as they were.
	sigset_t all;
	memset(&all, 0xff, sizeof(all));
	pthread_sigmask(SIG_SETMASK, &all, NULL);
	return read_forever(argument);
}

static void masks(void) {
	// A new thread has its creator's mask.
	pthread_t thread;
	sigset_t blocked;
	sigemptyset(&blocked);
	sigaddset(&blocked, SIGUSR2);
	pthread_sigmask(SIG_BLOCK, &blocked, NULL);
	CHECK(pthread_create(&thread, NULL, report_mask, NULL) == 0);
	CHECK(pthread_join(thread, NULL) == 0);
	CHECK(sigismember(&thread_mask, SIGUSR2) == 1);
	pthread_sigmask(SIG_UNBLOCK, &blocked, NULL);

	// A thread made by one whose cancellation is disabled can be
	// cancelled in a wait.
	CHECK(pipe(ends) == 0);
	int old;
	CHECK(pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, &old) == 0);
	CHECK(pthread_create(&thread, NULL, read_forever, NULL) == 0);
	CHECK(pthread_setcancelstate(old, NULL) == 0);
	usleep(20000);
	CHECK(pthread_cancel(thread) == 0);
	void *result = NULL;
	CHECK(pthread_join(thread, &result) == 0 && result == PTHREAD_CANCELED);
	// So can one that blocked every signal it could.
	CHECK(pthread_create(&thread, NULL, read_all_blocked, NULL) == 0);
	usleep(20000);
	CHECK(pthread_cancel(thread) == 0);
	CHECK(pthread_join(thread, &result) == 0 && result == PTHREAD_CANCELED);
	close(ends[0]);
	close(ends[1]);
}

int main(void) {
	check_start("library");
	RUN(allocations);
	RUN(environment);
	RUN(popens);
	RUN(catalogs);
	RUN(finished);
	RUN(bsd_lines);
	RUN(stacks);
	RUN(masks);
	return check_end();
}
