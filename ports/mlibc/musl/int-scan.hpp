// SPDX-FileCopyrightText: 2005-2020 Rich Felker, et al.
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: MIT

// Text to integer for scanf(3) and wscanf(3). This is musl 1.2.5's
// __intscan (src/internal/intscan.c, https://musl.libc.org, MIT), made a
// template over where the text comes from (an Input, as
// mlibc/float-scan.hpp describes it) in place of musl's FILE, without
// the pok argument that only musl's strto* functions pass, and taking
// C23's prefix 0b in bases 0 and 2 as it takes 0x in bases 0 and 16, as
// mlibc's strtol does. mlibc's own scanf took no field width for
// integers, so "%2d" read every digit.

#ifndef MLIBC_INT_SCAN_HPP
#define MLIBC_INT_SCAN_HPP

#include <ctype.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>

// musl's tests like c-'0'<10U are meant to compare unsigned: a character
// below '0' wraps around to a large number.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wsign-compare"

namespace mlibc::int_scan {

// The value of each digit, by character, from EOF (-1) up; 255 is no
// digit in any base.
inline constexpr unsigned char table[] = { 255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
  0,  1,  2,  3,  4,  5,  6,  7,  8,  9,255,255,255,255,255,255,
255, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35,255,255,255,255,255,
255, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,255,
};

// Reads an integer in `base` (0 for C's prefixes) from `f`; a value
// above `lim` is out of range (ERANGE).
template<typename Input>
unsigned long long scan_int(Input &f, unsigned base, unsigned long long lim)
{
	const unsigned char *val = table+1;
	int c, neg=0;
	unsigned x;
	unsigned long long y;
	if (base > 36 || base == 1) {
		errno = EINVAL;
		return 0;
	}
	while (isspace((c=f.get())));
	if (c=='+' || c=='-') {
		neg = -(c=='-');
		c = f.get();
	}
	if ((base == 0 || base == 16 || base == 2) && c=='0') {
		c = f.get();
		if ((c|32)=='x' && base != 2) {
			c = f.get();
			if (val[c]>=16) {
				f.unget();
				f.fail();
				return 0;
			}
			base = 16;
		} else if ((c|32)=='b' && base != 16) {
			c = f.get();
			if (val[c]>=2) {
				f.unget();
				f.fail();
				return 0;
			}
			base = 2;
		} else if (base == 0) {
			base = 8;
		}
	} else {
		if (base == 0) base = 10;
		if (val[c] >= base) {
			f.unget();
			f.fail();
			errno = EINVAL;
			return 0;
		}
	}
	if (base == 10) {
		for (x=0; c-'0'<10U && x<=UINT_MAX/10-1; c=f.get())
			x = x*10 + (c-'0');
		for (y=x; c-'0'<10U && y<=ULLONG_MAX/10 && 10*y<=ULLONG_MAX-(c-'0'); c=f.get())
			y = y*10 + (c-'0');
		if (c-'0'>=10U) goto done;
	} else if (!(base & (base-1))) {
		int bs = "\0\1\2\4\7\3\6\5"[(0x17*base)>>5&7];
		for (x=0; val[c]<base && x<=UINT_MAX/32; c=f.get())
			x = x<<bs | val[c];
		for (y=x; val[c]<base && y<=ULLONG_MAX>>bs; c=f.get())
			y = y<<bs | val[c];
	} else {
		for (x=0; val[c]<base && x<=UINT_MAX/36-1; c=f.get())
			x = x*base + val[c];
		for (y=x; val[c]<base && y<=ULLONG_MAX/base && base*y<=ULLONG_MAX-val[c]; c=f.get())
			y = y*base + val[c];
	}
	if (val[c]<base) {
		for (; val[c]<base; c=f.get());
		errno = ERANGE;
		y = lim;
		if (lim&1) neg = 0;
	}
done:
	f.unget();
	if (y>=lim) {
		if (!(lim&1) && !neg) {
			errno = ERANGE;
			return lim-1;
		} else if (y>lim) {
			errno = ERANGE;
			return lim;
		}
	}
	return (y^neg)-neg;
}

} // namespace mlibc::int_scan

#pragma GCC diagnostic pop

#endif // MLIBC_INT_SCAN_HPP
