// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the network-cards boot test runs (ADR 0197): for each card it is
// given, a name and the /24 of QEMU's user network behind it
// (`nics net0 10.0.2 net1 10.0.3`), it waits for netd(8) to give the card
// the address QEMU's DHCP server offers, .15, then sends 2 MiB through
// the card to QEMU's `cat` at .100 port 7 and checks that every byte
// comes back, in order: some thousands of frames each way, round each of
// the card's rings many times. The card's counters must have seen them,
// and the stack never have had a frame to send without a slot for it.
// Spark gives it the whole network (--network).

#include <arpa/inet.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdint.h>
#include <sys/socket.h>
#include <sys/time.h>

#include "check.h"

// How much goes through each card, and how much at a time.
#define ECHOED (2u << 20)
#define CHUNK 16384

// How long a card has to get its address from netd, in tenths of a
// second.
#define ADDRESS_WAIT 900

// What the file at `path` says, without its newline, in `out`.
static int read_text(const char *path, char *out, size_t size) {
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if(fd < 0)
		return -1;
	ssize_t got = read(fd, out, size - 1);
	close(fd);
	if(got < 0)
		return -1;
	out[got] = 0;
	if(got && out[got - 1] == '\n')
		out[got - 1] = 0;
	return 0;
}

// The number the file `name` of card `card`'s directory says, or -1.
static long long card_number(const char *card, const char *name) {
	char path[96], text[32];
	snprintf(path, sizeof(path), "/kernel/class/net/%s/%s", card, name);
	return read_text(path, text, sizeof(text)) ? -1 : strtoll(text, NULL, 10);
}

// The byte at `at` of what goes through the card numbered `seed`: no two
// nearby the same, so that a frame lost, repeated or out of order shows.
static uint8_t pattern(uint32_t at, uint32_t seed) {
	return (uint8_t)(at ^ (at >> 8) ^ (at >> 16) ^ (seed * 37));
}

static long long now_ms(void) {
	struct timespec now;
	clock_gettime(CLOCK_MONOTONIC, &now);
	return (long long)now.tv_sec * 1000 + now.tv_nsec / 1000000;
}

// Waits for netd to give `card` the address `want`; whether it did.
static int wait_for_address(const char *card, const char *want) {
	char path[96], text[64];
	snprintf(path, sizeof(path), "/kernel/class/net/%s/ipv4", card);
	for(int tries = 0; tries < ADDRESS_WAIT; tries++) {
		if(read_text(path, text, sizeof(text)) == 0 && !strcmp(text, want))
			return 1;
		struct timespec tenth = {.tv_nsec = 100000000};
		nanosleep(&tenth, NULL);
	}
	printf("nics: FAILED: %s has \"%s\", not %s\n", card, text, want);
	return 0;
}

static uint8_t out[CHUNK], back[CHUNK];

// Sends ECHOED bytes through `card` to `cat` at `echo` port 7 and reads
// them back, as they come, checking each.
static void echo_through(const char *card, const char *address, const char *echo, uint32_t seed) {
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	CHECK(fd >= 0);
	if(fd < 0)
		return;
	CHECK(setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, card, strlen(card)) == 0);
	struct sockaddr_in to = {
		.sin_family = AF_INET,
		.sin_port = htons(7),
		.sin_addr.s_addr = inet_addr(echo),
	};
	long long started = now_ms();
	CHECK(connect(fd, (struct sockaddr *)&to, sizeof(to)) == 0);
	struct sockaddr_in at;
	socklen_t len = sizeof(at);
	CHECK(getsockname(fd, (struct sockaddr *)&at, &len) == 0);
	CHECK(at.sin_addr.s_addr == inet_addr(address));
	CHECK(fcntl(fd, F_SETFL, O_NONBLOCK) == 0);
	uint32_t sent = 0, got = 0, wrong = 0;
	int stalls = 0;
	while(got < ECHOED && stalls < 30) {
		struct pollfd entry = {.fd = fd, .events = POLLIN | (sent < ECHOED ? POLLOUT : 0)};
		int ready = poll(&entry, 1, 1000);
		if(ready <= 0) {
			stalls++;
			continue;
		}
		stalls = 0;
		if((entry.revents & POLLOUT) && sent < ECHOED) {
			uint32_t n = ECHOED - sent < CHUNK ? ECHOED - sent : CHUNK;
			for(uint32_t i = 0; i < n; i++)
				out[i] = pattern(sent + i, seed);
			ssize_t wrote = write(fd, out, n);
			if(wrote > 0)
				sent += (uint32_t)wrote;
			else
				CHECK(errno == EAGAIN);
		}
		if(entry.revents & (POLLIN | POLLHUP | POLLERR)) {
			ssize_t n = read(fd, back, sizeof(back));
			if(n == 0)
				break;
			if(n < 0) {
				CHECK(errno == EAGAIN);
				continue;
			}
			for(ssize_t i = 0; i < n; i++)
				wrong += back[i] != pattern(got + (uint32_t)i, seed);
			got += (uint32_t)n;
		}
	}
	long long took = now_ms() - started;
	CHECK(got == ECHOED && sent == ECHOED);
	CHECK(wrong == 0);
	close(fd);
	if(got == ECHOED && wrong == 0)
		printf("nics: %s echoed 2 MiB through %s port 7 in %lld ms\n", card, echo, took);
	else
		printf("nics: FAILED: %s echoed %u of %u bytes, %u of them wrong\n", card, got, sent,
				wrong);
}

// The card `card`, behind QEMU's user network `net` (its first three
// numbers).
static void card(const char *card, const char *net, uint32_t seed) {
	char address[32], cidr[40], echo[32];
	snprintf(address, sizeof(address), "%s.15", net);
	snprintf(cidr, sizeof(cidr), "%s/24", address);
	snprintf(echo, sizeof(echo), "%s.100", net);
	CHECK(if_nametoindex(card) != 0);
	if(!wait_for_address(card, cidr)) {
		check_failures++;
		return;
	}
	printf("nics: %s has %s\n", card, cidr);
	long long received = card_number(card, "statistics/rx_packets");
	long long sent = card_number(card, "statistics/tx_packets");
	long long lost = card_number(card, "statistics/tx_dropped");
	echo_through(card, address, echo, seed);
	// 2 MiB in frames of 1460 bytes at most is 1437 of them each way, and
	// none was sent while the card had no slot for it.
	CHECK(card_number(card, "statistics/rx_packets") - received >= 1437);
	CHECK(card_number(card, "statistics/tx_packets") - sent >= 1437);
	CHECK(card_number(card, "statistics/tx_dropped") == lost);
}

int main(int argc, char **argv) {
	check_start("nics");
	CHECK(argc >= 3 && argc % 2 == 1);
	for(int at = 1; at + 1 < argc; at += 2) {
		printf("nics: %s\n", argv[at]);
		card(argv[at], argv[at + 1], (uint32_t)at);
	}
	return check_end();
}
