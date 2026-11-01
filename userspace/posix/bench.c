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
// benchmarks), and with --nvme <device> that NVMe disk; `ob bench` gives
// both systems the same virtio disk and NVMe disk, files on the host.
// With --btrfs <directory> it reads, makes and removes files in that
// directory, where `ob bench` has each system mount the same Btrfs disk.
// With --network it also times TCP and UDP over the loopback interface
// (127.0.0.1), which Radix's Spark lets it reach and linux-bench brings
// up on Linux. With --git <program> it times `git status` in a
// repository it makes in /tmp, with that git: the same release on both,
// built for each (ADR 0294).

#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
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
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static uint64_t now_ns(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

// The kernel events `ob bench` counts per operation, which Radix's
// /processes/stat lists one a line ("ipis: 12"); Linux's /proc/stat has
// none of them.
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

// Where the kernel's counts are, on each system.
#ifdef __radix__
#define KERNEL_STAT "/processes/stat"
#else
#define KERNEL_STAT "/proc/stat"
#endif

// Reads the kernel's state; false if KERNEL_STAT lacks an event.
static int read_events(struct kernel_state *state) {
	memset(state, 0, sizeof(*state));
	FILE *file = fopen(KERNEL_STAT, "r");
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

// A chain of additions, each waiting for the one before, so one clock
// cycle each on any x86-64 processor: the time of one is the processor's
// clock cycle, which says how fast each system runs it. `ob bench
// --from` shows it beside the rows (ADR 0124); no row compares it.
static void cpu_cycle(uint64_t rounds) {
#ifdef __x86_64__
	uint64_t x = 0;
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++)
		__asm__ volatile(".rept 100\n\taddq $1, %0\n\t.endr" : "+r"(x));
	report("cpu_cycle", started, rounds * 100);
	if(x != rounds * 100)
		fail("cpu_cycle");
#else
	(void)rounds;
#endif
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

// 127.0.0.1 and `port`.
static struct sockaddr_in loopback(unsigned short port) {
	struct sockaddr_in in;
	memset(&in, 0, sizeof(in));
	in.sin_family = AF_INET;
	in.sin_port = htons(port);
	in.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
	return in;
}

// A socket of `type` bound to 127.0.0.1 at a port the kernel picks, and
// that port in `port`; a TCP one listens.
static int bound(int type, unsigned short *port) {
	int fd = socket(AF_INET, type, 0);
	if(fd < 0)
		fail("socket");
	struct sockaddr_in at = loopback(0);
	socklen_t length = sizeof(at);
	if(bind(fd, (struct sockaddr *)&at, sizeof(at)) != 0
	   || getsockname(fd, (struct sockaddr *)&at, &length) != 0)
		fail("bind");
	if(type == SOCK_STREAM && listen(fd, 16) != 0)
		fail("listen");
	*port = ntohs(at.sin_port);
	return fd;
}

// A TCP socket connected to 127.0.0.1 `port`, without Nagle's delay, as
// a program that answers each message sets it.
static int tcp_connected(unsigned short port) {
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	if(fd < 0)
		fail("socket");
	int on = 1;
	struct sockaddr_in to = loopback(port);
	if(setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &on, sizeof(on)) != 0)
		fail("TCP_NODELAY");
	if(connect(fd, (struct sockaddr *)&to, sizeof(to)) != 0)
		fail("connect");
	return fd;
}

// A TCP connection made, taken and closed, by one process: its own
// connect, accept, and both ends' close, on one CPU.
static void tcp_connect(uint64_t rounds) {
	unsigned short port;
	int server = bound(SOCK_STREAM, &port);
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++) {
		int client = tcp_connected(port);
		int accepted = accept(server, NULL, NULL);
		if(accepted < 0)
			fail("accept");
		close(client);
		close(accepted);
	}
	report("posix_tcp_connect", started, rounds);
	close(server);
}

// A child answers each byte this process sends over a TCP connection,
// or with `datagrams` each UDP datagram, with one of its own, the child
// on `other`: the time of each round trip, but the first.
static void net_roundtrip(const char *name, int datagrams, int other, uint64_t rounds) {
	unsigned short port;
	int listening = bound(datagrams ? SOCK_DGRAM : SOCK_STREAM, &port);
	pid_t child = fork();
	if(child < 0)
		fail("fork");
	if(child == 0) {
		pin(other);
		int fd = listening;
		struct sockaddr_in from;
		socklen_t length = sizeof(from);
		if(!datagrams) {
			fd = accept(listening, NULL, NULL);
			int on = 1;
			if(fd < 0 || setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &on, sizeof(on)) != 0)
				_exit(1);
		}
		char byte;
		for(uint64_t i = 0; i < rounds; i++) {
			if(datagrams) {
				length = sizeof(from);
				if(recvfrom(fd, &byte, 1, 0, (struct sockaddr *)&from, &length) != 1
				   || sendto(fd, &byte, 1, 0, (struct sockaddr *)&from, length) != 1)
					_exit(1);
			} else if(read(fd, &byte, 1) != 1 || write(fd, &byte, 1) != 1) {
				_exit(1);
			}
		}
		_exit(0);
	}
	int fd;
	if(datagrams) {
		fd = socket(AF_INET, SOCK_DGRAM, 0);
		struct sockaddr_in to = loopback(port);
		if(fd < 0 || connect(fd, (struct sockaddr *)&to, sizeof(to)) != 0)
			fail("connect");
	} else {
		fd = tcp_connected(port);
	}
	char byte = 'x';
	if(write(fd, &byte, 1) != 1 || read(fd, &byte, 1) != 1)
		fail("the first round trip");
	struct mark started = start();
	for(uint64_t i = 1; i < rounds; i++)
		if(write(fd, &byte, 1) != 1 || read(fd, &byte, 1) != 1)
			fail("a round trip");
	report(name, started, rounds - 1);
	int status;
	if(waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))
		fail("the answering child");
	close(fd);
	close(listening);
}

// A child sends PIPE_TOTAL bytes over a TCP connection, PIPE_CHUNK at a
// time, and this process reads them, both on this CPU, as the pipe's
// throughput is measured: the time per MiB.
static void tcp_throughput(void) {
	static char buffer[PIPE_CHUNK];
	unsigned short port;
	int listening = bound(SOCK_STREAM, &port);
	struct mark started = start();
	pid_t child = fork();
	if(child < 0)
		fail("fork");
	if(child == 0) {
		close(listening);
		int fd = tcp_connected(port);
		memset(buffer, 'x', sizeof(buffer));
		for(size_t sent = 0; sent < PIPE_TOTAL; sent += PIPE_CHUNK)
			for(size_t done = 0; done < PIPE_CHUNK;) {
				ssize_t n = write(fd, buffer + done, PIPE_CHUNK - done);
				if(n <= 0)
					_exit(1);
				done += (size_t)n;
			}
		_exit(0);
	}
	int fd = accept(listening, NULL, NULL);
	if(fd < 0)
		fail("accept");
	size_t got = 0;
	for(;;) {
		ssize_t n = read(fd, buffer, sizeof(buffer));
		if(n < 0)
			fail("read");
		if(n == 0)
			break;
		got += (size_t)n;
	}
	int status;
	if(waitpid(child, &status, 0) != child || got != PIPE_TOTAL)
		fail("the TCP writer");
	report("posix_tcp_mib", started, PIPE_TOTAL >> 20);
	close(fd);
	close(listening);
}

// The network benchmarks, over the loopback interface.
static void network(const int *cpus, int count) {
	tcp_connect(2000);
	net_roundtrip("posix_tcp_roundtrip_same_cpu", 0, cpus[0], 20000);
	if(count > 1)
		net_roundtrip("posix_tcp_roundtrip_cross_cpu", 0, cpus[1], 20000);
	net_roundtrip("posix_udp_roundtrip_same_cpu", 1, cpus[0], 20000);
	if(count > 1)
		net_roundtrip("posix_udp_roundtrip_cross_cpu", 1, cpus[1], 20000);
	tcp_throughput();
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
// always raw, and its C library takes O_DIRECT and ignores it): a
// virtio disk's rows, or with `nvme` an NVMe disk's.
static void disk(const char *path, int nvme) {
	static const struct {
		const char *names[2];
		int writing;
		size_t size;
		uint64_t rounds;
	} runs[] = {
		{{"posix_disk_read_4k", "posix_nvme_read_4k"}, 0, 4096, 20000},
		{{"posix_disk_write_4k", "posix_nvme_write_4k"}, 1, 4096, 20000},
		{{"posix_disk_read_64k", "posix_nvme_read_64k"}, 0, 64 * 1024, 5000},
		{{"posix_disk_write_64k", "posix_nvme_write_64k"}, 1, 64 * 1024, 5000},
		{{"posix_disk_read_mib", "posix_nvme_read_mib"}, 0, 1 << 20, 500},
		{{"posix_disk_write_mib", "posix_nvme_write_mib"}, 1, 1 << 20, 500},
	};
	int fd = open(path, O_RDWR | O_DIRECT);
	if(fd < 0)
		fail(path);
	off_t size = lseek(fd, 0, SEEK_END);
	if(size < (off_t)DISK_SPAN) {
		printf("posix-bench: %s holds %lld bytes, fewer than %u\n", path, (long long)size,
		       DISK_SPAN);
		exit(1);
	}
	for(size_t i = 0; i < sizeof(runs) / sizeof(runs[0]); i++)
		disk_transfers(fd, runs[i].names[nvme], runs[i].writing, runs[i].size, runs[i].rounds);
	close(fd);
}

// The files of the Btrfs disk `ob bench` makes with Linux's mkfs.btrfs,
// each BTRFS_FILE bytes: `plain/<n>`, of bytes that do not compress, and
// `zstd/<n>`, of text that zstd compresses, in 128 KiB extents as Linux
// writes compressed files.
#define BTRFS_FILES 32
#define BTRFS_FILE (1u << 20)

// What a read of a file asks for at a time: what cp and cat ask for.
#define BTRFS_READ (128u << 10)

// Reads each of the files `dir`/`kind`/<n> once, whole, BTRFS_READ bytes
// at a time: from the disk, as nothing has read them since the machine
// started.
static void btrfs_reads(const char *dir, const char *kind, const char *name) {
	char *buffer = malloc(BTRFS_READ);
	if(!buffer)
		fail("malloc");
	char path[256];
	struct mark started = start();
	for(int i = 0; i < BTRFS_FILES; i++) {
		snprintf(path, sizeof(path), "%s/%s/%d", dir, kind, i);
		int fd = open(path, O_RDONLY);
		if(fd < 0)
			fail(path);
		size_t total = 0;
		ssize_t n;
		while((n = read(fd, buffer, BTRFS_READ)) > 0)
			total += (size_t)n;
		if(n < 0 || total != BTRFS_FILE)
			fail(path);
		close(fd);
	}
	report(name, started, BTRFS_FILES);
	free(buffer);
}

// Makes a file in `dir`, writes 4 KiB to it, closes it and removes it,
// `rounds` times: changes that the filesystem keeps in memory, as nothing
// syncs them.
static void btrfs_create(const char *dir, uint64_t rounds) {
	char path[256];
	static char data[4096];
	memset(data, 0x5a, sizeof(data));
	snprintf(path, sizeof(path), "%s/made", dir);
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++) {
		int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
		if(fd < 0 || write(fd, data, sizeof(data)) != (ssize_t)sizeof(data))
			fail("making a file");
		close(fd);
		if(unlink(path) != 0)
			fail("removing a file");
	}
	report("posix_btrfs_create", started, rounds);
}

// Writes 4 KiB to a file in `dir` and makes it durable with fsync,
// `rounds` times, each to the next of its first 16 blocks: what a
// database's log does. Linux writes the change to Btrfs's log tree;
// Radix commits a transaction (ADR 0151).
static void btrfs_fsync(const char *dir, uint64_t rounds) {
	char path[256];
	static char data[4096];
	uint32_t state = 0x9e3779b9;
	for(size_t i = 0; i < sizeof(data); i++) {
		state = state * 1664525 + 1013904223;
		data[i] = (char)(state >> 24);
	}
	snprintf(path, sizeof(path), "%s/synced", dir);
	int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
	if(fd < 0)
		fail(path);
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++) {
		off_t at = (off_t)(i % 16) * (off_t)sizeof(data);
		if(pwrite(fd, data, sizeof(data), at) != (ssize_t)sizeof(data) || fsync(fd) != 0)
			fail("writing and syncing a file");
	}
	report("posix_btrfs_fsync", started, rounds);
	close(fd);
}

// Writes BTRFS_FILES files `dir`/written/<n>, each the text of `zstd/0`,
// BTRFS_READ bytes at a time, and then has the filesystem write them to
// the disk: compressed, as the disk is mounted to compress with zstd
// (Linux's `compress=zstd:1`, Spark's `--compress`). On Radix, syncing a
// directory syncs its filesystem, as syncfs does on Linux.
static void btrfs_writes(const char *dir) {
	char *text = malloc(BTRFS_FILE);
	if(!text)
		fail("malloc");
	char path[256];
	snprintf(path, sizeof(path), "%s/zstd/0", dir);
	int fd = open(path, O_RDONLY);
	if(fd < 0 || read(fd, text, BTRFS_FILE) != (ssize_t)BTRFS_FILE)
		fail(path);
	close(fd);
	snprintf(path, sizeof(path), "%s/written", dir);
	if(mkdir(path, 0755) != 0)
		fail(path);
	struct mark started = start();
	for(int i = 0; i < BTRFS_FILES; i++) {
		snprintf(path, sizeof(path), "%s/written/%d", dir, i);
		fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
		if(fd < 0)
			fail(path);
		for(size_t at = 0; at < BTRFS_FILE; at += BTRFS_READ)
			if(write(fd, text + at, BTRFS_READ) != (ssize_t)BTRFS_READ)
				fail(path);
		close(fd);
	}
	snprintf(path, sizeof(path), "%s/written", dir);
	fd = open(path, O_RDONLY | O_DIRECTORY);
#ifdef __linux__
	if(fd < 0 || syncfs(fd) != 0)
#else
	if(fd < 0 || fsync(fd) != 0)
#endif
		fail("syncing the written files");
	report("posix_btrfs_write_zstd_mib", started, BTRFS_FILES);
	close(fd);
	free(text);
}

// The Btrfs benchmarks, in the directory `dir` where the Btrfs disk is.
static void btrfs(const char *dir) {
	btrfs_reads(dir, "plain", "posix_btrfs_read_mib");
	btrfs_reads(dir, "zstd", "posix_btrfs_read_zstd_mib");
	btrfs_create(dir, 5000);
	btrfs_fsync(dir, 200);
	btrfs_writes(dir);
}

// The repository of the git benchmark: GIT_FOLDERS folders of
// GIT_FILES files each, every one committed.
#define GIT_FOLDERS 10
#define GIT_FILES 100

// Runs git with `args` in the repository `dir`, its output thrown away,
// and fails unless it succeeds.
static void run_git(const char *git, const char *dir, const char *const *args) {
	const char *argv[16] = {git, "-C", dir};
	size_t n = 3;
	for(size_t i = 0; args[i] && n < 15; i++)
		argv[n++] = args[i];
	argv[n] = NULL;
	pid_t child = fork();
	if(child < 0)
		fail("fork");
	if(child == 0) {
		int null = open("/dev/null", O_WRONLY);
		if(null >= 0) {
			dup2(null, 1);
			dup2(null, 2);
		}
		execv(git, (char *const *)argv);
		_exit(127);
	}
	int status;
	if(waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) {
		errno = 0;
		fail(args[0]);
	}
}

// Removes `path` and everything beneath it.
static void remove_tree(const char *path) {
	DIR *dir = opendir(path);
	if(dir) {
		struct dirent *entry;
		while((entry = readdir(dir))) {
			if(!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, ".."))
				continue;
			char below[512];
			snprintf(below, sizeof(below), "%s/%s", path, entry->d_name);
			remove_tree(below);
		}
		closedir(dir);
		rmdir(path);
	} else {
		unlink(path);
	}
}

// `git status` in a repository of 1,000 files in 10 folders, all
// committed and none changed since: git reads its index, looks at each
// file's status against it, and finds nothing to say. Both systems run
// it with no settings of the system's or the user's.
static void git_status(const char *git, uint64_t rounds) {
	mkdir("/tmp", 01777);
	char dir[] = "/tmp/posix-bench-git-XXXXXX";
	if(!mkdtemp(dir))
		fail("making the git benchmark's folder");
	char path[256];
	for(int d = 0; d < GIT_FOLDERS; d++) {
		snprintf(path, sizeof(path), "%s/folder%d", dir, d);
		if(mkdir(path, 0755) != 0)
			fail(path);
		for(int f = 0; f < GIT_FILES; f++) {
			snprintf(path, sizeof(path), "%s/folder%d/file%d.c", dir, d, f);
			FILE *file = fopen(path, "w");
			if(!file)
				fail(path);
			fprintf(file, "// File %d of folder %d.\nint value_%d_%d = %d;\n", f, d, d, f, d * f);
			fclose(file);
		}
	}
	static const char *const env[][2] = {{"GIT_CONFIG_NOSYSTEM", "1"},
			{"GIT_CONFIG_GLOBAL", "/dev/null"}, {"GIT_AUTHOR_NAME", "Bench"},
			{"GIT_AUTHOR_EMAIL", "bench@dukovo.org"}, {"GIT_COMMITTER_NAME", "Bench"},
			{"GIT_COMMITTER_EMAIL", "bench@dukovo.org"},
			{"GIT_AUTHOR_DATE", "2026-11-01T12:00:00Z"},
			{"GIT_COMMITTER_DATE", "2026-11-01T12:00:00Z"}};
	for(size_t i = 0; i < sizeof(env) / sizeof(env[0]); i++)
		setenv(env[i][0], env[i][1], 1);
	setenv("HOME", dir, 1);
	run_git(git, dir, (const char *const[]){"init", "-q", "-b", "main", NULL});
	run_git(git, dir, (const char *const[]){"add", "-A", NULL});
	run_git(git, dir, (const char *const[]){"commit", "-q", "-m", "Files", NULL});
	// Once before timing, so that the index's times are settled and
	// every file's status is in the file cache, as when one works.
	const char *const status[] = {"status", "--porcelain", NULL};
	run_git(git, dir, status);
	struct mark started = start();
	for(uint64_t i = 0; i < rounds; i++)
		run_git(git, dir, status);
	report("posix_git_status", started, rounds);
	remove_tree(dir);
}

int main(int argc, char **argv) {
	if(argc > 1 && !strcmp(argv[1], "--exit"))
		return 0;
	const char *disk_path = NULL, *nvme_path = NULL, *btrfs_path = NULL, *git = NULL;
	int networked = 0;
	for(int i = 1; i < argc; i++) {
		if(!strcmp(argv[i], "--network"))
			networked = 1;
		else if(i + 1 < argc && !strcmp(argv[i], "--disk"))
			disk_path = argv[++i];
		else if(i + 1 < argc && !strcmp(argv[i], "--nvme"))
			nvme_path = argv[++i];
		else if(i + 1 < argc && !strcmp(argv[i], "--btrfs"))
			btrfs_path = argv[++i];
		else if(i + 1 < argc && !strcmp(argv[i], "--git"))
			git = argv[++i];
	}
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
	cpu_cycle(1000000);
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
	if(networked)
		network(cpus, count);
	if(disk_path)
		disk(disk_path, 0);
	if(nvme_path)
		disk(nvme_path, 1);
	if(btrfs_path)
		btrfs(btrfs_path);
	if(git)
		git_status(git, 50);
	printf("posix-bench: done\n");
	fflush(stdout);
	return 0;
}
