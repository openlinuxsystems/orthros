// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Prints the expected-results tables for userspace/posix/format.c from
// the host's glibc (C locale, TZ=UTC).
#define _GNU_SOURCE
#include <float.h>
#include <limits.h>
#include <locale.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <errno.h>
#include <gnu/libc-version.h>

static void cstr(const char *s) {
	putchar('"');
	int n = 0;
	for (; *s; s++, n++) {
		if (n && n % 90 == 0) printf("\"\n\t\t\"");
		if (*s == '"' || *s == '\\') printf("\\%c", *s);
		else if (*s == '\n') printf("\\n");
		else if (*s == '\t') printf("\\t");
		else putchar(*s);
	}
	putchar('"');
}

int main(void) {
	setlocale(LC_ALL, "C");
	setenv("TZ", "UTC", 1);
	tzset();
	char buf[1024];
	// Split so that REUSE does not take them for this file's own.
	printf("// SPDX-" "FileCopyrightText: 2026 The Dukovo contributors\n");
	printf("// SPDX-" "License-Identifier: Apache-2.0 OR MIT\n\n");
	printf("// What glibc %s answers, for format.c: written by\n", gnu_get_libc_version());
	printf("// expected/format-cases.c (cc -o fc expected/format-cases.c -lm &&\n");
	printf("// ./fc > format-cases.h, on a glibc system). Do not edit.\n\n");

	// Integers.
	struct { const char *f; char kind; } ifs[] = {
		{"%d",'i'},{"%5d",'i'},{"%-5d|",'i'},{"%05d",'i'},{"%+d",'i'},{"% d",'i'},{"%.3d",'i'},
		{"%.0d",'i'},{"%+.0d",'i'},{"% .0d",'i'},{"%-+6.3d|",'i'},{"%i",'i'},{"%hhd",'i'},{"%hd",'i'},
		{"%ld",'l'},{"%lld",'l'},{"%jd",'l'},{"%zd",'l'},{"%td",'l'},
		{"%u",'i'},{"%o",'i'},{"%#o",'i'},{"%#.0o",'i'},{"%#5o",'i'},{"%x",'i'},{"%X",'i'},{"%#x",'i'},
		{"%#X",'i'},{"%#10x",'i'},{"%-#10x|",'i'},{"%#010x",'i'},{"%.0x",'i'},{"%hhu",'i'},{"%hx",'i'},
		{"%lu",'l'},{"%llx",'l'},{"%#llo",'l'},{"%zu",'l'},{"%+u",'i'},{"% x",'i'},{"%#d",'i'},{"%'d",'i'},
	};
	long long ivs[] = {0, 1, -1, 42, -42, 255, 1000000, INT_MAX, INT_MIN, LLONG_MAX, LLONG_MIN};
	printf("static const struct int_case int_cases[] = {\n");
	for (size_t i = 0; i < sizeof ifs / sizeof *ifs; i++)
		for (size_t j = 0; j < sizeof ivs / sizeof *ivs; j++) {
			long long v = ivs[j];
			if (ifs[i].kind == 'i' && (v > UINT_MAX || v < INT_MIN)) continue;
			if (ifs[i].kind == 'i') snprintf(buf, sizeof buf, ifs[i].f, (int)v);
			else snprintf(buf, sizeof buf, ifs[i].f, v);
			printf("\t{"); cstr(ifs[i].f); printf(", '%c', (long long)0x%llxULL, ", ifs[i].kind, (unsigned long long)v);
			cstr(buf); printf("},\n");
		}
	printf("};\n\n");

	// Floating point.
	const char *ffs[] = {"%f","%.0f","%.1f","%.20f","%#.0f","%+f","% f","%010.3f","%-12.3f|","%'f",
		"%e","%.0e","%#.0e","%E","%.12e","%g","%G","%.3g","%#g","%.0g","%.17g","%-+12.4g|",
		"%a","%A","%.3a","%.0a","%#.0a","%+a"};
	double fvs[] = {0.0, -0.0, 1.0, 0.1, 0.5, 2.5, -1.5, 1e20, 1e100, 1e308, 1e-5, 1e-300, 5e-324,
		123456789.125, 9.5, 0.95, 99.995, 1.0/3, 2.0/3, 1e15, 1e16, 1e17, INFINITY, -INFINITY, NAN};
	printf("static const struct float_case float_cases[] = {\n");
	for (size_t i = 0; i < sizeof ffs / sizeof *ffs; i++)
		for (size_t j = 0; j < sizeof fvs / sizeof *fvs; j++) {
			double v = fvs[j];
			// glibc and musl write subnormal numbers differently with %a (both right).
			if (strchr(ffs[i], 'a') || strchr(ffs[i], 'A'))
				if (v != 0 && fabs(v) < DBL_MIN) continue;
			snprintf(buf, sizeof buf, ffs[i], v);
			printf("\t{"); cstr(ffs[i]); printf(", ");
			if (isnan(v)) printf("NAN");
			else if (isinf(v)) printf(v < 0 ? "-INFINITY" : "INFINITY");
			else printf("%a", v);
			printf(", "); cstr(buf); printf("},\n");
		}
	printf("};\n\n");

	// Long double: the decimal conversions only (%La's first digit is
	// the C library's choice).
	const char *lfs[] = {"%Lf", "%.30Lf", "%Le", "%.25Le", "%Lg", "%.21Lg"};
	long double lvs[] = {0.1L, 1.0L/3, 1e4000L, 1e-4000L, LDBL_MAX, 123456789012345678.25L};
	printf("static const struct long_double_case long_double_cases[] = {\n");
	for (size_t i = 0; i < sizeof lfs / sizeof *lfs; i++)
		for (size_t j = 0; j < sizeof lvs / sizeof *lvs; j++) {
			snprintf(buf, sizeof buf, lfs[i], lvs[j]);
			if (strlen(buf) > 300) continue;
			printf("\t{"); cstr(lfs[i]); printf(", %LaL, ", lvs[j]); cstr(buf); printf("},\n");
		}
	printf("};\n\n");

	// Text to numbers.
	const char *texts[] = {"0", "-0", "1", "0.1", "1e308", "1e309", "-1e309", "1e-400", "4.9e-324",
		"2.4703282292062327e-324", "2.4703282292062328e-324", "1.7976931348623157e308",
		"1.7976931348623158e308", "1.7976931348623159e308", "123456789012345678901234567890",
		"0.000000000000000000000000000000000000000000001", "  42abc", "\t-3.25e+2x", "0x1.8p3",
		"0X1P-1074", "0x1p-1075", "0x.8", "0x", "0x.p", ".5", "1.", "1e", "1e+", "1e+x", "-.e1",
		"+-1", "1.2.3", "inf", "-INF", "infinity", "infinit", "nan", "NAN(123)", "nan(12",
		"3.4028235e38", "3.4028236e38", "1.17549435e-38", "1.4e-45", "9007199254740993",
		"9007199254740993.0000000001", "2.2250738585072011e-308", "2.2250738585072012e-308",
		"1e23", "8.98846567431158e307", "0.30000000000000004", "1000000000000000000000000",
		"6.02214076e23", "1.602176634e-19", "3.14159265358979323846264338327950288"};
	printf("static const struct scan_case scan_cases[] = {\n");
	for (size_t i = 0; i < sizeof texts / sizeof *texts; i++) {
		char *end;
		errno = 0; double d = strtod(texts[i], &end); int de = errno; long dn = end - texts[i];
		errno = 0; float f = strtof(texts[i], &end); int fe = errno; long fn = end - texts[i];
		errno = 0; long double ld = strtold(texts[i], &end); int le = errno; long ln = end - texts[i];
		printf("\t{"); cstr(texts[i]);
		uint64_t db; memcpy(&db, &d, 8); uint32_t fb; memcpy(&fb, &f, 4);
		uint64_t lm; uint16_t lx; memcpy(&lm, &ld, 8); memcpy(&lx, (char *)&ld + 8, 2);
		if (isnan(d)) db &= ~(1ULL << 63), db = 0x7ff8000000000000ULL;
		if (isnan(f)) fb = 0x7fc00000U;
		if (isnan(ld)) lm = 0xc000000000000000ULL, lx = 0x7fff;
		printf(",\n\t\t0x%016llxULL, %ld, %d, 0x%08xU, %ld, %d, 0x%016llxULL, 0x%04x, %ld, %d},\n",
			(unsigned long long)db, dn, de == ERANGE, fb, fn, fe == ERANGE, (unsigned long long)lm, lx, ln, le == ERANGE);
	}
	printf("};\n\n");

	// strftime.
	long long times[] = {0, 1, 1000, 951782400, 1230768000, 1262217600, 1293753600, 0x7fffffffLL,
		0x80000000LL, 4102444800LL, -2208988800LL, 253402300799LL};
	const char *tfs[] = {"%a %A %b %B %h", "%c", "%C %d %D %e %F", "%g %G %V", "%H %I %j %m %M %p",
		"%r %R %S %T", "%u %U %w %W", "%x %X", "%y %Y %z %%", "%s", "%n%t|", "%Ey %Od %OH",
		"%-d %_d %0e %-j", "[%10Y]"};
	printf("static const struct time_case time_cases[] = {\n");
	for (size_t i = 0; i < sizeof times / sizeof *times; i++) {
		time_t t = times[i];
		struct tm tm;
		gmtime_r(&t, &tm);
		for (size_t j = 0; j < sizeof tfs / sizeof *tfs; j++) {
			size_t n = strftime(buf, sizeof buf, tfs[j], &tm);
			if (!n) continue;
			printf("\t{%lldLL, ", times[i]); cstr(tfs[j]); printf(", "); cstr(buf); printf("},\n");
		}
	}
	printf("};\n");
	return 0;
}
