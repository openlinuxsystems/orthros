// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Time zones and English's locales (ADR 0194): localtime, mktime and
// strftime's %Z and %z in zones of the IANA time zone database the
// system ships, against what Python's zoneinfo answers on Linux, with
// daylight saving time before 1970, now, and past 2037, where only each
// zone file's TZ rule says; tzset's globals; TZ rules of a program's
// own, and names that are no zone; /etc/localtime; and the dates,
// numbers and sorting order of English's locales.

#include <langinfo.h>
#include <locale.h>
#include <time.h>

#include "check.h"

#define LENGTH(array) (sizeof(array) / sizeof((array)[0]))

static void differs(const char *what, const char *got, const char *expected) {
	check_failed(__FILE__, __LINE__, what, 0);
	printf("%s:   got \"%s\", expected \"%s\"\n", check_name, got, expected);
}

// A time in a zone, and what localtime makes of it, as strftime's
// "%F %T %Z %z", and whether it is daylight saving time.
struct zone_case {
	const char *zone;
	long long t;
	const char *local;
	int dst;
};

static const struct zone_case zone_cases[] = {
		{"Europe/Berlin", 1909396800LL, "2030-07-04 14:00:00 CEST +0200", 1},
		{"Europe/Berlin", 1894708800LL, "2030-01-15 13:00:00 CET +0100", 0},
		{"America/New_York", 1909396800LL, "2030-07-04 08:00:00 EDT -0400", 1},
		{"America/New_York", 15681600LL, "1970-07-01 08:00:00 EDT -0400", 1},
		// Past 2037, from the files' TZ rules alone.
		{"Europe/Berlin", 2382782400LL, "2045-07-04 14:00:00 CEST +0200", 1},
		{"America/New_York", 2397816000LL, "2045-12-25 07:00:00 EST -0500", 0},
		// Half an hour off, and a daylight saving time of half an hour.
		{"Asia/Kolkata", 1909396800LL, "2030-07-04 17:30:00 IST +0530", 0},
		{"Australia/Lord_Howe", 1894708800LL, "2030-01-15 23:00:00 +11 +1100", 1},
		{"Australia/Lord_Howe", 1909396800LL, "2030-07-04 22:30:00 +1030 +1030", 0},
		// Before 1970: British Standard Time, the summer's offset all year.
		{"Europe/London", -36763200LL, "1968-11-01 13:00:00 BST +0100", 0},
		{"America/Sao_Paulo", 1894708800LL, "2030-01-15 09:00:00 -03 -0300", 0},
		{"UTC", 1909396800LL, "2030-07-04 12:00:00 UTC +0000", 0},
		// TZ's other forms: a leading ':', a path, a POSIX rule.
		{":Europe/Berlin", 1909396800LL, "2030-07-04 14:00:00 CEST +0200", 1},
		{"/system/resources/zoneinfo/Asia/Kolkata", 1909396800LL, "2030-07-04 17:30:00 IST +0530",
				0},
		{"CET-1CEST,M3.5.0,M10.5.0/3", 1909396800LL, "2030-07-04 14:00:00 CEST +0200", 1},
		{"<+0330>-3:30", 1909396800LL, "2030-07-04 15:30:00 +0330 +0330", 0},
		// A name that is no zone, and one that leaves the database: UTC.
		{"Nowhere/Atlantis", 1909396800LL, "2030-07-04 12:00:00 UTC +0000", 0},
		{"../../settings/passwd", 1909396800LL, "2030-07-04 12:00:00 UTC +0000", 0},
};

// What localtime makes of `t` in the zone tzset took last.
static void local_text(time_t t, char *text, size_t size, int *dst) {
	struct tm tm;
	CHECK(localtime_r(&t, &tm) == &tm);
	CHECK(strftime(text, size, "%F %T %Z %z", &tm) > 0);
	*dst = tm.tm_isdst;
	// mktime undoes it.
	struct tm copy = tm;
	copy.tm_isdst = -1;
	time_t back = mktime(&copy);
	if(back != t) {
		char got[32], expected[32];
		snprintf(got, sizeof(got), "%lld", (long long)back);
		snprintf(expected, sizeof(expected), "%lld", (long long)t);
		differs("mktime of localtime", got, expected);
	}
}

static void zones(void) {
	for(size_t i = 0; i < LENGTH(zone_cases); i++) {
		const struct zone_case *c = &zone_cases[i];
		CHECK(setenv("TZ", c->zone, 1) == 0);
		tzset();
		char text[64];
		int dst;
		local_text((time_t)c->t, text, sizeof(text), &dst);
		if(strcmp(text, c->local))
			differs(c->zone, text, c->local);
		if(!dst != !c->dst)
			differs(c->zone, dst ? "daylight saving time" : "standard time",
					c->dst ? "daylight saving time" : "standard time");
	}
}

// tzset's globals: the zone's names, and its standard offset west of
// UTC, in seconds.
static void globals(void) {
	CHECK(setenv("TZ", "Europe/Berlin", 1) == 0);
	tzset();
	CHECK(!strcmp(tzname[0], "CET") && !strcmp(tzname[1], "CEST"));
	CHECK(timezone == -3600 && daylight == 1);
	CHECK(setenv("TZ", "America/New_York", 1) == 0);
	tzset();
	CHECK(!strcmp(tzname[0], "EST") && !strcmp(tzname[1], "EDT"));
	CHECK(timezone == 18000 && daylight == 1);
	CHECK(setenv("TZ", "Asia/Kolkata", 1) == 0);
	tzset();
	CHECK(!strcmp(tzname[0], "IST") && timezone == -19800 && daylight == 0);
}

// Without TZ, the zone is /etc/localtime's, a link into the database:
// UTC unless timedatectl(1) set another.
static void localtime_file(void) {
	CHECK(unsetenv("TZ") == 0);
	tzset();
	char target[128] = {0};
	CHECK(readlink("/etc/localtime", target, sizeof(target) - 1) > 0);
	CHECK(!strncmp(target, "/system/resources/zoneinfo/", 27));
	const char *zone = target + 27;
	char text[64];
	int dst;
	local_text((time_t)1909396800LL, text, sizeof(text), &dst);
	if(!strcmp(zone, "UTC"))
		CHECK(!strcmp(text, "2030-07-04 12:00:00 UTC +0000"));
	else
		printf("zones: /etc/localtime is %s: %s\n", zone, text);
	// The database names every zone of zone1970.tab.
	CHECK(access("/system/resources/zoneinfo/zone1970.tab", R_OK) == 0);
	CHECK(access("/system/resources/zoneinfo/Pacific/Auckland", R_OK) == 0);
	// A link of the database's is one of its files: US/Eastern is
	// America/New_York.
	CHECK(readlink("/system/resources/zoneinfo/US/Eastern", target, sizeof(target)) > 0);
}

// What strftime and nl_langinfo say in `locale` for 2030-07-04 08:09:10:
// its date format, the date, and the names of the day, month and morning.
static void dates_in(const char *locale, const char *d_fmt, const char *x, const char *names) {
	CHECK(setlocale(LC_ALL, locale) != NULL);
	CHECK(!strcmp(setlocale(LC_ALL, NULL), locale));
	CHECK(!strcmp(nl_langinfo(CODESET), "UTF-8") && MB_CUR_MAX >= 4);
	if(strcmp(nl_langinfo(D_FMT), d_fmt))
		differs(locale, nl_langinfo(D_FMT), d_fmt);
	struct tm tm = {.tm_year = 130, .tm_mon = 6, .tm_mday = 4, .tm_hour = 8, .tm_min = 9,
			.tm_sec = 10, .tm_wday = 4, .tm_yday = 184};
	char text[64];
	CHECK(strftime(text, sizeof(text), "%x", &tm) > 0);
	if(strcmp(text, x))
		differs(locale, text, x);
	CHECK(strftime(text, sizeof(text), "%A %B %p", &tm) > 0);
	if(strcmp(text, names))
		differs(locale, text, names);
}

static void locales(void) {
	dates_in("en_US.UTF-8", "%m/%d/%Y", "07/04/2030", "Thursday July AM");
	// Numbers and money as Americans write them.
	struct lconv *conv = localeconv();
	CHECK(!strcmp(conv->decimal_point, ".") && !strcmp(conv->thousands_sep, ","));
	CHECK(!strcmp(conv->currency_symbol, "$"));
	// Letters before their case: "apple" before "Banana", which the C
	// locale's bytes put first.
	CHECK(strcoll("apple", "Banana") < 0);
	CHECK(strcoll("Banana", "cherry") < 0);
	dates_in("en_GB.UTF-8", "%d/%m/%y", "04/07/30", "Thursday July am");
	conv = localeconv();
	CHECK(!strcmp(conv->currency_symbol, "\xc2\xa3"));
	CHECK(setlocale(LC_ALL, "en_IN.UTF-8") != NULL && setlocale(LC_ALL, "en_AU.UTF-8") != NULL);
	// The codeset may be written as each C library writes it.
	CHECK(setlocale(LC_ALL, "en_US.utf8") != NULL);
	// No language but English's ships before 1.0.
	CHECK(setlocale(LC_ALL, "de_DE.UTF-8") == NULL);
	// LANG names a locale for every category.
	CHECK(setenv("LANG", "en_GB.UTF-8", 1) == 0);
	CHECK(setlocale(LC_ALL, "") && !strcmp(setlocale(LC_TIME, NULL), "en_GB.UTF-8"));
	CHECK(setlocale(LC_ALL, "C") && strcoll("apple", "Banana") > 0);
}

int main(void) {
	check_start("zones");
	RUN(zones);
	RUN(globals);
	RUN(localtime_file);
	RUN(locales);
	return check_end();
}
