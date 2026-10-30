// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the machine-check-program boot test runs (ADR 0168): it fills a
// page of its own, says where the page is and where it reads it, and then
// reads it, until the test has QEMU say the page's memory went bad. Its
// SIGBUS handler says what it was told, reads the page once more, which
// must fault again (the page has left its memory), says how much memory
// /processes/meminfo counts as bad, and ends.

#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static volatile char *page;
static volatile sig_atomic_t faults;

// The line of /processes/meminfo that counts memory that went bad.
static void print_corrupted(void) {
	FILE *file = fopen("/processes/meminfo", "r");
	if(!file) {
		printf("memfail: no /processes/meminfo\n");
		return;
	}
	char line[128];
	while(fgets(line, sizeof line, file))
		if(strncmp(line, "hardware_corrupted:", 19) == 0)
			printf("memfail: %s", line);
	fclose(file);
}

static void on_bus(int signal, siginfo_t *info, void *context) {
	(void)signal;
	(void)context;
	if(faults++ == 0) {
		printf("memfail: SIGBUS, code %d, address %p, lsb %d\n", info->si_code, info->si_addr,
		       info->si_addr_lsb);
		fflush(stdout);
		// SA_NODEFER lets this read's fault come back here.
		(void)page[0];
		printf("memfail: read the bad page again\n");
		_exit(1);
	}
	printf("memfail: SIGBUS again, code %d, address %p\n", info->si_code, info->si_addr);
	print_corrupted();
	fflush(stdout);
	_exit(0);
}

int main(void) {
	struct sigaction action = {0};
	action.sa_sigaction = on_bus;
	action.sa_flags = SA_SIGINFO | SA_NODEFER;
	if(sigaction(SIGBUS, &action, NULL) != 0) {
		perror("memfail: sigaction");
		return 1;
	}
	void *mapped = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if(mapped == MAP_FAILED) {
		perror("memfail: mmap");
		return 1;
	}
	page = mapped;
	memset(mapped, 0x5a, 4096);
	print_corrupted();
	printf("memfail: reading %p from %p\n", mapped, (void *)main);
	fflush(stdout);
	for(;;)
		(void)page[0];
}
