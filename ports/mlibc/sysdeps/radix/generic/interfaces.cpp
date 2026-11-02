// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Network interfaces' names and numbers on Radix (ADR 0190):
// if_nametoindex, if_indextoname and if_nameindex read /kernel/class/net,
// where each interface is a directory of its name with its number in
// `ifindex` (radix-sysfs(5)), as Linux's C libraries ask the kernel
// through ioctl and netlink. getaddrinfo's AI_ADDRCONFIG reads their
// `ipv4` and `ipv6` files, for whether the machine has addresses of each
// family (ADR 0332).

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <mlibc/all-sysdeps.hpp>
#include <mlibc/allocator.hpp>
#include <net/if.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

namespace {

constexpr char NET[] = "/kernel/class/net/";

// Whether `name` could be an interface's: a file name shorter than
// IFNAMSIZ, so that a path made of it stays in the interface's directory.
bool plausible(const char *name, size_t len) {
	return len > 0 && len < IFNAMSIZ && !memchr(name, '/', len)
			&& strcmp(name, ".") && strcmp(name, "..");
}

// The number of the interface `len` bytes of `name` names, read from its
// `ifindex`, or 0 if there is none.
unsigned int index_of(const char *name, size_t len) {
	char path[sizeof(NET) + IFNAMSIZ + sizeof("/ifindex")];
	memcpy(path, NET, sizeof(NET) - 1);
	memcpy(path + sizeof(NET) - 1, name, len);
	strcpy(path + sizeof(NET) - 1 + len, "/ifindex");
	int fd;
	if(mlibc::sysdep<Open>(path, O_RDONLY | O_CLOEXEC, 0, &fd))
		return 0;
	char text[16];
	ssize_t got = 0;
	int e = mlibc::sysdep<Read>(fd, text, sizeof(text) - 1, &got);
	mlibc::sysdep<Close>(fd);
	if(e || got <= 0)
		return 0;
	text[got] = 0;
	return static_cast<unsigned int>(strtoul(text, nullptr, 10));
}

// Calls `f` with each interface's name and its length, in the listing's
// order, until it returns false; an errno if the interfaces cannot be
// listed.
template<typename F>
int each_interface(F f) {
	int dir;
	if(int e = mlibc::sysdep<OpenDir>(NET, &dir); e)
		return e;
	alignas(struct dirent) char entries[1024];
	for(bool more = true; more;) {
		size_t got = 0;
		if(int e = mlibc::sysdep<ReadEntries>(dir, entries, sizeof(entries), &got); e) {
			mlibc::sysdep<Close>(dir);
			return e;
		}
		more = got > 0;
		for(size_t at = 0; at < got && more;) {
			auto entry = reinterpret_cast<struct dirent *>(entries + at);
			at += entry->d_reclen;
			size_t len = strlen(entry->d_name);
			if(plausible(entry->d_name, len))
				more = f(entry->d_name, len);
		}
	}
	mlibc::sysdep<Close>(dir);
	return 0;
}

// Whether the file at `path`, of an interface's directory, holds a line
// that starts with `start` (`len` bytes, none for any line).
bool has_line(const char *path, const char *start, size_t len) {
	int fd;
	if(mlibc::sysdep<Open>(path, O_RDONLY | O_CLOEXEC, 0, &fd))
		return false;
	char text[1024];
	ssize_t got = 0;
	int e = mlibc::sysdep<Read>(fd, text, sizeof(text) - 1, &got);
	mlibc::sysdep<Close>(fd);
	if(e || got <= 0)
		return false;
	text[got] = 0;
	for(const char *line = text; *line;) {
		if(*line != '\n' && !strncmp(line, start, len))
			return true;
		const char *end = strchr(line, '\n');
		if(!end)
			break;
		line = end + 1;
	}
	return false;
}

} // namespace

namespace mlibc {

// As glibc counts them: any address but a loopback one, link-local IPv6
// ones too. `lo` has the cards' IPv4 addresses after its own, and their
// IPv6 ones, so only the cards are asked.
int Sysdeps<InetConfigured>::operator()(bool *ipv4, bool *ipv6) {
	*ipv4 = *ipv6 = false;
	return each_interface([&](const char *name, size_t len) {
		if(len == 2 && !memcmp(name, "lo", 2))
			return true;
		char path[sizeof(NET) + IFNAMSIZ + sizeof("/ipv4")];
		memcpy(path, NET, sizeof(NET) - 1);
		memcpy(path + sizeof(NET) - 1, name, len);
		strcpy(path + sizeof(NET) - 1 + len, "/ipv4");
		*ipv4 = *ipv4 || has_line(path, "", 0);
		strcpy(path + sizeof(NET) - 1 + len, "/ipv6");
		*ipv6 = *ipv6 || has_line(path, "address ", 8);
		return !(*ipv4 && *ipv6);
	});
}

int Sysdeps<IfNametoindex>::operator()(const char *name, unsigned int *ret) {
	size_t len = strlen(name);
	*ret = plausible(name, len) ? index_of(name, len) : 0;
	return *ret ? 0 : ENODEV;
}

int Sysdeps<IfIndextoname>::operator()(unsigned int index, char *name) {
	bool found = false;
	if(int e = each_interface([&](const char *each, size_t len) {
			found = index && index_of(each, len) == index;
			if(found)
				memcpy(name, each, len + 1);
			return !found;
		}); e)
		return e;
	return found ? 0 : ENXIO;
}

// The list and its names are one allocation, which IfFreeNameindex frees:
// the entries, the last all zeros, then a name of IFNAMSIZ bytes for each.
int Sysdeps<IfNameindex>::operator()(struct if_nameindex **out) {
	size_t count = 0;
	if(int e = each_interface([&](const char *, size_t) {
			count++;
			return true;
		}); e)
		return e;
	size_t names = (count + 1) * sizeof(struct if_nameindex);
	auto list = static_cast<struct if_nameindex *>(
			getAllocator().allocate(names + count * IFNAMSIZ));
	if(!list)
		return ENOBUFS;
	// An interface that comes between the two listings is left out.
	size_t filled = 0;
	if(int e = each_interface([&](const char *name, size_t len) {
			if(filled == count)
				return false;
			unsigned int index = index_of(name, len);
			if(!index)
				return true;
			char *place = reinterpret_cast<char *>(list) + names + filled * IFNAMSIZ;
			memcpy(place, name, len + 1);
			list[filled++] = {index, place};
			return true;
		}); e) {
		getAllocator().free(list);
		return e;
	}
	list[filled] = {0, nullptr};
	*out = list;
	return 0;
}

void Sysdeps<IfFreeNameindex>::operator()(struct if_nameindex *list) {
	if(list)
		getAllocator().free(list);
}

} // namespace mlibc
