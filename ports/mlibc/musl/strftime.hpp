// SPDX-FileCopyrightText: 2005-2020 Rich Felker, et al.
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: MIT

// strftime(3) and wcsftime(3): musl 1.2.5's (src/time/strftime.c and
// src/time/wcsftime.c, https://musl.libc.org, MIT), with mlibc's
// locales. They have every conversion C and POSIX name (mlibc's had no
// %u, %w, %V, %G, %g, %W or %s and ended the program on them), the '-',
// '_', '0' and '+' flags and widths, and keep the text around %c, %x and
// %X, which mlibc's dropped. To these, as glibc has them: %k and %l (the
// hour on 24 and 12 hours, padded with a space), %P (am or pm), the '^'
// flag (upper case) and the '#' flag (upper case for names, lower case
// for %p and %Z), and an unknown conversion is copied as it stands,
// where musl's would make them return 0.

#ifndef MLIBC_MUSL_STRFTIME_HPP
#define MLIBC_MUSL_STRFTIME_HPP

#include <ctype.h>
#include <langinfo.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <wchar.h>
#include <wctype.h>
#include <mlibc/calendar.hpp>
#include <mlibc/locale.hpp>

// musl's tests like t[d]-'0'<10U are meant to compare unsigned: a
// character below '0' wraps around to a large number.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wsign-compare"

namespace mlibc::musl_time {

inline int is_leap(int y)
{
	/* Avoid overflow */
	if (y>INT_MAX-1900) y -= 2000;
	y += 1900;
	return !(y%4) && ((y%100) || !(y%400));
}

inline int week_num(const struct tm *tm)
{
	int val = (tm->tm_yday + 7U - (tm->tm_wday+6U)%7) / 7;
	/* If 1 Jan is just 1-3 days past Monday,
	 * the previous week is also in this year. */
	if ((tm->tm_wday + 371U - tm->tm_yday - 2) % 7 <= 2)
		val++;
	if (!val) {
		val = 52;
		/* If 31 December of prev year a Thursday,
		 * or Friday of a leap year, then the
		 * prev year has 53 weeks. */
		int dec31 = (tm->tm_wday + 7U - tm->tm_yday - 1) % 7;
		if (dec31 == 4 || (dec31 == 5 && is_leap(tm->tm_year%400-1)))
			val++;
	} else if (val == 53) {
		/* If 1 January is not a Thursday, and not
		 * a Wednesday of a leap year, then this
		 * year has only 52 weeks. */
		int jan1 = (tm->tm_wday + 371U - tm->tm_yday) % 7;
		if (jan1 != 4 && (jan1 != 3 || !is_leap(tm->tm_year)))
			val = 1;
	}
	return val;
}

inline size_t strftime_l(char *__restrict s, size_t n, const char *__restrict f,
		const struct tm *__restrict tm, localeinfo *loc);

// Whether `c` is a conversion fmt_1 knows.
inline bool is_conversion(int c)
{
	return c > 0 && c < 128 && strchr("aAbBcCdDeFgGhHIjklmMnpPrRsStTuUVwWxXyYzZ%", c);
}

// The case glibc's '^' (`upper`) and '#' (`swap`) flags give conversion
// `c`: 1 for upper case, -1 for lower case, 0 to leave it.
inline int case_of(int c, bool upper, bool swap)
{
	if (swap && c < 128 && strchr("aAbBh", c)) return 1;
	if (swap && (c == 'p' || c == 'Z')) return -1;
	return upper ? 1 : 0;
}

inline const char *fmt_1(char (*s)[100], size_t *l, int f, const struct tm *tm, localeinfo *loc, int pad)
{
	nl_item item;
	long long val;
	const char *fmt = "-";
	int width = 2, def_pad = '0';

	switch (f) {
	case 'a':
		if (tm->tm_wday > 6U) goto string;
		item = ABDAY_1 + tm->tm_wday;
		goto nl_strcat;
	case 'A':
		if (tm->tm_wday > 6U) goto string;
		item = DAY_1 + tm->tm_wday;
		goto nl_strcat;
	case 'h':
	case 'b':
		if (tm->tm_mon > 11U) goto string;
		item = ABMON_1 + tm->tm_mon;
		goto nl_strcat;
	case 'B':
		if (tm->tm_mon > 11U) goto string;
		item = MON_1 + tm->tm_mon;
		goto nl_strcat;
	case 'c':
		item = D_T_FMT;
		goto nl_strftime;
	case 'C':
		val = (1900LL+tm->tm_year) / 100;
		goto number;
	case 'e':
		def_pad = '_';
		[[fallthrough]];
	case 'd':
		val = tm->tm_mday;
		goto number;
	case 'D':
		fmt = "%m/%d/%y";
		goto recu_strftime;
	case 'F':
		fmt = "%Y-%m-%d";
		goto recu_strftime;
	case 'g':
	case 'G':
		val = tm->tm_year + 1900LL;
		if (tm->tm_yday < 3 && week_num(tm) != 1) val--;
		else if (tm->tm_yday > 360 && week_num(tm) == 1) val++;
		if (f=='g') val %= 100;
		else width = 4;
		goto number;
	case 'k':
		def_pad = '_';
		[[fallthrough]];
	case 'H':
		val = tm->tm_hour;
		goto number;
	case 'l':
		def_pad = '_';
		[[fallthrough]];
	case 'I':
		val = tm->tm_hour;
		if (!val) val = 12;
		else if (val > 12) val -= 12;
		goto number;
	case 'j':
		val = tm->tm_yday+1;
		width = 3;
		goto number;
	case 'm':
		val = tm->tm_mon+1;
		goto number;
	case 'M':
		val = tm->tm_min;
		goto number;
	case 'n':
		*l = 1;
		return "\n";
	case 'p':
		item = tm->tm_hour >= 12 ? PM_STR : AM_STR;
		goto nl_strcat;
	case 'P':
		// glibc's: %p in lower case.
		fmt = mlibc::nl_langinfo_l(tm->tm_hour >= 12 ? PM_STR : AM_STR, loc);
		for (*l = 0; fmt[*l] && *l < sizeof *s; ++*l)
			(*s)[*l] = static_cast<char>(tolower(static_cast<unsigned char>(fmt[*l])));
		return *s;
	case 'r':
		item = T_FMT_AMPM;
		goto nl_strftime;
	case 'R':
		fmt = "%H:%M";
		goto recu_strftime;
	case 's':
		val = tm_to_secs(tm) - tm->tm_gmtoff;
		width = 1;
		goto number;
	case 'S':
		val = tm->tm_sec;
		goto number;
	case 't':
		*l = 1;
		return "\t";
	case 'T':
		fmt = "%H:%M:%S";
		goto recu_strftime;
	case 'u':
		val = tm->tm_wday ? tm->tm_wday : 7;
		width = 1;
		goto number;
	case 'U':
		val = (tm->tm_yday + 7U - tm->tm_wday) / 7;
		goto number;
	case 'W':
		val = (tm->tm_yday + 7U - (tm->tm_wday+6U)%7) / 7;
		goto number;
	case 'V':
		val = week_num(tm);
		goto number;
	case 'w':
		val = tm->tm_wday;
		width = 1;
		goto number;
	case 'x':
		item = D_FMT;
		goto nl_strftime;
	case 'X':
		item = T_FMT;
		goto nl_strftime;
	case 'y':
		val = (tm->tm_year + 1900LL) % 100;
		if (val < 0) val = -val;
		goto number;
	case 'Y':
		val = tm->tm_year + 1900LL;
		if (val >= 10000) {
			*l = snprintf(*s, sizeof *s, "+%lld", val);
			return *s;
		}
		width = 4;
		goto number;
	case 'z':
		if (tm->tm_isdst < 0) {
			*l = 0;
			return "";
		}
		*l = snprintf(*s, sizeof *s, "%+.4ld",
			tm->tm_gmtoff/3600*100 + tm->tm_gmtoff%3600/60);
		return *s;
	case 'Z':
		if (tm->tm_isdst < 0) {
			*l = 0;
			return "";
		}
		// The zone's name from the tm itself (musl checks it against
		// its own time zone data).
		fmt = tm->tm_zone ? tm->tm_zone : "";
		goto string;
	case '%':
		*l = 1;
		return "%";
	default:
		return 0;
	}
number:
	switch (pad ? pad : def_pad) {
	case '-': *l = snprintf(*s, sizeof *s, "%lld", val); break;
	case '_': *l = snprintf(*s, sizeof *s, "%*lld", width, val); break;
	case '0':
	default:  *l = snprintf(*s, sizeof *s, "%0*lld", width, val); break;
	}
	return *s;
nl_strcat:
	fmt = mlibc::nl_langinfo_l(item, loc);
string:
	*l = strlen(fmt);
	return fmt;
nl_strftime:
	fmt = mlibc::nl_langinfo_l(item, loc);
recu_strftime:
	*l = strftime_l(*s, sizeof *s, fmt, tm, loc);
	if (!*l) return 0;
	return *s;
}

inline size_t strftime_l(char *__restrict s, size_t n, const char *__restrict f,
		const struct tm *__restrict tm, localeinfo *loc)
{
	size_t l, k;
	char buf[100];
	char *p;
	const char *t;
	int pad, plus;
	unsigned long width;
	for (l=0; l<n; f++) {
		if (!*f) {
			s[l] = 0;
			return l;
		}
		if (*f != '%') {
			s[l++] = *f;
			continue;
		}
		const char *start = f++;
		pad = 0;
		plus = 0;
		bool upper = false, swap = false;
		for (;; f++) {
			if (*f == '-' || *f == '_' || *f == '0') pad = *f;
			else if (*f == '+') plus = 1;
			else if (*f == '^') upper = true;
			else if (*f == '#') swap = true;
			else break;
		}
		if (*f-'0'<10U) {
			width = strtoul(f, &p, 10);
		} else {
			width = 0;
			p = const_cast<char *>(f);
		}
		if (*p == 'C' || *p == 'F' || *p == 'G' || *p == 'Y') {
			if (!width && p!=f) width = 1;
		} else {
			width = 0;
		}
		f = p;
		if (*f == 'E' || *f == 'O') f++;
		if (!is_conversion(*f)) {
			// glibc's way: the text from the '%' as it stands, and a '%'
			// that ends the format is copied too.
			k = (*f ? f + 1 : f) - start;
			if (k > n-l) k = n-l;
			memcpy(s+l, start, k);
			l += k;
			if (!*f) f--;
			continue;
		}
		t = fmt_1(&buf, &k, *f, tm, loc, pad);
		if (!t) break;
		if (int c = case_of(*f, upper, swap); c) {
			if (t != buf) {
				if (k > sizeof buf) k = sizeof buf;
				memcpy(buf, t, k);
				t = buf;
			}
			for (size_t i = 0; i < k; i++)
				buf[i] = static_cast<char>(c > 0 ? toupper(static_cast<unsigned char>(buf[i]))
						: tolower(static_cast<unsigned char>(buf[i])));
		}
		if (width) {
			/* Trim off any sign and leading zeros, then
			 * count remaining digits to determine behavior
			 * for the + flag. */
			if (*t=='+' || *t=='-') t++, k--;
			for (; *t=='0' && t[1]-'0'<10U; t++, k--);
			if (width < k) width = k;
			size_t d;
			for (d=0; t[d]-'0'<10U; d++);
			if (tm->tm_year < -1900) {
				s[l++] = '-';
				width--;
			} else if (plus && d+(width-k) >= (*p=='C'?3:5)) {
				s[l++] = '+';
				width--;
			}
			for (; width > k && l < n; width--)
				s[l++] = '0';
		}
		if (k > n-l) k = n-l;
		memcpy(s+l, t, k);
		l += k;
	}
	if (n) {
		if (l==n) l=n-1;
		s[l] = 0;
	}
	return 0;
}

inline size_t wcsftime_l(wchar_t *__restrict s, size_t n, const wchar_t *__restrict f,
		const struct tm *__restrict tm, localeinfo *loc)
{
	size_t l, k;
	char buf[100];
	wchar_t wbuf[100];
	wchar_t *p;
	const char *t_mb;
	const wchar_t *t;
	int pad, plus;
	unsigned long width;
	for (l=0; l<n; f++) {
		if (!*f) {
			s[l] = 0;
			return l;
		}
		if (*f != '%') {
			s[l++] = *f;
			continue;
		}
		const wchar_t *start = f++;
		pad = 0;
		plus = 0;
		bool upper = false, swap = false;
		for (;; f++) {
			if (*f == '-' || *f == '_' || *f == '0') pad = *f;
			else if (*f == '+') plus = 1;
			else if (*f == '^') upper = true;
			else if (*f == '#') swap = true;
			else break;
		}
		width = wcstoul(f, &p, 10);
		if (*p == 'C' || *p == 'F' || *p == 'G' || *p == 'Y') {
			if (!width && p!=f) width = 1;
		} else {
			width = 0;
		}
		f = p;
		if (*f == 'E' || *f == 'O') f++;
		if (!is_conversion(*f)) {
			k = (*f ? f + 1 : f) - start;
			if (k > n-l) k = n-l;
			wmemcpy(s+l, start, k);
			l += k;
			if (!*f) f--;
			continue;
		}
		t_mb = fmt_1(&buf, &k, *f, tm, loc, pad);
		if (!t_mb) break;
		k = mbstowcs(wbuf, t_mb, sizeof wbuf / sizeof *wbuf);
		if (k == (size_t)-1) return 0;
		t = wbuf;
		if (int c = case_of(*f, upper, swap); c)
			for (size_t i = 0; i < k; i++)
				wbuf[i] = static_cast<wchar_t>(c > 0 ? towupper(wbuf[i]) : towlower(wbuf[i]));
		if (width) {
			for (; *t=='+' || *t=='-' || (*t=='0'&&t[1]); t++, k--);
			width--;
			if (plus && tm->tm_year >= 10000-1900)
				s[l++] = '+';
			else if (tm->tm_year < -1900)
				s[l++] = '-';
			else
				width++;
			for (; width > k && l < n; width--)
				s[l++] = '0';
		}
		if (k >= n-l) k = n-l;
		wmemcpy(s+l, t, k);
		l += k;
	}
	if (n) {
		if (l==n) l=n-1;
		s[l] = 0;
	}
	return 0;
}

} // namespace mlibc::musl_time

#pragma GCC diagnostic pop

#endif // MLIBC_MUSL_STRFTIME_HPP
