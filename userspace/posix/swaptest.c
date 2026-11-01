// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the swap boot test runs (ADR 0152): `swaptest <device> <MiB>`
// turns on the swap area on <device> with swapon(8), fills <MiB> of
// memory, more than the machine has, with a pattern, and checks it all
// as it comes back from swap; forks, and has the child check half of it
// and change a quarter while the parent's stays as it was; checks that
// swapoff(8) cannot turn the area off while more is out on it than memory
// has room for; then keeps the first 16 MiB, or as much of them as half
// the memory left free has room for, and turns the area off while some of
// those are still out on it, and checks them once more. On the way it
// checks that the area cannot be turned on twice, that the device
// refuses writes while it is on, and what /processes says of it.

#include <fcntl.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>

#include "check.h"
#include "programs.h"

#define PAGE 4096
#define WORDS (PAGE / sizeof(unsigned long))
// The pages kept for swapoff(8) to bring back: at most 16 MiB.
#define KEPT (16 * 256)

// The value of word `word` of page `page` of the memory, as a fill made
// with `seed` wrote it.
static unsigned long pattern(size_t page, size_t word, unsigned long seed) {
	return (page * WORDS + word) * 0x9e3779b97f4a7c15ul ^ seed;
}

// Writes the pattern of `seed` over pages `first` to `end` of `memory`,
// every `step`th page.
static void fill(unsigned long *memory, size_t first, size_t end, size_t step,
		unsigned long seed) {
	for(size_t page = first; page < end; page += step)
		for(size_t word = 0; word < WORDS; word++)
			memory[page * WORDS + word] = pattern(page, word, seed);
}

// How many of pages `first` to `end` of `memory`, every `step`th, do not
// hold the pattern of `seed`.
static size_t wrong(const unsigned long *memory, size_t first, size_t end, size_t step,
		unsigned long seed) {
	size_t bad = 0;
	for(size_t page = first; page < end; page += step)
		for(size_t word = 0; word < WORDS; word++)
			if(memory[page * WORDS + word] != pattern(page, word, seed)) {
				bad++;
				break;
			}
	return bad;
}

// The number after `key` in the file `path`, or -1.
static long long field(const char *path, const char *key) {
	char text[8192];
	text[0] = 0;
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if(fd >= 0) {
		ssize_t got = read(fd, text, sizeof(text) - 1);
		text[got > 0 ? got : 0] = 0;
		close(fd);
	}
	const char *at = strstr(text, key);
	return at ? atoll(at + strlen(key)) : -1;
}

int main(int argc, char **argv) {
	check_start("swaptest");
	if(argc != 3) {
		printf("swaptest: usage: swaptest <device> <MiB>\n");
		return 1;
	}
	const char *device = argv[1];
	size_t pages = (size_t)atol(argv[2]) * 256;

	CHECK(field("/processes/meminfo", "swap_total: ") == 0);
	STATUS(0, "/bin/swapon", device);
	long long total = field("/processes/meminfo", "swap_total: ");
	CHECK(total > 0);
	printf("swaptest: swap is on, %lld KiB\n", total);
	// Once only, and the device takes no writes while it is on.
	STATUS(1, "/bin/swapon", device);
	int fd = open(device, O_WRONLY | O_CLOEXEC);
	CHECK(fd >= 0);
	CHECK_ERROR(pwrite(fd, "x", 1, PAGE), EBUSY);
	close(fd);
	CONTAINS(" partition ", "/bin/swapon", "--show");

	unsigned long *memory =
			mmap(NULL, pages * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	CHECK(memory != MAP_FAILED);
	if(memory == MAP_FAILED)
		return check_end();
	fill(memory, 0, pages, 1, 1);
	printf("swaptest: filled %zu MiB\n", pages / 256);
	CHECK(wrong(memory, 0, pages, 1, 1) == 0);
	long long out = field("/processes/stat", "swap_outs: ");
	CHECK(out > 0);
	CHECK(field("/processes/stat", "swap_ins: ") > 0);
	CHECK(field("/processes/meminfo", "swap_free: ") < total);
	printf("swaptest: read it back, %lld pages swapped out\n", out);

	// A child shares what is swapped out, and its writes are its own.
	pid_t child = check_fork();
	if(child == 0) {
		CHECK(wrong(memory, 0, pages, 2, 1) == 0);
		fill(memory, 0, pages, 4, 2);
		CHECK(wrong(memory, 0, pages, 4, 2) == 0);
		CHECK(wrong(memory, 1, pages, 4, 1) == 0);
		check_exit_child();
	}
	CHECK_CHILD(child);
	CHECK(wrong(memory, 0, pages, 1, 1) == 0);
	printf("swaptest: a child changed its copy\n");

	// With more out on it than memory has room for, the area cannot be
	// turned off, and stays on.
	STATUS(2, "/bin/swapoff", device);
	CHECK(field("/processes/meminfo", "swap_total: ") == total);

	// What is let go of frees its slots; what is kept comes back when
	// the area is turned off with pages on it. The boot archive takes
	// most of a small machine's memory, and more as programs are added
	// to it, so as much is kept as half the memory left free has room
	// for, and the rest is let go of too.
	size_t kept = pages < KEPT ? pages : KEPT;
	CHECK(munmap(memory + kept * WORDS, (pages - kept) * PAGE) == 0);
	long long free_kib = field("/processes/meminfo", "\nfree: ");
	CHECK(free_kib > 0);
	size_t fits = free_kib > 0 ? (size_t)free_kib / 4 / 2 : 0;
	if(fits < kept) {
		CHECK(fits > 0);
		CHECK(munmap(memory + fits * WORDS, (kept - fits) * PAGE) == 0);
		kept = fits;
	}
	printf("swaptest: kept %zu KiB\n", kept * 4);
	CHECK(field("/processes/meminfo", "swap_free: ") < total);
	STATUS(0, "/bin/swapoff", device);
	CHECK(field("/processes/meminfo", "swap_total: ") == 0);
	OUTPUT("", "", "/bin/swapon", "--show");
	CHECK(wrong(memory, 0, kept, 1, 1) == 0);
	STATUS(4, "/bin/swapoff", device);
	printf("swaptest: swap is off\n");
	munmap(memory, kept * PAGE);
	return check_end();
}
