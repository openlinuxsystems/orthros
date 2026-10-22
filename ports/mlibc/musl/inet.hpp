// SPDX-FileCopyrightText: 2005-2020 Rich Felker, et al.
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: MIT

// Text to Internet addresses and back for inet_pton(3) and inet_ntop(3).
// This is musl 1.2.5's inet_pton and inet_ntop (src/network/inet_pton.c
// and inet_ntop.c, https://musl.libc.org, MIT), in C++. mlibc's own took
// "1.2.03.4" and "1.2.3.4.5" for IPv4 addresses and refused
// "1:2:3:4:5:6:7::", "::1:2:3:4:5:6:7" and "::ffff:192.168.0.1".

#ifndef MLIBC_INET_HPP
#define MLIBC_INET_HPP

#include <ctype.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>

// musl's tests like c-'0'<10 are meant to compare unsigned: a character
// below '0' wraps around to a large number.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wsign-compare"

namespace mlibc::inet {

inline int hexval(unsigned c)
{
	if (c-'0'<10) return c-'0';
	c |= 32;
	if (c-'a'<6) return c-'a'+10;
	return -1;
}

// IPv4 takes four decimal numbers, each 0 to 255 without a leading zero;
// IPv6 takes RFC 4291's forms, "::" and a final IPv4 address included.
inline int pton(int af, const char *s, void *a0)
{
	uint16_t ip[8];
	unsigned char *a = static_cast<unsigned char *>(a0);
	int i, j, v, d, brk=-1, need_v4=0;

	if (af==AF_INET) {
		for (i=0; i<4; i++) {
			for (v=j=0; j<3 && isdigit(s[j]); j++)
				v = 10*v + s[j]-'0';
			if (j==0 || (j>1 && s[0]=='0') || v>255) return 0;
			a[i] = v;
			if (s[j]==0 && i==3) return 1;
			if (s[j]!='.') return 0;
			s += j+1;
		}
		return 0;
	} else if (af!=AF_INET6) {
		errno = EAFNOSUPPORT;
		return -1;
	}

	if (*s==':' && *++s!=':') return 0;

	for (i=0; ; i++) {
		if (s[0]==':' && brk<0) {
			brk=i;
			ip[i&7]=0;
			if (!*++s) break;
			if (i==7) return 0;
			continue;
		}
		for (v=j=0; j<4 && (d=hexval(s[j]))>=0; j++)
			v=16*v+d;
		if (j==0) return 0;
		ip[i&7] = v;
		if (!s[j] && (brk>=0 || i==7)) break;
		if (i==7) return 0;
		if (s[j]!=':') {
			if (s[j]!='.' || (i<6 && brk<0)) return 0;
			need_v4=1;
			i++;
			ip[i&7]=0;
			break;
		}
		s += j+1;
	}
	if (brk>=0) {
		memmove(ip+brk+7-i, ip+brk, 2*(i+1-brk));
		for (j=0; j<7-i; j++) ip[brk+j] = 0;
	}
	for (j=0; j<8; j++) {
		*a++ = ip[j]>>8;
		*a++ = ip[j];
	}
	if (need_v4 && pton(AF_INET, s, a-4) <= 0) return 0;
	return 1;
}

// IPv6 addresses take "::" for their longest run of zero groups, and an
// IPv4-mapped one (::ffff:0:0/96) ends in an IPv4 address.
inline const char *ntop(int af, const void *a0, char *s, socklen_t l)
{
	const unsigned char *a = static_cast<const unsigned char *>(a0);
	int i, j, max, best;
	char buf[100];

	switch (af) {
	case AF_INET:
		if (snprintf(s, l, "%d.%d.%d.%d", a[0],a[1],a[2],a[3]) < l)
			return s;
		break;
	case AF_INET6:
		if (memcmp(a, "\0\0\0\0\0\0\0\0\0\0\377\377", 12))
			snprintf(buf, sizeof buf,
				"%x:%x:%x:%x:%x:%x:%x:%x",
				256*a[0]+a[1],256*a[2]+a[3],
				256*a[4]+a[5],256*a[6]+a[7],
				256*a[8]+a[9],256*a[10]+a[11],
				256*a[12]+a[13],256*a[14]+a[15]);
		else
			snprintf(buf, sizeof buf,
				"%x:%x:%x:%x:%x:%x:%d.%d.%d.%d",
				256*a[0]+a[1],256*a[2]+a[3],
				256*a[4]+a[5],256*a[6]+a[7],
				256*a[8]+a[9],256*a[10]+a[11],
				a[12],a[13],a[14],a[15]);
		/* Replace longest /(^0|:)[:0]{2,}/ with "::" */
		for (i=best=0, max=2; buf[i]; i++) {
			if (i && buf[i] != ':') continue;
			j = strspn(buf+i, ":0");
			if (j>max) best=i, max=j;
		}
		if (max>3) {
			buf[best] = buf[best+1] = ':';
			memmove(buf+best+2, buf+best+max, i-best-max+1);
		}
		if (strlen(buf) < l) {
			strcpy(s, buf);
			return s;
		}
		break;
	default:
		errno = EAFNOSUPPORT;
		return 0;
	}
	errno = ENOSPC;
	return 0;
}

} // namespace mlibc::inet

#pragma GCC diagnostic pop

#endif // MLIBC_INET_HPP
