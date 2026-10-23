// SPDX-FileCopyrightText: 2005-2020 Rich Felker, et al.
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: MIT

// Message catalogs for catopen(3), catgets(3) and catclose(3). This is
// musl 1.2.5's (src/locale/catopen.c, catgets.c and catclose.c,
// https://musl.libc.org, MIT), in C++: a catalog is a file in the format
// the BSDs' and musl's gencat write, mapped whole, and a name without a
// '/' is looked for along NLSPATH. mlibc had none of them. The locale
// NL_CAT_LOCALE names is LC_MESSAGES's, as setlocale gives it; musl
// asks its own nl_langinfo item for it.

#ifndef MLIBC_CATALOG_HPP
#define MLIBC_CATALOG_HPP

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <locale.h>
#include <nl_types.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

namespace mlibc::catalog {

// The numbers of a catalog are big-endian 32-bit words.
inline uint32_t word(const void *p)
{
	return __builtin_bswap32(*static_cast<const uint32_t *>(p));
}

// The first byte after s that is c or the end of the string.
inline const char *chrnul(const char *s, int c)
{
	while (*s && *s != c) s++;
	return s;
}

// Maps the file `name` read-only, whole: returns where, and its size in
// *size; or null.
inline const unsigned char *map_file(const char *name, size_t *size)
{
	int fd = ::open(name, O_RDONLY|O_CLOEXEC);
	if (fd < 0) return nullptr;
	struct stat st;
	void *map = MAP_FAILED;
	if (!fstat(fd, &st) && st.st_size > 0)
		map = mmap(nullptr, st.st_size, PROT_READ, MAP_SHARED, fd, 0);
	::close(fd);
	if (map == MAP_FAILED) return nullptr;
	*size = st.st_size;
	return static_cast<const unsigned char *>(map);
}

inline nl_catd do_catopen(const char *name)
{
	size_t size;
	const unsigned char *map = map_file(name, &size);
	/* Size recorded in the file must match file size; otherwise
	 * the information needed to unmap the file will be lost. */
	if (!map || word(map) != 0xff88ff89 || 20+word(map+8) != size) {
		if(map) munmap(const_cast<unsigned char *>(map), size);
		errno = ENOENT;
		return (nl_catd)-1;
	}
	return (nl_catd)map;
}

inline nl_catd open_catalog(const char *name, int oflag)
{
	nl_catd catd;

	if (strchr(name, '/')) return do_catopen(name);

	char buf[PATH_MAX];
	size_t i;
	const char *path, *lang, *p, *z;
	if (getauxval(AT_SECURE) || !(path = getenv("NLSPATH"))) {
		errno = ENOENT;
		return (nl_catd)-1;
	}
	lang = oflag ? setlocale(LC_MESSAGES, nullptr) : getenv("LANG");
	if (!lang) lang = "";
	for (p=path; *p; p=z) {
		i = 0;
		z = chrnul(p, ':');
		for (; p<z; p++) {
			const char *v;
			size_t l;
			if (*p!='%') v=p, l=1;
			else switch (*++p) {
			case 'N': v=name; l=strlen(v); break;
			case 'L': v=lang; l=strlen(v); break;
			case 'l': v=lang; l=strcspn(v,"_.@"); break;
			case 't':
				v=chrnul(lang,'_');
				if (*v) v++;
				l=strcspn(v,".@");
				break;
			case 'c': v="UTF-8"; l=5; break;
			case '%': v="%"; l=1; break;
			default: v=0;
			}
			if (!v || l >= sizeof buf - i) {
				break;
			}
			memcpy(buf+i, v, l);
			i += l;
		}
		if (!*z && (p<z || !i)) break;
		if (p<z) continue;
		if (*z) z++;
		buf[i] = 0;
		/* Leading : or :: in NLSPATH is same as %N */
		catd = do_catopen(i ? buf : name);
		if (catd != (nl_catd)-1) return catd;
	}
	errno = ENOENT;
	return (nl_catd)-1;
}

inline int cmp(const void *a, const void *b)
{
	uint32_t x = word(a), y = word(b);
	return x<y ? -1 : x>y ? 1 : 0;
}

inline char *message(nl_catd catd, int set_id, int msg_id, const char *s)
{
	const char *map = (const char *)catd;
	uint32_t nsets = word(map+4);
	const char *sets = map+20;
	const char *msgs = map+20+word(map+12);
	const char *strings = map+20+word(map+16);
	uint32_t set_id_be = __builtin_bswap32(set_id);
	uint32_t msg_id_be = __builtin_bswap32(msg_id);
	const char *set = static_cast<const char *>(bsearch(&set_id_be, sets, nsets, 12, cmp));
	if (!set) {
		errno = ENOMSG;
		return (char *)s;
	}
	uint32_t nmsgs = word(set+4);
	msgs += 12*word(set+8);
	const char *msg = static_cast<const char *>(bsearch(&msg_id_be, msgs, nmsgs, 12, cmp));
	if (!msg) {
		errno = ENOMSG;
		return (char *)s;
	}
	return (char *)(strings + word(msg+8));
}

inline int close_catalog(nl_catd catd)
{
	char *map = (char *)catd;
	munmap(map, word(map+8)+20);
	return 0;
}

} // namespace mlibc::catalog

#endif // MLIBC_CATALOG_HPP
