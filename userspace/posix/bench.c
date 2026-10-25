// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The POSIX benchmarks of `ob bench`: one C program, built for Radix on
// mlibc (Spark runs it with --posix) and for Linux with the host's C
// compiler and C library (linux-bench runs it), so both sides make the
// same calls through their C libraries. Each benchmark prints a line in
// the format of the others,
//
//     BENCH name=<name> ns=<nanoseconds per operation> iterations=<count>
//
// and the program prints "posix-bench: done" at the end. It pins itself
// to the first CPU it may run on. With --exit it exits at once: the
// program the exec benchmark runs. With --disk <device> it also reads
// and writes that disk, with nothing cached between (the disk
// benchmarks); `ob bench` gives both systems the same virtio disk, a
// file on the host.

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <semaphore.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static uint64_t now_ns(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

// The kernel events `ob bench` counts per operation, which Radix's
// /proc/stat lists one a line ("ipis: 12"); Linux's has none of them.
static const char *const event_names[] = {"switches", "timer_writes", "ipis", "halts", "polls"};
#define EVENTS (sizeof(event_names) / sizeof(event_names[0]))

// The CPU the benchmarks run on (`main` pins them to it).
static int home_cpu;

// What the kernel says at a moment: its events, and how long the
// benchmarks' CPU and the others had been idle (Radix's per-CPU lines,
// "cpu1: user_ns=... idle_ns=..."), and how many others there are.
struct kernel_state {
	uint64_t events[EVENTS];
	uint64_t idle_here, idle_elsewhere;
	unsigned others;
};

// Reads the kernel's state; false if /proc/stat lacks an event.
static int read_events(struct kernel_state *state) {
	memset(state, 0, sizeof(*state));
	FILE *file = fopen("/proc/stat", "r");
	if(!file)
		return 0;
	char line[256];
	unsigned found = 0;
	while(fgets(line, sizeof(line), file)) {
		for(size_t i = 0; i < EVENTS; i++) {
			size_t length = strlen(event_names[i]);
			if(!strncmp(line, event_names[i], length) && line[length] == ':') {
				state->events[i] = strtoull(line + length + 1, NULL, 10);
				found |= 1u << i;
			}
		}
		int cpu;
		const char *idle = strstr(line, " idle_ns=");
		if(sscanf(line, "cpu%d:", &cpu) == 1 && idle) {
			uint64_t ns = strtoull(idle + strlen(" idle_ns="), NULL, 10);
			if(cpu == home_cpu) {
				state->idle_here = ns;
			} else {
				state->idle_elsewhere += ns;
				state->others++;
			}
		}
	}
	fclose(file);
	return found == (1u << EVENTS) - 1;
}

// When a benchmark started, and the kernel's state then.
struct mark {
	uint64_t ns;
	struct kernel_state kernel;
	int counted;
};

// On Radix, Spark copies what the program prints to the kernel log,
// which draws it on the console: a pause before each benchmark lets the
// drawing of what the ones before it printed end before it starts, so
// that its instruction count leaves that drawing out.
static void settle(void) {
	fflush(stdout);
	struct timespec pause = {.tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
	while(nanosleep(&pause, &pause) != 0 && errno == EINTR) {}
}

static struct mark start(void) {
	settle();
	struct mark mark;
	mark.counted = read_events(&mark.kernel);
	mark.ns = now_ns();
	return mark;
}

// Prints the time per operation since `started`, and on Radix the
// kernel's events since then as a BENCHCOUNT line, which ob bench divides
// by the rounds, with how long the benchmarks' CPU sat idle and how long
// the other CPUs worked meanwhile.
static void report(const char *name, struct mark started, uint64_t count) {
	uint64_t elapsed = now_ns() - started.ns;
	double ns = (double)elapsed / (double)count;
	printf("BENCH name=%s ns=%.2f iterations=%llu\n", name, ns, (unsigned long long)count);
	struct kernel_state now;
	if(started.counted && read_events(&now)) {
		printf("BENCHCOUNT group=%s rounds=%llu", name, (unsigned long long)count);
		for(size_t i = 0; i < EVENTS; i++)
			printf(" %s=%llu", event_names[i],
			       (unsigned long long)(now.events[i] - started.kernel.events[i]));
		uint64_t idle_elsewhere = now.idle_elsewhere - started.kernel.idle_elsewhere;
		uint64_t elsewhere = (uint64_t)now.others * elapsed;
		printf(" idle_here_ns=%llu busy_elsewhere_ns=%llu\n",
		       (unsigned long long)(now.idle_here - started.kernel.idle_here),
		       (unsigned long long)(elsewhere > idle_elsewhere ? elsewhere - idle_elsewhere : 0));
	}
	fflush(stdout);
}

static void fail(const char *what) {
	printf("posix-bench: %s failed: %s\n", what, strerror(errno));
	fflush(stdout);
	exit(1);
}

// The CPUs this process may run on, in order; returns how many.
static int allowed_cpus(int *cpus, int most) {
	cpu_set_t set;
	CPU_ZERO(&set);
	if(sched_getaffinity(0, sizeof(set), &set) != 0)
		fail("sched_getaffinity");
	int count = 0;
	for(int cpu = 0; cpu < CPU_SETSIZE && count < most; cpu++)
		if(CPU_ISSET(cpu, &set))
			cpus[count++] = cpu;
	return count;
}

// Pins the calling thread to `cpu`.
static void pin(int cpu) {
	cpu_set_t set;
	CPU_ZERO(&set);
	CPU_SET(cpu, &set);
	if(sched_setaffinity(0, sizeof(set), &set) != 0)
		fail("sched_setaffinity");
}

// A child that exits at once, and the wait for it.
static void fork_wait(uint64_t rounds) {
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++) {
		pid_t child = fork();
		if(child < 0)
			fail("fork");
		if(child == 0)
			_exit(0);
		int status;
		if(waitpid(child, &status, 0) != child || !WIFEXITED(status))
			fail("waitpid");
	}
	report("posix_fork_wait", started, rounds);
}

// A child that runs this program with --exit, and the wait for it.
static void fork_exec_wait(const char *self, uint64_t rounds) {
	char *const argv[] = {(char *)self, "--exit", NULL};
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++) {
		pid_t child = fork();
		if(child < 0)
			fail("fork");
		if(child == 0) {
			execv(self, argv);
			_exit(127);
		}
		int status;
		if(waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))
			fail("exec");
	}
	report("posix_fork_exec_wait", started, rounds);
}

// What a pipe carries per write, and in all.
#define PIPE_CHUNK (64 * 1024)
#define PIPE_TOTAL (256u * 1024 * 1024)

// A child writes PIPE_TOTAL bytes into a pipe, PIPE_CHUNK at a time, and
// this process reads them, both on this CPU: the time per MiB.
static void pipe_throughput(void) {
	static char buffer[PIPE_CHUNK];
	int fds[2];
	if(pipe(fds) != 0)
		fail("pipe");
	struct mark started = start();
	pid_t child = fork();
	if(child < 0)
		fail("fork");
	if(child == 0) {
		close(fds[0]);
		memset(buffer, 'x', sizeof(buffer));
		for(size_t sent = 0; sent < PIPE_TOTAL; sent += PIPE_CHUNK)
			for(size_t done = 0; done < PIPE_CHUNK;) {
				ssize_t n = write(fds[1], buffer + done, PIPE_CHUNK - done);
				if(n <= 0)
					_exit(1);
				done += (size_t)n;
			}
		_exit(0);
	}
	close(fds[1]);
	size_t got = 0;
	for(;;) {
		ssize_t n = read(fds[0], buffer, sizeof(buffer));
		if(n < 0)
			fail("read");
		if(n == 0)
			break;
		got += (size_t)n;
	}
	int status;
	if(waitpid(child, &status, 0) != child || got != PIPE_TOTAL)
		fail("the pipe's writer");
	report("posix_pipe_mib", started, PIPE_TOTAL >> 20);
	close(fds[0]);
}

static volatile sig_atomic_t caught;

static void on_signal(int signal) {
	(void)signal;
	caught++;
}

// A signal this process sends itself, its handler and the return from
// it.
static void signal_delivery(uint64_t rounds) {
	struct sigaction action;
	memset(&action, 0, sizeof(action));
	action.sa_handler = on_signal;
	sigemptyset(&action.sa_mask);
	if(sigaction(SIGUSR1, &action, NULL) != 0)
		fail("sigaction");
	pid_t self = getpid();
	caught = 0;
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++)
		kill(self, SIGUSR1);
	report("posix_signal", started, rounds);
	if((uint64_t)caught != rounds)
		fail("signal delivery");
}

// poll on a pipe with data to read, which returns at once.
static void poll_ready(uint64_t rounds) {
	int fds[2];
	if(pipe(fds) != 0 || write(fds[1], "x", 1) != 1)
		fail("pipe");
	struct pollfd entry = {.fd = fds[0], .events = POLLIN};
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++)
		if(poll(&entry, 1, 0) != 1)
			fail("poll");
	report("posix_poll", started, rounds);
	close(fds[0]);
	close(fds[1]);
}

// Two threads passing a turn there and back through two semaphores, as
// futexes wake and wait in both C libraries.
struct turns {
	sem_t ask, answer;
	uint64_t rounds;
	int cpu;
};

static void *answerer(void *argument) {
	struct turns *turns = argument;
	pin(turns->cpu);
	for(uint64_t i = 0; i < turns->rounds; i++) {
		while(sem_wait(&turns->ask) != 0) {}
		sem_post(&turns->answer);
	}
	return NULL;
}

// Passes the turns to the answerer and back, and reports all but the
// first, which only finds the answerer pinned and waiting.
static void pass_turns(const char *name, struct turns *turns) {
	sem_post(&turns->ask);
	while(sem_wait(&turns->answer) != 0) {}
	struct mark started = start();
	for(uint64_t i = 1; i < turns->rounds; i++) {
		sem_post(&turns->ask);
		while(sem_wait(&turns->answer) != 0) {}
	}
	report(name, started, turns->rounds - 1);
}

static void futex_pingpong(const char *name, int other, uint64_t rounds) {
	struct turns turns = {.rounds = rounds, .cpu = other};
	if(sem_init(&turns.ask, 0, 0) != 0 || sem_init(&turns.answer, 0, 0) != 0)
		fail("sem_init");
	pthread_t thread;
	if(pthread_create(&thread, NULL, answerer, &turns) != 0)
		fail("pthread_create");
	pass_turns(name, &turns);
	pthread_join(thread, NULL);
	sem_destroy(&turns.ask);
	sem_destroy(&turns.answer);
}

// The same between two processes, through semaphores shared between
// processes in memory both map: the kernel names each futex word by
// that memory, not by its address in one process.
static void futex_processes(const char *name, int other, uint64_t rounds) {
	struct turns *turns = mmap(NULL, sizeof(*turns), PROT_READ | PROT_WRITE,
	                           MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	if(turns == MAP_FAILED)
		fail("mmap");
	turns->rounds = rounds;
	turns->cpu = other;
	if(sem_init(&turns->ask, 1, 0) != 0 || sem_init(&turns->answer, 1, 0) != 0)
		fail("sem_init");
	pid_t child = fork();
	if(child < 0)
		fail("fork");
	if(child == 0) {
		answerer(turns);
		_exit(0);
	}
	pass_turns(name, turns);
	int status;
	if(waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))
		fail("waitpid");
	sem_destroy(&turns->ask);
	sem_destroy(&turns->answer);
	munmap(turns, sizeof(*turns));
}

// How much of the disk the disk benchmarks go through, a transfer after
// the other, before they start again at its beginning: more than the
// largest transfer many times over, and less than the host's cache.
#define DISK_SPAN (32u << 20)

// Transfers of `size` bytes, reads or with `writing` writes, one after
// the other through the first DISK_SPAN bytes of the disk in `fd`.
static void disk_transfers(int fd, const char *name, int writing, size_t size, uint64_t rounds) {
	void *buffer;
	// Linux's O_DIRECT moves data straight between the disk and the
	// program's memory, which must be aligned for it; so does Radix for
	// any transfer on a sector's boundary, and copies the rest.
	if(posix_memalign(&buffer, 4096, size) != 0)
		fail("posix_memalign");
	memset(buffer, 0x5a, size);
	off_t at = 0;
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++) {
		ssize_t done = writing ? pwrite(fd, buffer, size, at) : pread(fd, buffer, size, at);
		if(done != (ssize_t)size)
			fail(writing ? "a disk write" : "a disk read");
		at = (at + (off_t)size) % DISK_SPAN;
	}
	report(name, started, rounds);
	free(buffer);
}

// The disk benchmarks on the disk at `path`, opened as Linux's disk
// tools open one to bypass the page cache (Radix's block devices are
// always raw, and its C library takes O_DIRECT and ignores it).
static void disk(const char *path) {
	int fd = open(path, O_RDWR | O_DIRECT);
	if(fd < 0)
		fail(path);
	off_t size = lseek(fd, 0, SEEK_END);
	if(size < (off_t)DISK_SPAN) {
		printf("posix-bench: %s holds %lld bytes, fewer than %u\n", path, (long long)size,
		       DISK_SPAN);
		exit(1);
	}
	disk_transfers(fd, "posix_disk_read_4k", 0, 4096, 20000);
	disk_transfers(fd, "posix_disk_write_4k", 1, 4096, 20000);
	disk_transfers(fd, "posix_disk_read_64k", 0, 64 * 1024, 5000);
	disk_transfers(fd, "posix_disk_write_64k", 1, 64 * 1024, 5000);
	disk_transfers(fd, "posix_disk_read_mib", 0, 1 << 20, 500);
	disk_transfers(fd, "posix_disk_write_mib", 1, 1 << 20, 500);
	close(fd);
}

int main(int argc, char **argv) {
	if(argc > 1 && !strcmp(argv[1], "--exit"))
		return 0;
	const char *disk_path = NULL;
	if(argc > 2 && !strcmp(argv[1], "--disk"))
		disk_path = argv[2];
	const char *self = argv[0];
	if(!self || !strchr(self, '/')) {
		printf("posix-bench: run me by my path, which the exec benchmark runs again\n");
		return 2;
	}
	int cpus[2];
	int count = allowed_cpus(cpus, 2);
	if(count == 0)
		fail("finding a CPU");
	home_cpu = cpus[0];
	pin(home_cpu);
	fork_wait(2000);
	fork_exec_wait(self, 500);
	pipe_throughput();
	signal_delivery(100000);
	poll_ready(100000);
	futex_pingpong("posix_futex_same_cpu", cpus[0], 50000);
	if(count > 1)
		futex_pingpong("posix_futex_cross_cpu", cpus[1], 50000);
	futex_processes("posix_futex_shared_same_cpu", cpus[0], 50000);
	if(count > 1)
		futex_processes("posix_futex_shared_cross_cpu", cpus[1], 50000);
	if(disk_path)
		disk(disk_path);
	printf("posix-bench: done\n");
	fflush(stdout);
	return 0;
}
