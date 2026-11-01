// SPDX-FileCopyrightText: 2005-2020 Rich Felker, et al.
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: MIT

// The time zone, and the offset from UTC in force at a time: TZ, or else
// /etc/localtime, read as a POSIX TZ string or as a TZif file of the
// IANA time zone database (RFC 8536). musl 1.2.5's src/time/__tz.c
// (https://musl.libc.org, MIT), which localtime, mktime, tzset and
// strftime's %Z use (patch 0051, ADR 0194).
//
// mlibc's own read only a file's version 1 data, which zic's slim files
// (its default since 2020) leave empty, and gave every time after the
// last transition the file's first type, usually local mean time; it
// knew nothing of the TZ string at the end of a file, which says what
// comes after. musl's reads the 64-bit data of version 2 and later
// files and the TZ string after them. Changed from musl's:
//   - a file is read with read(2) into memory of its own, and checked
//     whole before it is used: its tables must fit in it, its
//     transitions' types and its types' names must lie in its tables,
//     and its names must end in it; musl trusts a file it maps, and
//     only reads files of the system's own in set-user-ID programs,
//     which Radix has none of;
//   - a TZ string's rules skip only the '.' between their numbers, so
//     that one cut short ends at its end;
//   - the system's database, /system/resources/zoneinfo, is searched
//     first, then musl's places;
//   - a file whose TZ string is empty takes its names from its types,
//     as one without a TZ string does.

#ifndef MLIBC_MUSL_TIMEZONE_HPP
#define MLIBC_MUSL_TIMEZONE_HPP

#include <abi-bits/fcntl.h>
#include <ctype.h>
#include <limits.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <frg/mutex.hpp>
#include <mlibc/all-sysdeps.hpp>
#include <mlibc/allocator.hpp>
#include <mlibc/calendar.hpp>
#include <mlibc/lock.hpp>

namespace mlibc::musl_tz {

// The name of UTC, which gmtime's tm_zone points to.
inline const char utc[] = "UTC";

// Taken by every function here.
inline FutexLock lock;

inline char std_name[TZNAME_MAX + 1];
inline char dst_name[TZNAME_MAX + 1];

inline int dst_off;
inline int r0[5], r1[5];

// The file, and its transitions, their types' indexes, the types and
// their names.
inline const unsigned char *zi, *trans, *tindex, *types, *abbrevs, *abbrevs_end;
inline size_t map_size;

inline char old_tz_buf[32];
inline char *old_tz = old_tz_buf;
inline size_t old_tz_size = sizeof old_tz_buf;

// The largest time zone file read: the largest of the database's are a
// few kilobytes.
constexpr size_t file_max = 1 << 16;

inline int getint(const char **p)
{
	unsigned x;
	for (x=0; **p-'0'<10U; (*p)++) x = **p-'0' + 10*x;
	return x;
}

inline int getoff(const char **p)
{
	int neg = 0;
	if (**p == '-') {
		++*p;
		neg = 1;
	} else if (**p == '+') {
		++*p;
	}
	int off = 3600*getint(p);
	if (**p == ':') {
		++*p;
		off += 60*getint(p);
		if (**p == ':') {
			++*p;
			off += getint(p);
		}
	}
	return neg ? -off : off;
}

inline void getrule(const char **p, int rule[5])
{
	int r = rule[0] = **p;

	if (r!='M') {
		if (r=='J') ++*p;
		else rule[0] = 0;
		rule[1] = getint(p);
	} else {
		++*p; rule[1] = getint(p);
		if (**p == '.') ++*p;
		rule[2] = getint(p);
		if (**p == '.') ++*p;
		rule[3] = getint(p);
	}

	if (**p=='/') {
		++*p;
		rule[4] = getoff(p);
	} else {
		rule[4] = 7200;
	}
}

inline void getname(char *d, const char **p)
{
	int i;
	if (**p == '<') {
		++*p;
		for (i=0; (*p)[i] && (*p)[i]!='>'; i++)
			if (i<TZNAME_MAX) d[i] = (*p)[i];
		if ((*p)[i]) ++*p;
	} else {
		for (i=0; ((*p)[i]|32)-'a'<26U; i++)
			if (i<TZNAME_MAX) d[i] = (*p)[i];
	}
	*p += i;
	d[i<TZNAME_MAX?i:TZNAME_MAX] = 0;
}

inline uint32_t zi_read32(const unsigned char *z)
{
	return (unsigned)z[0]<<24 | z[1]<<16 | z[2]<<8 | z[3];
}

// The file at `path`, read whole into memory of its own with a 0 after
// it, and its size; nullptr if it cannot be read, or is empty or larger
// than any time zone's.
inline unsigned char *read_file(const char *path, size_t *size)
{
	int fd;
	if (mlibc::sysdep<Open>(path, O_RDONLY | O_CLOEXEC, 0, &fd))
		return nullptr;
	struct stat info;
	unsigned char *data = nullptr;
	size_t len = 0;
	if (!mlibc::sysdep<Stat>(mlibc::fsfd_target::fd, fd, "", 0, &info)
			&& info.st_size > 0 && static_cast<size_t>(info.st_size) <= file_max) {
		len = info.st_size;
		data = static_cast<unsigned char *>(getAllocator().allocate(len + 1));
		size_t done = 0;
		while (data && done < len) {
			ssize_t chunk;
			if (mlibc::sysdep<Read>(fd, data + done, len - done, &chunk) || chunk <= 0)
				break;
			done += chunk;
		}
		if (data && done != len) {
			getAllocator().deallocate(data, len + 1);
			data = nullptr;
		}
	}
	mlibc::sysdep<Close>(fd);
	if (!data)
		return nullptr;
	data[len] = 0;
	*size = len;
	return data;
}

inline void unmap(const unsigned char *map, size_t size)
{
	getAllocator().deallocate(const_cast<unsigned char *>(map), size + 1);
}

// Sets trans, tindex, types, abbrevs and abbrevs_end from the file at
// `map`: its version 2 data if it has some, else its version 1 data.
// Returns where that data ends, or 0 if the file does not hold the
// tables its header counts or their indexes lead outside them.
inline size_t read_tables(const unsigned char *map, size_t size)
{
	// A header (44 bytes): "TZif", the version, 15 reserved bytes, and
	// six counts: UT/local and standard/wall indicators, leap seconds,
	// transitions, types and bytes of names. Returns where its data
	// ends, and with `use` set, takes its tables.
	auto block = [&](size_t at, size_t time_size, bool use) -> size_t {
		if (size < at + 44 || memcmp(map + at, "TZif", 4))
			return 0;
		size_t isut = zi_read32(map + at + 20), isstd = zi_read32(map + at + 24);
		size_t leap = zi_read32(map + at + 28), time = zi_read32(map + at + 32);
		size_t type = zi_read32(map + at + 36), chars = zi_read32(map + at + 40);
		// Each count is at most the file's size, so none of the sums
		// below overflows.
		if (isut > size || isstd > size || leap > size || time > size || type > size
				|| chars > size)
			return 0;
		size_t data = time*time_size + time + type*6 + chars + leap*(time_size+4) + isstd + isut;
		if (size - (at + 44) < data)
			return 0;
		if (!use)
			return at + 44 + data;
		if (!type || !chars)
			return 0;
		trans = map + at + 44;
		tindex = trans + time*time_size;
		types = tindex + time;
		abbrevs = types + 6*type;
		abbrevs_end = abbrevs + chars;
		for (size_t i = 0; i < time; i++)
			if (tindex[i] >= type) return 0;
		for (size_t i = 0; i < type; i++)
			if (types[6*i+5] >= chars) return 0;
		// Every name ends in the table.
		if (abbrevs_end[-1])
			return 0;
		return at + 44 + data;
	};
	// A version 2 or later file's version 1 data is only stepped over.
	bool v1_only = map[4] == 0 || map[4] == '1';
	size_t v1 = block(0, 4, v1_only);
	if (!v1 || v1_only)
		return v1;
	return block(v1, 8, true);
}

inline void do_tzset()
{
	char buf[NAME_MAX+33], *pathname=buf+32;
	const char *try_, *s, *p;
	unsigned char *map = 0;
	size_t mapped = 0;
	size_t i;
	static const char search[] =
		"/system/resources/zoneinfo/\0/usr/share/zoneinfo/\0/share/zoneinfo/\0/etc/zoneinfo/\0";

	s = getenv("TZ");
	if (!s) s = "/etc/localtime";
	if (!*s) s = utc;

	if (old_tz && !strcmp(s, old_tz)) return;

	for (i=0; i<5; i++) r0[i] = r1[i] = 0;

	if (zi) unmap(zi, map_size);
	zi = 0;

	/* Cache the old value of TZ to check if it has changed. Avoid
	 * free so as not to pull it into static programs. Growth
	 * strategy makes it so free would have minimal benefit anyway. */
	i = strlen(s);
	if (i > PATH_MAX+1) s = utc, i = 3;
	if (i >= old_tz_size) {
		old_tz_size *= 2;
		if (i >= old_tz_size) old_tz_size = i+1;
		if (old_tz_size > PATH_MAX+2) old_tz_size = PATH_MAX+2;
		old_tz = static_cast<char *>(getAllocator().allocate(old_tz_size));
	}
	if (old_tz) memcpy(old_tz, s, i+1);

	int posix_form = 0;
	if (*s != ':') {
		p = s;
		char dummy_name[TZNAME_MAX+1];
		getname(dummy_name, &p);
		if (p!=s && (*p == '+' || *p == '-' || isdigit(*p)
		             || !strcmp(dummy_name, "UTC")
		             || !strcmp(dummy_name, "GMT")))
			posix_form = 1;
	}

	/* An absolute tzfile pathname or a relative pathame beginning
	 * with ".", or a name in the database. */
	if (!posix_form) {
		if (*s == ':') s++;
		if (*s == '/' || *s == '.') {
			map = read_file(s, &mapped);
		} else {
			size_t l = strlen(s);
			if (l <= NAME_MAX && !strchr(s, '.')) {
				memcpy(pathname, s, l+1);
				pathname[l] = 0;
				for (try_=search; !map && *try_; try_+=l+1) {
					l = strlen(try_);
					memcpy(pathname-l, try_, l);
					map = read_file(pathname-l, &mapped);
				}
			}
		}
		if (!map) s = utc;
	}
	size_t end = 0;
	if (map && !(end = read_tables(map, mapped))) {
		unmap(map, mapped);
		map = 0;
		s = utc;
	}

	zi = map;
	map_size = mapped;
	if (map) {
		// After version 2 and later data, a TZ string between newlines
		// says what comes after the last transition.
		if (map[4] != 0 && map[4] != '1' && end + 2 < mapped && map[end] == '\n'
				&& map[mapped-1] == '\n' && map[end+1] != '\n') {
			s = (const char *)map + end + 1;
		} else {
			const unsigned char *p;
			tzname[0] = tzname[1] = 0;
			daylight = timezone = dst_off = 0;
			for (p=types; p<abbrevs; p+=6) {
				if (!p[4] && !tzname[0]) {
					tzname[0] = (char *)abbrevs + p[5];
					timezone = -(int32_t)zi_read32(p);
				}
				if (p[4] && !tzname[1]) {
					tzname[1] = (char *)abbrevs + p[5];
					dst_off = -(int32_t)zi_read32(p);
					daylight = 1;
				}
			}
			if (!tzname[0]) tzname[0] = tzname[1];
			if (!tzname[0]) tzname[0] = (char *)utc;
			if (!daylight) {
				tzname[1] = tzname[0];
				dst_off = timezone;
			}
			return;
		}
	}

	if (!s) s = utc;
	getname(std_name, &s);
	tzname[0] = std_name;
	timezone = getoff(&s);
	getname(dst_name, &s);
	tzname[1] = dst_name;
	if (dst_name[0]) {
		daylight = 1;
		if (*s == '+' || *s=='-' || *s-'0'<10U)
			dst_off = getoff(&s);
		else
			dst_off = timezone - 3600;
	} else {
		daylight = 0;
		dst_off = timezone;
	}

	if (*s == ',') s++, getrule(&s, r0);
	if (*s == ',') s++, getrule(&s, r1);
}

/* Search zoneinfo rules to find the one that applies to the given time,
 * and determine alternate opposite-DST-status rule that may be needed. */

inline size_t scan_trans(long long t, int local, size_t *alt)
{
	int scale = 3 - (trans == zi+44);
	uint64_t x;
	int off = 0;

	size_t a = 0, n = (tindex-trans)>>scale, m;

	if (!n) {
		if (alt) *alt = 0;
		return 0;
	}

	/* Binary search for 'most-recent rule before t'. */
	while (n > 1) {
		m = a + n/2;
		x = zi_read32(trans + (m<<scale));
		if (scale == 3) x = x<<32 | zi_read32(trans + (m<<scale) + 4);
		else x = (int32_t)x;
		if (local) off = (int32_t)zi_read32(types + 6 * tindex[m-1]);
		if (t - off < (int64_t)x) {
			n /= 2;
		} else {
			a = m;
			n -= n/2;
		}
	}

	/* First and last entry are special. First means to use lowest-index
	 * non-DST type. Last means to apply POSIX-style rule if available. */
	n = (tindex-trans)>>scale;
	if (a == n-1) return -1;
	if (a == 0) {
		x = zi_read32(trans);
		if (scale == 3) x = x<<32 | zi_read32(trans + 4);
		else x = (int32_t)x;
		/* Find the lowest non-DST type, or 0 if none. */
		size_t j = 0;
		for (size_t i=abbrevs-types; i; i-=6) {
			if (!types[i-6+4]) j = i-6;
		}
		if (local) off = (int32_t)zi_read32(types + j);
		/* If t is before first transition, use the above-found type
		 * and the index-zero (after transition) type as the alt. */
		if (t - off < (int64_t)x) {
			if (alt) *alt = tindex[0];
			return j/6;
		}
	}

	/* Try to find a neighboring opposite-DST-status rule. */
	if (alt) {
		if (a && types[6*tindex[a-1]+4] != types[6*tindex[a]+4])
			*alt = tindex[a-1];
		else if (a+1<n && types[6*tindex[a+1]+4] != types[6*tindex[a]+4])
			*alt = tindex[a+1];
		else
			*alt = tindex[a];
	}

	return tindex[a];
}

inline int days_in_month(int m, int is_leap)
{
	if (m==2) return 28+is_leap;
	else return 30+((0xad5>>(m-1))&1);
}

/* Convert a POSIX DST rule plus year to seconds since epoch. */

inline long long rule_to_secs(const int *rule, int year)
{
	int is_leap;
	long long t = mlibc::musl_time::year_to_secs(year, &is_leap);
	int x, m, n, d;
	if (rule[0]!='M') {
		x = rule[1];
		if (rule[0]=='J' && (x < 60 || !is_leap)) x--;
		t += 86400 * x;
	} else {
		m = rule[1];
		n = rule[2];
		d = rule[3];
		t += mlibc::musl_time::month_to_secs(m-1, is_leap);
		int wday = (int)((t + 4*86400) % (7*86400)) / 86400;
		int days = d - wday;
		if (days < 0) days += 7;
		if (n == 5 && days+28 >= days_in_month(m, is_leap)) n = 4;
		t += 86400 * (days + 7*(n-1));
	}
	t += rule[4];
	return t;
}

/* Determine the time zone in effect for a given time in seconds since the
 * epoch. It can be given in local or universal time. The results will
 * indicate whether DST is in effect at the queried time, and will give both
 * the GMT offset for the active zone/DST rule and the opposite DST. This
 * enables a caller to efficiently adjust for the case where an explicit
 * DST specification mismatches what would be in effect at the time. */

inline void secs_to_zone(long long t, int local, int *isdst, long *offset, long *oppoff, const char **zonename)
{
	frg::unique_lock<FutexLock> guard(lock);

	do_tzset();

	if (zi) {
		size_t alt, i = scan_trans(t, local, &alt);
		if (i != (size_t)-1) {
			*isdst = types[6*i+4];
			*offset = (int32_t)zi_read32(types+6*i);
			*zonename = (const char *)abbrevs + types[6*i+5];
			if (oppoff) *oppoff = (int32_t)zi_read32(types+6*alt);
			return;
		}
	}

	if (!daylight) goto std;

	{
		/* FIXME: may be broken if DST changes right at year boundary?
		 * Also, this could be more efficient.*/
		long long y = t / 31556952 + 70;
		while (mlibc::musl_time::year_to_secs(y, 0) > t) y--;
		while (mlibc::musl_time::year_to_secs(y+1, 0) < t) y++;

		long long t0 = rule_to_secs(r0, y);
		long long t1 = rule_to_secs(r1, y);

		if (!local) {
			t0 += timezone;
			t1 += dst_off;
		}
		if (t0 < t1) {
			if (t >= t0 && t < t1) goto dst;
			goto std;
		} else {
			if (t >= t1 && t < t0) goto std;
			goto dst;
		}
	}
std:
	*isdst = 0;
	*offset = -timezone;
	if (oppoff) *oppoff = -dst_off;
	*zonename = tzname[0];
	return;
dst:
	*isdst = 1;
	*offset = -dst_off;
	if (oppoff) *oppoff = -timezone;
	*zonename = tzname[1];
}

inline void tzset()
{
	frg::unique_lock<FutexLock> guard(lock);
	do_tzset();
}

// The name of `tm`'s zone if it is still one of the zone's names, or "":
// a tm made under another TZ points into a file no longer read.
inline const char *tm_to_tzname(const struct tm *tm)
{
	const void *p = tm->tm_zone;
	frg::unique_lock<FutexLock> guard(lock);
	do_tzset();
	if (p != utc && p != tzname[0] && p != tzname[1] &&
	    (!zi || (uintptr_t)p-(uintptr_t)abbrevs >= (uintptr_t)(abbrevs_end - abbrevs)))
		p = "";
	return static_cast<const char *>(p);
}

} // namespace mlibc::musl_tz

#endif // MLIBC_MUSL_TIMEZONE_HPP
