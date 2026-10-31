// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the dhcp boot test runs (ADR 0191), beside netd(8), the one
// service Spark starts: a virtio network card that starts with no
// address gets one from QEMU's DHCP server through netd. The card has
// the address, the router and the DNS server QEMU gives, in ip(8) and in
// resolv.conf, and netd keeps the lease; a connection goes through it.
// Sockets on other cards may share netd's port 68, one a card. netd
// started again goes on with the lease, and the connection with it; an
// administrator's address stays through what netd changes; the settings
// file gives the card an address of its own, and taken away, has it ask
// DHCP again. Spark gives it the whole network (--network).

#include <arpa/inet.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sys/socket.h>

#include "check.h"
#include "programs.h"

#define NET0 "/kernel/class/net/net0/"
#define LEASE "/system/state/netd/52-54-00-12-34-56.lease"
#define RESOLV "/etc/resolv.conf"
#define SETTINGS "/system/settings/network"

// QEMU's user network: its router and DHCP server, its DNS server, the
// first address it offers, and where it runs `cat` (the test's guestfwd).
#define ROUTER "10.0.2.2"
#define DNS "10.0.2.3"
#define OFFERED "10.0.2.15"
#define ECHO "10.0.2.100"

// What the file at `path` says, in `out`; empty if there is none.
static void read_text(const char *path, char *out, size_t size) {
	out[0] = 0;
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if(fd < 0)
		return;
	ssize_t got = read(fd, out, size - 1);
	close(fd);
	out[got > 0 ? got : 0] = 0;
}

// Waits up to `seconds` for the file at `path` to have `needle` in it;
// says whether it came to.
static int wait_file(const char *path, const char *needle, int seconds) {
	char text[1024];
	for(int tenth = 0; tenth < seconds * 10; tenth++) {
		read_text(path, text, sizeof(text));
		if(strstr(text, needle))
			return 1;
		usleep(100 * 1000);
	}
	printf("dhcp: %s says \"%s\" after %d seconds\n", path, text, seconds);
	return 0;
}

// A connection to QEMU's `cat`, which sends `word` back.
static int connect_echo(void) {
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	struct sockaddr_in to = {
		.sin_family = AF_INET,
		.sin_port = htons(7),
		.sin_addr.s_addr = inet_addr(ECHO),
	};
	CHECK(connect(fd, (struct sockaddr *)&to, sizeof(to)) == 0);
	return fd;
}

static void echo(int fd, const char *word) {
	size_t len = strlen(word);
	CHECK(write(fd, word, len) == (ssize_t)len);
	char back[32] = {0};
	size_t got = 0;
	while(got < len) {
		ssize_t n = read(fd, back + got, len - got);
		CHECK(n > 0);
		if(n <= 0)
			break;
		got += (size_t)n;
	}
	CHECK(got == len && !memcmp(back, word, len));
}

// netd gives the card what QEMU's DHCP server offers.
static int connection;

static void leased(void) {
	CHECK(wait_file(NET0 "ipv4", OFFERED "/24", 60));
	OUTPUT("", "net0             UP             " OFFERED "/24 \n", "/bin/ip", "-br", "address",
	       "show", "net0");
	CONTAINS("default via " ROUTER " dev net0 \n", "/bin/ip", "route");
	CHECK(wait_file(RESOLV, "nameserver " DNS "\n", 10));
	CONTAINS("# Written by netd(8)", "/bin/head", "-n", "1", RESOLV);
	// /etc/resolv.conf is a link Spark copied from the defaults.
	char target[64] = {0};
	CHECK(readlink("/system/settings/resolv.conf", target, sizeof(target) - 1) > 0);
	CHECK(!strcmp(target, "/system/live/netd/resolv.conf"));
	CONTAINS("address = " OFFERED "/24\n", "/bin/cat", LEASE);
	CONTAINS("server = " ROUTER "\n", "/bin/cat", LEASE);
	connection = connect_echo();
	struct sockaddr_in at;
	socklen_t len = sizeof(at);
	CHECK(getsockname(connection, (struct sockaddr *)&at, &len) == 0);
	CHECK(at.sin_addr.s_addr == inet_addr(OFFERED));
	echo(connection, "leased\n");
	printf("dhcp: netd gave net0 " OFFERED "/24, the router and the DNS server\n");
}

// A UDP socket bound to the interface `device` ("" for none) and then to
// port 68, or -1 with errno.
static int port_68(const char *device) {
	int fd = socket(AF_INET, SOCK_DGRAM, 0);
	if(*device && setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, device, strlen(device)) != 0)
		return -1;
	struct sockaddr_in at = {.sin_family = AF_INET, .sin_port = htons(68)};
	if(bind(fd, (struct sockaddr *)&at, sizeof(at)) != 0) {
		int error = errno;
		close(fd);
		errno = error;
		return -1;
	}
	return fd;
}

// Port 68 is netd's on net0, and others' on other interfaces, one each.
static void shared(void) {
	CHECK_ERROR(port_68(""), EADDRINUSE);
	CHECK_ERROR(port_68("net0"), EADDRINUSE);
	int lo = port_68("lo");
	CHECK(lo >= 0);
	CHECK_ERROR(port_68("lo"), EADDRINUSE);
	// It shares the port, so it stays on lo.
	CHECK_ERROR(setsockopt(lo, SOL_SOCKET, SO_BINDTODEVICE, "", 0), EINVAL);
	struct sockaddr_in self = {
		.sin_family = AF_INET,
		.sin_port = htons(68),
		.sin_addr.s_addr = inet_addr("127.0.0.1"),
	};
	CHECK(sendto(lo, "x", 1, 0, (struct sockaddr *)&self, sizeof(self)) == 1);
	char byte = 0;
	CHECK(recv(lo, &byte, 1, 0) == 1 && byte == 'x');
	close(lo);
	CHECK_ERROR(port_68(""), EADDRINUSE);
	int again = port_68("lo");
	CHECK(again >= 0);
	close(again);
}

// netd started again goes on with the lease the card has, and the
// connection goes on through it.
static void restarted(void) {
	STATUS(0, "/bin/service", "netd", "restart");
	// The new netd's own word, which the boot test looks for too.
	sleep(3);
	char text[64];
	read_text(NET0 "ipv4", text, sizeof(text));
	CHECK(!strcmp(text, OFFERED "/24\n"));
	echo(connection, "restarted\n");
	CONTAINS("address = " OFFERED "/24\n", "/bin/cat", LEASE);
}

static void write_settings(const char *text) {
	int fd = open(SETTINGS, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
	CHECK(fd >= 0);
	CHECK(write(fd, text, strlen(text)) == (ssize_t)strlen(text));
	close(fd);
}

// The settings file's address in place of the lease, an administrator's
// address beside both, and DHCP again once the settings say nothing.
static void settings(void) {
	STATUS(0, "/bin/ip", "address", "add", "192.168.77.1/24", "dev", "net0");
	write_settings("[net0]\naddress = 10.0.2.20/24\nrouter = " ROUTER "\ndns = 10.0.2.4\n");
	STATUS(0, "/bin/service", "netd", "reload");
	// netd takes the lease's address away, then gives the settings'.
	CHECK(wait_file(NET0 "ipv4", "10.0.2.20/24", 20));
	char text[128];
	read_text(NET0 "ipv4", text, sizeof(text));
	CHECK(!strcmp(text, "10.0.2.20/24\n192.168.77.1/24\n"));
	CHECK(wait_file(RESOLV, "nameserver 10.0.2.4\n", 10));
	CHECK(access(LEASE, F_OK) != 0 && errno == ENOENT);
	CONTAINS("default via " ROUTER " dev net0 \n", "/bin/ip", "route");

	write_settings("# Every card by DHCP.\n");
	STATUS(0, "/bin/service", "netd", "reload");
	CHECK(wait_file(NET0 "ipv4", OFFERED "/24", 60));
	read_text(NET0 "ipv4", text, sizeof(text));
	CHECK(!strcmp(text, OFFERED "/24\n192.168.77.1/24\n"));
	CHECK(wait_file(RESOLV, "nameserver " DNS "\n", 10));
	CHECK(wait_file(LEASE, "address = " OFFERED "/24\n", 10));
	STATUS(0, "/bin/ip", "address", "del", "192.168.77.1/24", "dev", "net0");
	int fd = connect_echo();
	echo(fd, "again\n");
	close(fd);
	printf("dhcp: the settings set net0 up, and DHCP again once they said nothing\n");
}

int main(void) {
	check_start("dhcp");
	RUN(leased);
	RUN(shared);
	RUN(restarted);
	RUN(settings);
	close(connection);
	return check_end();
}
