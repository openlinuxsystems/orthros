// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the spark-network boot test runs as its services (ADR 0184): it
// checks that Spark gave it the network its service file asks for, and
// nothing more. With `granted` that is a capability to connect out, then
// one to listen on ports 8000 to 8009 and one on port 9000, and the C
// library's sockets go through them: it listens on ports 8005 and 9000
// and connects to the first, but cannot bind port 8010. With `none` it
// holds no network capability, and cannot make a socket.

#include <arpa/inet.h>
#include <netinet/in.h>
#include <radix/radix.h>
#include <sys/socket.h>

#include "check.h"

// What a network slot should hold: the rights and the ports, or nothing
// for an empty one (rights 0).
struct wanted {
	uint64_t rights;
	uint32_t first;
	uint32_t count;
};

// 127.0.0.1 and `port`.
static struct sockaddr_in loopback(unsigned short port) {
	struct sockaddr_in in;
	memset(&in, 0, sizeof(in));
	in.sin_family = AF_INET;
	in.sin_port = htons(port);
	in.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
	return in;
}

// Checks that the network slots hold what `wanted` says, in order, and
// that the rest are empty.
static void holds(const struct wanted *wanted, size_t count) {
	for(uint32_t i = 0; i < RADIX_STARTUP_NETWORK_SLOTS; i++) {
		uint32_t slot = RADIX_STARTUP_SLOT_NETWORK + i;
		struct radix_capability_info_results info;
		uint64_t status = radix_capability_info(slot, &info);
		if(i >= count) {
			if(status == 0)
				printf("netgrant: FAILED: slot %u holds a capability of kind %u\n", slot,
						(unsigned)info.kind);
			CHECK(status != 0);
			continue;
		}
		CHECK(status == 0);
		if(status)
			continue;
		CHECK(info.kind == RADIX_OBJECT_KIND_NETWORK);
		if(info.rights != wanted[i].rights)
			printf("netgrant: FAILED: slot %u has the rights %#llx, not %#llx\n", slot,
					(unsigned long long)info.rights, (unsigned long long)wanted[i].rights);
		CHECK(info.rights == wanted[i].rights);
		struct radix_network_ports_results ports;
		CHECK(radix_network_ports(slot, &ports) == 0);
		if(ports.first != wanted[i].first || ports.count != wanted[i].count)
			printf("netgrant: FAILED: slot %u covers %u ports from %u\n", slot,
					(unsigned)ports.count, (unsigned)ports.first);
		CHECK(ports.first == wanted[i].first && ports.count == wanted[i].count);
	}
}

// A TCP socket bound to 127.0.0.1 `port` and listening there.
static int listening(unsigned short port) {
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	CHECK(fd >= 0);
	struct sockaddr_in at = loopback(port);
	CHECK(bind(fd, (struct sockaddr *)&at, sizeof(at)) == 0);
	CHECK(listen(fd, 1) == 0);
	return fd;
}

static void granted(void) {
	uint64_t kept = RADIX_RIGHTS_COPY | RADIX_RIGHTS_TRANSFER;
	const struct wanted wanted[] = {
			{RADIX_RIGHTS_SEND | kept, 0, 65536},
			{RADIX_RIGHTS_RECEIVE | kept, 8000, 10},
			{RADIX_RIGHTS_RECEIVE | kept, 9000, 1},
	};
	holds(wanted, sizeof(wanted) / sizeof(wanted[0]));
	// It listens on its ports, through the second capability and the
	// third, and connects out through the first.
	int server = listening(8005);
	int other = listening(9000);
	int client = socket(AF_INET, SOCK_STREAM, 0);
	CHECK(client >= 0);
	struct sockaddr_in to = loopback(8005);
	CHECK(connect(client, (struct sockaddr *)&to, sizeof(to)) == 0);
	int accepted = accept(server, NULL, NULL);
	CHECK(accepted >= 0);
	CHECK(write(client, "granted", 7) == 7);
	char buffer[8];
	CHECK(read(accepted, buffer, sizeof(buffer)) == 7 && memcmp(buffer, "granted", 7) == 0);
	close(accepted);
	close(client);
	close(other);
	close(server);
	// No capability covers port 8010.
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	struct sockaddr_in at = loopback(8010);
	CHECK_ERROR(bind(fd, (struct sockaddr *)&at, sizeof(at)), EACCES);
	close(fd);
	printf("netgrant: Spark gave it the network to connect out and to listen on ports "
		   "8000-8009 and 9000\n");
}

static void none(void) {
	holds(NULL, 0);
	CHECK_ERROR(socket(AF_INET, SOCK_STREAM, 0), EACCES);
	CHECK_ERROR(socket(AF_INET, SOCK_DGRAM, 0), EACCES);
	printf("netgrant: Spark gave it no network\n");
}

int main(int argc, char **argv) {
	check_start("netgrant");
	if(argc == 2 && strcmp(argv[1], "granted") == 0)
		RUN(granted);
	else if(argc == 2 && strcmp(argv[1], "none") == 0)
		RUN(none);
	else
		check_failed(__FILE__, __LINE__, "its argument is granted or none", EINVAL);
	int code = check_end();
	// The last line, which the boot test waits for, either way.
	printf("netgrant: done\n");
	return code;
}
