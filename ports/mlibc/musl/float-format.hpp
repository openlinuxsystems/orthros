// SPDX-FileCopyrightText: 2005-2020 Rich Felker, et al.
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: MIT

// printf's %e, %f, %g and %a, exactly: each digit is the true one, for
// any precision and any double or long double. This is musl 1.2.5's
// fmt_fp (src/stdio/vfprintf.c, https://musl.libc.org, MIT), made a
// template over the sink and the character type, with the locale's
// decimal point and thousands grouping (%'f), which musl does not do.
// frigg's print_float, which mlibc used, went through a uint64_t and
// pow(), so 1e20 with %f or anything with %.20f came out wrong.

#ifndef MLIBC_FLOAT_FORMAT_HPP
#define MLIBC_FLOAT_FORMAT_HPP

#include <errno.h>
#include <float.h>
#include <limits.h>
#include <math.h>
#include <stdint.h>
#include <frg/formatting.hpp>

namespace mlibc::float_format {

#define MLIBC_MAX(a,b) ((a)>(b) ? (a) : (b))
#define MLIBC_MIN(a,b) ((a)<(b) ? (a) : (b))

// musl's flags.
inline constexpr unsigned ALT_FORM = 1U << ('#' - ' ');
inline constexpr unsigned ZERO_PAD = 1U << ('0' - ' ');
inline constexpr unsigned LEFT_ADJ = 1U << ('-' - ' ');
inline constexpr unsigned PAD_POS = 1U << (' ' - ' ');
inline constexpr unsigned MARK_POS = 1U << ('+' - ' ');
inline constexpr unsigned GROUPED = 1U << ('\'' - ' ');

// Do not override this check: the code below depends on float.h's
// constants being right, and may overflow the stack if they are not.
#if LDBL_MANT_DIG == 53
typedef char compiler_defines_long_double_incorrectly[9-(int)sizeof(long double)];
#endif

inline char *fmt_u(uintmax_t x, char *s) {
	unsigned long y;
	for (   ; x>ULONG_MAX; x/=10) *--s = '0' + x%10;
	for (y=x;           y; y/=10) *--s = '0' + y%10;
	return s;
}

// Whether a thousands separator goes before the last `rest` digits of an
// integer part, by `grouping` (localeconv(3)): each byte is the size of a
// group counting from the right, CHAR_MAX ends the grouping, and the end
// of the string repeats the size before it.
inline bool group_boundary(const char *grouping, int rest) {
	int at = 0;
	for (int size = 0;; grouping++) {
		int c = static_cast<unsigned char>(*grouping);
		if (!c)
			return size && rest > at && (rest - at) % size == 0;
		if (c >= CHAR_MAX)
			return false;
		size = c;
		at += size;
		if (at >= rest)
			return at == rest;
	}
}

// How many separators an integer part of `digits` digits takes.
inline int group_count(const char *grouping, int digits) {
	int n = 0;
	for (int rest = 1; rest < digits; rest++)
		if (group_boundary(grouping, rest))
			n++;
	return n;
}

template<typename Char, typename Sink>
struct Output {
	Sink &sink;
	const frg::locale_options<Char> &locale;
	// With %'f: the integer part's digits still to write, and whether to
	// group them.
	bool grouped = false;
	int rest = 0;

	void out(const char *s, size_t l) {
		for (; l; l--, s++) {
			sink.append(static_cast<Char>(*s));
			if (grouped && --rest > 0 && group_boundary(locale.grouping, rest))
				sink.append(locale.thousands_sep);
		}
	}

	void pad(char c, int w, int l, unsigned fl) {
		if (fl & (LEFT_ADJ | ZERO_PAD) || l >= w) return;
		for (l = w - l; l > 0; l--)
			sink.append(static_cast<Char>(c));
	}

	void radix() {
		sink.append(locale.decimal_point);
	}
};

// Formats `y` as conversion `t` (e, f, g, a or their capitals) with width
// `w`, precision `p` (-1 for none) and flags `fl`. Returns the characters
// it wrote, or -1 with errno EOVERFLOW if they would be more than INT_MAX.
template<typename Char, typename Sink>
int fmt_fp(Output<Char, Sink> &f, long double y, int w, int p, unsigned fl, int t) {
	static const char xdigits[16] = {
		'0', '1', '2', '3', '4', '5', '6', '7',
		'8', '9', 'A', 'B', 'C', 'D', 'E', 'F'
	};
	uint32_t big[(LDBL_MANT_DIG+28)/29 + 1          // mantissa expansion
		+ (LDBL_MAX_EXP+LDBL_MANT_DIG+28+8)/9]; // exponent expansion
	uint32_t *a, *d, *r, *z;
	int e2=0, e, i, j, l;
	char buf[9+LDBL_MANT_DIG/4], *s;
	const char *prefix="-0X+0X 0X-0x+0x 0x";
	int pl;
	char ebuf0[3*sizeof(int)], *ebuf=&ebuf0[3*sizeof(int)], *estr = ebuf;
	// The decimal point's length, and the separators' (Dukovo).
	int rl = frg::generic_strlen(f.locale.decimal_point);
	int sl = 0;

	pl=1;
	if (signbit(y)) {
		y=-y;
	} else if (fl & MARK_POS) {
		prefix+=3;
	} else if (fl & PAD_POS) {
		prefix+=6;
	} else prefix++, pl=0;

	if (!isfinite(y)) {
		const char *s = (t&32)?"inf":"INF";
		if (y!=y) s=(t&32)?"nan":"NAN";
		f.pad(' ', w, 3+pl, fl&~ZERO_PAD);
		f.out(prefix, pl);
		f.out(s, 3);
		f.pad(' ', w, 3+pl, fl^LEFT_ADJ);
		return MLIBC_MAX(w, 3+pl);
	}

	y = frexpl(y, &e2) * 2;
	if (y) e2--;

	if ((t|32)=='a') {
		long double round = 8.0;
		int re;

		if (t&32) prefix += 9;
		pl += 2;

		if (p<0 || p>=LDBL_MANT_DIG/4-1) re=0;
		else re=LDBL_MANT_DIG/4-1-p;

		if (re) {
			round *= 1<<(LDBL_MANT_DIG%4);
			while (re--) round*=16;
			if (*prefix=='-') {
				y=-y;
				y-=round;
				y+=round;
				y=-y;
			} else {
				y+=round;
				y-=round;
			}
		}

		estr=fmt_u(e2<0 ? -e2 : e2, ebuf);
		if (estr==ebuf) *--estr='0';
		*--estr = (e2<0 ? '-' : '+');
		*--estr = t+('p'-'a');

		s=buf;
		do {
			int x=y;
			*s++=xdigits[x]|(t&32);
			y=16*(y-x);
			if (s-buf==1 && (y||p>0||(fl&ALT_FORM))) *s++='.';
		} while (y);

		// Whether buf has a '.', which goes out as the locale's decimal
		// point (Dukovo).
		int point = s-buf > 1;
		if (p > INT_MAX-1-rl-(ebuf-estr)-pl)
			goto overflow;
		if (p && s-buf-2 < p)
			l = (p+2) + (ebuf-estr);
		else
			l = (s-buf) + (ebuf-estr);
		int zeros = l-(ebuf-estr)-(s-buf);
		l += point*(rl-1);

		f.pad(' ', w, pl+l, fl);
		f.out(prefix, pl);
		f.pad('0', w, pl+l, fl^ZERO_PAD);
		f.out(buf, 1);
		if (point) {
			f.radix();
			f.out(buf+2, s-buf-2);
		}
		f.pad('0', zeros, 0, 0);
		f.out(estr, ebuf-estr);
		f.pad(' ', w, pl+l, fl^LEFT_ADJ);
		return MLIBC_MAX(w, pl+l);
	}
	if (p<0) p=6;

	if (y) y *= 0x1p28, e2-=28;

	if (e2<0) a=r=z=big;
	else a=r=z=big+sizeof(big)/sizeof(*big) - LDBL_MANT_DIG - 1;

	do {
		*z = y;
		y = 1000000000*(y-*z++);
	} while (y);

	while (e2>0) {
		uint32_t carry=0;
		int sh=MLIBC_MIN(29,e2);
		for (d=z-1; d>=a; d--) {
			uint64_t x = ((uint64_t)*d<<sh)+carry;
			*d = x % 1000000000;
			carry = x / 1000000000;
		}
		if (carry) *--a = carry;
		while (z>a && !z[-1]) z--;
		e2-=sh;
	}
	while (e2<0) {
		uint32_t carry=0, *b;
		int sh=MLIBC_MIN(9,-e2), need=1+(p+LDBL_MANT_DIG/3U+8)/9;
		for (d=a; d<z; d++) {
			uint32_t rm = *d & ((1<<sh)-1);
			*d = (*d>>sh) + carry;
			carry = (1000000000>>sh) * rm;
		}
		if (!*a) a++;
		if (carry) *z++ = carry;
		/* Avoid (slow!) computation past requested precision */
		b = (t|32)=='f' ? r : a;
		if (z-b > need) z = b+need;
		e2+=sh;
	}

	if (a<z) for (i=10, e=9*(r-a); *a>=(uint32_t)i; i*=10, e++);
	else e=0;

	/* Perform rounding: j is precision after the radix (possibly neg) */
	j = p - ((t|32)!='f')*e - ((t|32)=='g' && p);
	if (j < 9*(z-r-1)) {
		uint32_t x;
		/* We avoid C's broken division of negative numbers */
		d = r + 1 + ((j+9*LDBL_MAX_EXP)/9 - LDBL_MAX_EXP);
		j += 9*LDBL_MAX_EXP;
		j %= 9;
		for (i=10, j++; j<9; i*=10, j++);
		x = *d % i;
		/* Are there any significant digits past j? */
		if (x || d+1!=z) {
			long double round = 2/LDBL_EPSILON;
			long double small;
			if ((*d/i & 1) || (i==1000000000 && d>a && (d[-1]&1)))
				round += 2;
			if (x<(uint32_t)i/2) small=0x0.8p0;
			else if (x==(uint32_t)i/2 && d+1==z) small=0x1.0p0;
			else small=0x1.8p0;
			if (pl && *prefix=='-') round*=-1, small*=-1;
			*d -= x;
			/* Decide whether to round by probing round+small */
			if (round+small != round) {
				*d = *d + i;
				while (*d > 999999999) {
					*d--=0;
					if (d<a) *--a=0;
					(*d)++;
				}
				for (i=10, e=9*(r-a); *a>=(uint32_t)i; i*=10, e++);
			}
		}
		if (z>d+1) z=d+1;
	}
	for (; z>a && !z[-1]; z--);

	if ((t|32)=='g') {
		if (!p) p++;
		if (p>e && e>=-4) {
			t--;
			p-=e+1;
		} else {
			t-=2;
			p--;
		}
		if (!(fl&ALT_FORM)) {
			/* Count trailing zeros in last place */
			if (z>a && z[-1]) for (i=10, j=0; z[-1]%i==0; i*=10, j++);
			else j=9;
			if ((t|32)=='f')
				p = MLIBC_MIN(p,MLIBC_MAX(0,9*(z-r-1)-j));
			else
				p = MLIBC_MIN(p,MLIBC_MAX(0,9*(z-r-1)+e-j));
		}
	}
	if (p > INT_MAX-1-(p || (fl&ALT_FORM))*rl)
		goto overflow;
	l = 1 + p + (p || (fl&ALT_FORM)) * rl;
	if ((t|32)=='f') {
		if (e > INT_MAX-l) goto overflow;
		if (e>0) l+=e;
		// The separators %'f puts in the integer part (Dukovo).
		if (fl & GROUPED) {
			int seps = group_count(f.locale.grouping, e>0 ? e+1 : 1);
			sl = frg::generic_strlen(f.locale.thousands_sep);
			if (seps && sl > (INT_MAX-l)/seps) goto overflow;
			l += seps * sl;
		}
	} else {
		estr=fmt_u(e<0 ? -e : e, ebuf);
		while(ebuf-estr<2) *--estr='0';
		*--estr = (e<0 ? '-' : '+');
		*--estr = t;
		if (ebuf-estr > INT_MAX-l) goto overflow;
		l += ebuf-estr;
	}

	if (l > INT_MAX-pl) goto overflow;
	f.pad(' ', w, pl+l, fl);
	f.out(prefix, pl);
	f.pad('0', w, pl+l, fl^ZERO_PAD);

	if ((t|32)=='f') {
		if (a>r) a=r;
		if (sl) {
			f.grouped = true;
			f.rest = e>0 ? e+1 : 1;
		}
		for (d=a; d<=r; d++) {
			char *s = fmt_u(*d, buf+9);
			if (d!=a) while (s>buf) *--s='0';
			else if (s==buf+9) *--s='0';
			f.out(s, buf+9-s);
		}
		f.grouped = false;
		if (p || (fl&ALT_FORM)) f.radix();
		for (; d<z && p>0; d++, p-=9) {
			char *s = fmt_u(*d, buf+9);
			while (s>buf) *--s='0';
			f.out(s, MLIBC_MIN(9,p));
		}
		f.pad('0', p+9, 9, 0);
	} else {
		if (z<=a) z=a+1;
		for (d=a; d<z && p>=0; d++) {
			char *s = fmt_u(*d, buf+9);
			if (s==buf+9) *--s='0';
			if (d!=a) while (s>buf) *--s='0';
			else {
				f.out(s++, 1);
				if (p>0||(fl&ALT_FORM)) f.radix();
			}
			f.out(s, MLIBC_MIN(buf+9-s, p));
			p -= buf+9-s;
		}
		f.pad('0', p+18, 18, 0);
		f.out(estr, ebuf-estr);
	}

	f.pad(' ', w, pl+l, fl^LEFT_ADJ);

	return MLIBC_MAX(w, pl+l);

overflow:
	errno = EOVERFLOW;
	return -1;
}

#undef MLIBC_MAX
#undef MLIBC_MIN

} // namespace mlibc::float_format

namespace mlibc {

// Formats `value` as printf's conversion `t` (one of eEfFgGaA) with
// `opts`. Returns false, with errno EOVERFLOW, if it would be more than
// INT_MAX characters.
template<typename Char, typename Sink>
bool format_float(Sink &sink, long double value, Char t, const frg::format_options &opts,
		const frg::locale_options<Char> &locale) {
	using namespace float_format;
	unsigned fl = 0;
	if (opts.alt_conversion) fl |= ALT_FORM;
	if (opts.fill_zeros) fl |= ZERO_PAD;
	if (opts.left_justify) fl |= LEFT_ADJ;
	if (opts.plus_becomes_space) fl |= PAD_POS;
	if (opts.always_sign) fl |= MARK_POS;
	if (opts.group_thousands) fl |= GROUPED;
	Output<Char, Sink> out{sink, locale};
	return fmt_fp(out, value, opts.minimum_width, opts.precision ? *opts.precision : -1, fl,
			static_cast<int>(t)) >= 0;
}

} // namespace mlibc

#endif // MLIBC_FLOAT_FORMAT_HPP
