// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the interfaces boot test runs (ADR 0190): a virtio network card
// that starts with no address is set up as a DHCP client and ip(8) set
// one up. Its name and number, through the C library and
// /kernel/class/net; ip and ss showing it bare; a DHCPDISCOVER sent from
// 0.0.0.0 through a socket bound to the card with SO_BINDTODEVICE, which
// QEMU's DHCP server answers with an offer the socket receives, though
// the card has no address yet; ip giving the card the offered address
// and the router, and showing them; a connection through the card to
// QEMU's `cat` at 10.0.2.100 port 7, which ss lists; what the card
// counted; and ip taking it all away again. Spark gives it the whole
// network (--network), with the control right that sets cards up.

#include <arpa/inet.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <stddef.h>
#include <sys/socket.h>
#include <sys/time.h>

#include "check.h"
#include "programs.h"

// The card's own files.
#define NET0 "/kernel/class/net/net0/"

// QEMU's user network: its router and DHCP server, the first address it
// offers, and where it runs `cat` (the test's guestfwd).
#define ROUTER "10.0.2.2"
#define OFFERED "10.0.2.15"
#define ECHO "10.0.2.100"

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

// The number the file at `path` says, or -1.
static long long read_number(const char *path) {
	char text[32];
	return read_text(path, text, sizeof(text)) ? -1 : strtoll(text, NULL, 10);
}

// The card is net0, number 2, after lo.
static void names(void) {
	CHECK(if_nametoindex("net0") == 2);
	char name[IF_NAMESIZE];
	CHECK(if_indextoname(2, name) && !strcmp(name, "net0"));
	struct if_nameindex *list = if_nameindex();
	CHECK(list != NULL);
	if(list) {
		CHECK(list[0].if_index == 1 && !strcmp(list[0].if_name, "lo"));
		CHECK(list[1].if_index == 2 && !strcmp(list[1].if_name, "net0"));
		CHECK(list[2].if_index == 0 && list[2].if_name == NULL);
		if_freenameindex(list);
	}
	char text[64];
	CHECK(read_text(NET0 "address", text, sizeof(text)) == 0
			&& !strcmp(text, "52:54:00:12:34:56"));
	CHECK(read_number(NET0 "mtu") == 1500);
	CHECK(read_number(NET0 "carrier") == 1);
	CHECK(read_text(NET0 "operstate", text, sizeof(text)) == 0 && !strcmp(text, "up"));
	CHECK(read_text(NET0 "ipv4", text, sizeof(text)) == 0 && !strcmp(text, ""));
}

// Before it is set up, the card has no address and no route.
static void bare(void) {
	OUTPUT("", "lo               UNKNOWN        127.0.0.1/8 \n"
	           "net0             UP             \n",
	       "/bin/ip", "-br", "address");
	OUTPUT("", "", "/bin/ip", "route");
}

// A DHCP message (RFC 2131): a DHCPDISCOVER, and the offer that answers
// it, with room for the 312 bytes of options every client takes.
struct bootp {
	uint8_t op, htype, hlen, hops;
	uint32_t xid;
	uint16_t secs, flags;
	uint32_t ciaddr, yiaddr, siaddr, giaddr;
	uint8_t chaddr[16];
	uint8_t sname[64];
	uint8_t file[128];
	uint8_t cookie[4];
	uint8_t options[308];
};

_Static_assert(sizeof(struct bootp) == 548, "a DHCP message is 548 bytes at least");

static const uint8_t COOKIE[4] = {99, 130, 83, 99};

// The value of option `code` in `options`, of `len` bytes, and its length.
static const uint8_t *option(const uint8_t *options, size_t len, uint8_t code, size_t *size) {
	for(size_t at = 0; at < len && options[at] != 255;) {
		if(options[at] == 0) {
			at++;
			continue;
		}
		if(at + 2 > len || at + 2 + options[at + 1] > len)
			return NULL;
		if(options[at] == code) {
			*size = options[at + 1];
			return &options[at + 2];
		}
		at += 2 + options[at + 1];
	}
	return NULL;
}

// The address QEMU offered, in network order.
static uint32_t offered;

// A DHCPDISCOVER from 0.0.0.0 port 68 to 255.255.255.255 port 67, through
// a socket bound to net0, and the DHCPOFFER QEMU broadcasts back.
static void offer(void) {
	int fd = socket(AF_INET, SOCK_DGRAM, 0);
	CHECK(fd >= 0);
	CHECK(setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, "net0", 4) == 0);
	char device[IF_NAMESIZE];
	socklen_t len = sizeof(device);
	CHECK(getsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, device, &len) == 0);
	CHECK(len == 5 && !strcmp(device, "net0"));
	struct sockaddr_in client = {.sin_family = AF_INET, .sin_port = htons(68)};
	CHECK(bind(fd, (struct sockaddr *)&client, sizeof(client)) == 0);
	struct timeval wait = {.tv_sec = 5};
	CHECK(setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &wait, sizeof(wait)) == 0);

	struct bootp discover;
	memset(&discover, 0, sizeof(discover));
	discover.op = 1;
	discover.htype = 1;
	discover.hlen = 6;
	discover.xid = htonl(0x0c0ffee0);
	// The server broadcasts its answer: the client has no address yet.
	discover.flags = htons(0x8000);
	const uint8_t mac[6] = {0x52, 0x54, 0x00, 0x12, 0x34, 0x56};
	memcpy(discover.chaddr, mac, sizeof(mac));
	memcpy(discover.cookie, COOKIE, sizeof(COOKIE));
	const uint8_t options[] = {53, 1, 1, 255};
	memcpy(discover.options, options, sizeof(options));
	struct sockaddr_in everyone = {
		.sin_family = AF_INET,
		.sin_port = htons(67),
		.sin_addr.s_addr = htonl(INADDR_BROADCAST),
	};
	// To every host only when the socket says so, as on Linux.
	CHECK_ERROR(sendto(fd, &discover, sizeof(discover), 0, (struct sockaddr *)&everyone,
			sizeof(everyone)), EACCES);
	int on = 1;
	CHECK(setsockopt(fd, SOL_SOCKET, SO_BROADCAST, &on, sizeof(on)) == 0);
	CHECK(sendto(fd, &discover, sizeof(discover), 0, (struct sockaddr *)&everyone,
			sizeof(everyone)) == (ssize_t)sizeof(discover));

	struct bootp answer;
	struct sockaddr_in from;
	socklen_t from_len = sizeof(from);
	memset(&answer, 0, sizeof(answer));
	ssize_t got = recvfrom(fd, &answer, sizeof(answer), 0, (struct sockaddr *)&from, &from_len);
	CHECK(got >= (ssize_t)offsetof(struct bootp, options));
	if(got < (ssize_t)offsetof(struct bootp, options)) {
		close(fd);
		return;
	}
	CHECK(from.sin_addr.s_addr == inet_addr(ROUTER) && from.sin_port == htons(67));
	CHECK(answer.op == 2 && answer.xid == discover.xid);
	CHECK(!memcmp(answer.chaddr, mac, sizeof(mac)) && !memcmp(answer.cookie, COOKIE, 4));
	size_t options_len = (size_t)got - offsetof(struct bootp, options);
	size_t size = 0;
	const uint8_t *type = option(answer.options, options_len, 53, &size);
	CHECK(type && size == 1 && type[0] == 2);
	const uint8_t *server = option(answer.options, options_len, 54, &size);
	CHECK(server && size == 4 && !memcmp(server, &from.sin_addr.s_addr, 4));
	offered = answer.yiaddr;
	struct in_addr address = {.s_addr = offered};
	printf("interfaces: %s offered %s to net0, which had no address\n", ROUTER,
			inet_ntoa(address));
	CHECK(offered == inet_addr(OFFERED));
	close(fd);
}

// ip gives the card the offered address and the router, and shows them;
// what it may not do, it says so.
static void configure(void) {
	STATUS(0, "/bin/ip", "address", "add", OFFERED "/24", "dev", "net0");
	STATUS(0, "/bin/ip", "route", "add", "default", "via", ROUTER);
	OUTPUT("", "net0             UP             " OFFERED "/24 \n",
	       "/bin/ip", "-br", "address", "show", "net0");
	OUTPUT("", "default via " ROUTER " dev net0 \n"
	           "10.0.2.0/24 dev net0 proto kernel scope link src " OFFERED " \n",
	       "/bin/ip", "route");
	CONTAINS("    inet " OFFERED "/24 brd 10.0.2.255 scope global net0\n", "/bin/ip", "a");
	STATUS(2, "/bin/ip", "address", "add", OFFERED "/24", "dev", "net0");
	STATUS(2, "/bin/ip", "route", "add", "default", "via", ROUTER);
	STATUS(2, "/bin/ip", "route", "replace", "default", "via", "172.16.0.1");
	char text[64];
	CHECK(read_text(NET0 "ipv4", text, sizeof(text)) == 0 && !strcmp(text, OFFERED "/24"));
}

// A connection through the card from its new address, to QEMU's `cat`,
// which ss lists while it is open.
static void echo(void) {
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	struct sockaddr_in to = {
		.sin_family = AF_INET,
		.sin_port = htons(7),
		.sin_addr.s_addr = inet_addr(ECHO),
	};
	CHECK(connect(fd, (struct sockaddr *)&to, sizeof(to)) == 0);
	struct sockaddr_in at;
	socklen_t len = sizeof(at);
	CHECK(getsockname(fd, (struct sockaddr *)&at, &len) == 0);
	CHECK(at.sin_addr.s_addr == offered);
	CHECK(write(fd, "ping\n", 5) == 5);
	char back[8] = {0};
	size_t got = 0;
	while(got < 5) {
		ssize_t n = read(fd, back + got, 5 - got);
		CHECK(n > 0);
		if(n <= 0)
			break;
		got += (size_t)n;
	}
	CHECK(got == 5 && !memcmp(back, "ping\n", 5));
	char line[96];
	snprintf(line, sizeof(line), "%s:%u", OFFERED, ntohs(at.sin_port));
	CONTAINS(line, "/bin/ss", "-tn");
	CONTAINS("ESTAB", "/bin/ss", "-t");
	close(fd);
	printf("interfaces: a connection from %s to %s port 7 echoed what it sent\n", OFFERED, ECHO);
}

// What the card counted: the offer and the echo came, the discover and
// the connection went.
static void counted(void) {
	CHECK(read_number(NET0 "statistics/rx_packets") >= 3);
	CHECK(read_number(NET0 "statistics/tx_packets") >= 3);
	CHECK(read_number(NET0 "statistics/rx_bytes") > 300);
	CONTAINS("  net0:", "/bin/cat", "/processes/net/dev");
}

// ip takes the router and the address away again.
static void bare_again(void) {
	STATUS(0, "/bin/ip", "route", "del", "default");
	OUTPUT("", "10.0.2.0/24 dev net0 proto kernel scope link src " OFFERED " \n",
	       "/bin/ip", "route");
	STATUS(2, "/bin/ip", "route", "del", "default");
	STATUS(0, "/bin/ip", "address", "flush", "dev", "net0");
	OUTPUT("", "net0             UP             \n", "/bin/ip", "-br", "address", "show",
	       "dev", "net0");
	OUTPUT("", "", "/bin/ip", "route");
	STATUS(2, "/bin/ip", "address", "add", "127.0.0.2/8", "dev", "lo");
}

int main(void) {
	check_start("interfaces");
	RUN(names);
	RUN(bare);
	RUN(offer);
	RUN(configure);
	RUN(echo);
	RUN(counted);
	RUN(bare_again);
	return check_end();
}
