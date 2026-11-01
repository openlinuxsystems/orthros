// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Numbers and times as text, against what glibc answers
// (format-cases.h): printf's integer and floating-point conversions,
// strtod, strtof, strtold and scanf, strftime, and the broken-down
// times localtime, gmtime, mktime and timegm make; and characters in the
// C.UTF-8 locale, the one programs start in.

#include <float.h>
#include <limits.h>
#include <locale.h>
#include <math.h>
#include <stdint.h>
#include <wchar.h>
#include <wctype.h>

#include "check.h"

struct int_case {
	const char *format;
	// 'i': passed as an int, 'l' as a long long.
	char kind;
	long long value;
	const char *expected;
};

struct float_case {
	const char *format;
	double value;
	const char *expected;
};

struct long_double_case {
	const char *format;
	long double value;
	const char *expected;
};

// A text, and what strtod, strtof and strtold make of it: the bits of
// the number, how many characters they took and whether they set
// ERANGE. Whether a result below the normal numbers sets ERANGE is the C
// library's choice (C11 7.22.1.3), and glibc and musl choose
// differently, so those are not compared.
struct scan_case {
	const char *text;
	uint64_t double_bits;
	long double_used;
	int double_range;
	uint32_t float_bits;
	long float_used;
	int float_range;
	uint64_t long_double_mantissa;
	uint16_t long_double_exponent;
	long long_double_used;
	int long_double_range;
};

// What strftime makes of `format` for gmtime's broken-down `time`.
struct time_case {
	long long time;
	const char *format;
	const char *expected;
};

#include "format-cases.h"

#define LENGTH(array) (sizeof(array) / sizeof((array)[0]))

// Reports a case that came out other than expected.
static void differs(const char *what, const char *format, const char *got, const char *expected) {
	check_failed(__FILE__, __LINE__, what, 0);
	printf("%s:   %s: got \"%s\", expected \"%s\"\n", check_name, format, got, expected);
}

static void integers(void) {
	char text[512];
	for(size_t i = 0; i < LENGTH(int_cases); i++) {
		const struct int_case *c = &int_cases[i];
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wformat-nonliteral"
		if(c->kind == 'i')
			snprintf(text, sizeof(text), c->format, (int)c->value);
		else
			snprintf(text, sizeof(text), c->format, c->value);
#pragma clang diagnostic pop
		if(strcmp(text, c->expected))
			differs("printf of an integer", c->format, text, c->expected);
	}
}

static void floats(void) {
	char text[512];
	for(size_t i = 0; i < LENGTH(float_cases); i++) {
		const struct float_case *c = &float_cases[i];
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wformat-nonliteral"
		snprintf(text, sizeof(text), c->format, c->value);
#pragma clang diagnostic pop
		if(strcmp(text, c->expected))
			differs("printf of a double", c->format, text, c->expected);
	}
	for(size_t i = 0; i < LENGTH(long_double_cases); i++) {
		const struct long_double_case *c = &long_double_cases[i];
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wformat-nonliteral"
		snprintf(text, sizeof(text), c->format, c->value);
#pragma clang diagnostic pop
		if(strcmp(text, c->expected))
			differs("printf of a long double", c->format, text, c->expected);
	}
	// The digits are exact however many are asked for: 2^-1074 has 751
	// significant ones.
	static char tiny[1200];
	CHECK(snprintf(tiny, sizeof(tiny), "%.1100f", 0x1p-1074) == 1102);
	CHECK(!strncmp(tiny + 2 + 323, "4940656458412465441765687928682213723650", 40));
	CHECK(!strncmp(tiny + 2 + 1071, "625", 3) && strspn(tiny + 2 + 1074, "0") == 26);
	// printf counts what it would write; %n stores it.
	int written = -1;
	CHECK(snprintf(NULL, 0, "%.3f%n", 2.0, &written) == 5 && written == 5);
	// Wide output takes the same path.
	wchar_t wide[64];
	CHECK(swprintf(wide, LENGTH(wide), L"%.2f|%5.1e|%g", 3.14159, 12345.0, 1e-5) > 0);
	CHECK(!wcscmp(wide, L"3.14|1.2e+04|1e-05"));
}

static void scanning(void) {
	for(size_t i = 0; i < LENGTH(scan_cases); i++) {
		const struct scan_case *c = &scan_cases[i];
		char *end;
		errno = 0;
		double d = strtod(c->text, &end);
		int range = errno == ERANGE;
		uint64_t d_bits;
		memcpy(&d_bits, &d, sizeof(d));
		if(isnan(d))
			d_bits = 0x7ff8000000000000ULL;
		if(fabs(d) <= DBL_MIN)
			range = c->double_range;
		if(d_bits != c->double_bits || end - c->text != c->double_used
				|| range != c->double_range) {
			check_failed(__FILE__, __LINE__, "strtod", errno);
			printf("%s:   \"%s\": got %a (%#llx), %ld used, ERANGE %d\n", check_name, c->text, d,
					(unsigned long long)d_bits, (long)(end - c->text), range);
		}

		errno = 0;
		float f = strtof(c->text, &end);
		range = errno == ERANGE;
		uint32_t f_bits;
		memcpy(&f_bits, &f, sizeof(f));
		if(isnan(f))
			f_bits = 0x7fc00000U;
		if(fabsf(f) <= FLT_MIN)
			range = c->float_range;
		if(f_bits != c->float_bits || end - c->text != c->float_used
				|| range != c->float_range) {
			check_failed(__FILE__, __LINE__, "strtof", errno);
			printf("%s:   \"%s\": got %a (%#x), %ld used, ERANGE %d\n", check_name, c->text,
					(double)f, f_bits, (long)(end - c->text), range);
		}

		errno = 0;
		long double l = strtold(c->text, &end);
		range = errno == ERANGE;
		uint64_t mantissa;
		uint16_t exponent;
		memcpy(&mantissa, &l, sizeof(mantissa));
		memcpy(&exponent, (char *)&l + 8, sizeof(exponent));
		if(isnan(l))
			mantissa = 0xc000000000000000ULL, exponent = 0x7fff;
		if(fabsl(l) <= LDBL_MIN)
			range = c->long_double_range;
		if(mantissa != c->long_double_mantissa || exponent != c->long_double_exponent
				|| end - c->text != c->long_double_used || range != c->long_double_range) {
			check_failed(__FILE__, __LINE__, "strtold", errno);
			printf("%s:   \"%s\": got %La, %ld used, ERANGE %d\n", check_name, c->text, l,
					(long)(end - c->text), range);
		}

		// Wide text, where it is all ASCII, gives the same.
		wchar_t wide[128];
		size_t length = strlen(c->text);
		if(length < LENGTH(wide)) {
			for(size_t j = 0; j <= length; j++)
				wide[j] = (unsigned char)c->text[j];
			wchar_t *wend;
			double w = wcstod(wide, &wend);
			uint64_t w_bits;
			memcpy(&w_bits, &w, sizeof(w));
			if(isnan(w))
				w_bits = 0x7ff8000000000000ULL;
			if(w_bits != c->double_bits || wend - wide != c->double_used) {
				check_failed(__FILE__, __LINE__, "wcstod", 0);
				printf("%s:   \"%s\": got %a, %ld used\n", check_name, c->text, w,
						(long)(wend - wide));
			}
		}
	}
	// scanf takes the longest prefix that could start a number: "1e+x"
	// is a matching failure that used "1e+".
	double d = 0;
	float f = 0;
	long double l = 0;
	int used = 0;
	CHECK(sscanf("0.1 2.5e3 1e400", "%lf %f %Lf%n", &d, &f, &l, &used) == 3);
	CHECK(d == 0.1 && f == 2500.0f && l == 1e400L && used == 15);
	CHECK(sscanf("12345.678", "%4lf%n", &d, &used) == 1 && d == 1234.0 && used == 4);
	CHECK(sscanf("0x1.8p1 -inf nan", "%lf %lf %lf", &d, &d, &d) == 3);
	char rest[8] = {0};
	CHECK(sscanf("1e+x", "%lf%7s", &d, rest) == 0);
	CHECK(sscanf("-.5;", "%lf%c", &d, rest) == 2 && d == -0.5 && rest[0] == ';');
	CHECK(sscanf("", "%lf", &d) == EOF);
	CHECK(sscanf("abc", "%lf", &d) == 0);
}

static void dates(void) {
	char text[256];
	for(size_t i = 0; i < LENGTH(time_cases); i++) {
		const struct time_case *c = &time_cases[i];
		time_t t = c->time;
		struct tm tm;
		CHECK(gmtime_r(&t, &tm) == &tm);
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wformat-nonliteral"
		size_t n = strftime(text, sizeof(text), c->format, &tm);
#pragma clang diagnostic pop
		if(n != strlen(c->expected) || strcmp(text, c->expected))
			differs("strftime", c->format, text, c->expected);
	}
	// Too small a buffer: 0, as C says.
	time_t zero = 0;
	struct tm tm;
	CHECK(gmtime_r(&zero, &tm) && strftime(text, 4, "%Y", &tm) == 0);
	CHECK(strftime(text, 5, "%Y", &tm) == 4 && !strcmp(text, "1970"));
	// Wide, the same.
	wchar_t wide[64];
	CHECK(wcsftime(wide, LENGTH(wide), L"%F %T %j %V", &tm) == 26);
	CHECK(!wcscmp(wide, L"1970-01-01 00:00:00 001 01"));
}

// Broken-down times and back, as Lua's os.date and os.time use them: the
// fields strftime prints are the struct's, and mktime and timegm undo
// localtime and gmtime.
static void broken_down(void) {
	const long long instants[] = {0, 1, 1000, 86399, 86400, 951782400, 1230768000, 0x7fffffffLL,
			0x80000000LL, 4102444800LL, -1, -2208988800LL, (long long)time(NULL)};
	for(size_t i = 0; i < LENGTH(instants); i++) {
		time_t t = instants[i];
		struct tm local, utc;
		CHECK(localtime_r(&t, &local) == &local);
		CHECK(gmtime_r(&t, &utc) == &utc);
		long long day = (t >= 0 ? t : t - 86399) / 86400;
		int wday = (int)(((day + 4) % 7 + 7) % 7);
		CHECK(utc.tm_wday == wday);
		CHECK(utc.tm_hour == (int)(((t % 86400) + 86400) % 86400 / 3600));
		struct tm copy = local;
		CHECK(mktime(&copy) == t);
		copy = utc;
		CHECK(timegm(&copy) == t);
		char fields[128];
		snprintf(fields, sizeof(fields), "%d %d %d %d %d %d %d %d", local.tm_year + 1900,
				local.tm_mon + 1, local.tm_mday, local.tm_hour, local.tm_min, local.tm_sec,
				local.tm_wday, local.tm_yday + 1);
		char printed[128];
		CHECK(strftime(printed, sizeof(printed), "%Y %-m %-d %-H %-M %-S %w %-j", &local) > 0);
		if(strcmp(fields, printed))
			differs("strftime of localtime's fields", "%Y %-m %-d %-H %-M %-S %w %-j", printed,
					fields);
	}
	// mktime puts out-of-range fields right and fills in the day.
	struct tm tm = {.tm_year = 124, .tm_mon = 0, .tm_mday = 32, .tm_isdst = 0};
	CHECK(timegm(&tm) == 1706745600 && tm.tm_mon == 1 && tm.tm_mday == 1 && tm.tm_wday == 4
			&& tm.tm_yday == 31);
}

// UTF-8 in the C.UTF-8 locale, which LANG names for every program, with
// Unicode's character classes; the C locale's characters are bytes.
static void characters(void) {
	const char *name = setlocale(LC_ALL, "");
	CHECK(name && !strcmp(name, "C.UTF-8"));
	CHECK(MB_CUR_MAX == 4);
	mbstate_t state = {0};
	wchar_t wide;
	CHECK(mbrtowc(&wide, "\xc3\xa9!", 3, &state) == 2 && wide == 0xe9);
	CHECK(mbrtowc(&wide, "\xf0\x9f\x8d\x8c", 4, &state) == 4 && wide == 0x1f34c);
	errno = 0;
	CHECK(mbrtowc(&wide, "\xc3(", 2, &state) == (size_t)-1 && errno == EILSEQ);
	memset(&state, 0, sizeof(state));
	char bytes[8];
	CHECK(wcrtomb(bytes, 0x6c34, &state) == 3 && !memcmp(bytes, "\xe6\xb0\xb4", 3));
	CHECK(mbstowcs(NULL, "z\xc3\x9f\xe6\xb0\xb4\xf0\x9f\x8d\x8c", 0) == 4);
	CHECK(iswalpha(0xe9) && iswupper(0xc9) && towupper(0xe9) == 0xc9);
	CHECK(iswalpha(0x4e00) && !iswalpha(0x2603) && iswspace(0x3000));
	CHECK(towlower(0x0394) == 0x03b4 && towupper(0x0434) == 0x0414);
	// printf's %lc and %ls make UTF-8.
	char text[32];
	CHECK(snprintf(text, sizeof(text), "%lc|%ls", (wint_t)0x3c0, L"\u00e9t\u00e9") == 8);
	CHECK(!strcmp(text, "\xcf\x80|\xc3\xa9t\xc3\xa9"));
	// English's locales are there (the zones test checks them), others
	// not yet; C is still there.
	CHECK(setlocale(LC_ALL, "en_US.UTF-8") != NULL);
	CHECK(setlocale(LC_ALL, "fr_FR.UTF-8") == NULL);
	CHECK(setlocale(LC_ALL, "C") && !strcmp(setlocale(LC_ALL, NULL), "C"));
	CHECK(MB_CUR_MAX == 1);
	CHECK(setlocale(LC_ALL, "C.utf8") != NULL && MB_CUR_MAX == 4);
	setlocale(LC_ALL, "C");
}

int main(void) {
	check_start("format");
	RUN(integers);
	RUN(floats);
	RUN(scanning);
	RUN(dates);
	RUN(broken_down);
	RUN(characters);
	return check_end();
}
