#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds nvi for Radix (ob(1), PORTS) and installs it as vi, with ex and
# view, which are vi run by those names, and their manual pages. nvi's
# CMakeLists.txt finds what the C library has by building programs; this
# does what it does with what it would find for Radix: wide characters
# on ncurses, no iconv, which the C library does not have yet (so files
# are read as UTF-8, the locale's encoding), BSD's regex code, and the
# BSD functions it lacks from compat/. Recovery files are kept in
# /var/tmp/vi.recover, which nvi makes.
set -eu

cd "$PORT_BUILD"
src=$PORT_SRC

# The headers CMakeLists.txt generates: each part's prototypes, the
# option and command tables, and the version.
for part in cl common ex vi; do
	(cd "$src" && sed -n 's/^ \* PUBLIC: \(.*\)/\1/p' "$part"/*.c) >"${part}_extern.h"
done
(cd "$src" && awk -f common/options.awk common/options.c) >options_def.h
(cd "$src" && awk -f ex/ex.awk ex/ex_cmd.c) >ex_def.h
sed -n 's/^.*version \([^)]*)\).*/#define VI_VERSION "\1"/p' "$src/README" >version.h
grep -q VI_VERSION version.h

# config.h as CMakeLists.txt would write it for Radix.
sed -e 's/^#cmakedefine \(USE_WIDECHAR\|HAVE_NCURSES_H\|HAVE_PTY_H\|HAVE_TERM_H\)$/#define \1/' \
	-e 's/^#cmakedefine \(.*\)$/\/* #undef \1 *\//' "$src/files/config.h.in" >config.h
sed -e 's|@vi_cv_path_msgcat@|/system/resources/vi/catalog/|' \
	-e 's|@vi_cv_path_preserve@|/var/tmp/vi.recover/|' "$src/files/pathnames.h.in" >pathnames.h

flags="$CFLAGS -std=gnu17 -D_GNU_SOURCE -D_XOPEN_SOURCE_EXTENDED -D__REGEX_PRIVATE \
-I$PORT_BUILD -I$src/regex -I$SYSROOT/system/include/db1 -I$PORT_DIR/compat \
-include $PORT_DIR/compat/compat.h -fstack-protector -Wno-deprecated-non-prototype"
# A makefile of the sources, so that make compiles them $JOBS at a time.
mkdir -p obj
{
	echo "CC = $CC"
	echo "FLAGS = $flags"
	objects=
	for source in "$src"/cl/*.c "$src"/common/*.c "$src"/ex/*.c "$src"/vi/*.c \
		"$src"/regex/regcomp.c "$src"/regex/regerror.c "$src"/regex/regexec.c \
		"$src"/regex/regfree.c "$PORT_DIR/compat/compat.c"; do
		object=obj/$(basename "$(dirname "$source")")_$(basename "$source" .c).o
		objects="$objects $object"
		printf '%s: %s\n\t@$(CC) $(FLAGS) -c %s -o $@\n' "$object" "$source" "$source"
	done
	printf 'vi:%s\n\t$(CC) -o $@ $^ -lncurses -ldb1\n' "$objects"
} >Makefile
make -j"$JOBS" vi >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}

bin="$SYSROOT/system/commands"
man="$SYSROOT/system/resources/man/man1"
mkdir -p "$bin" "$man"
cp vi "$bin/vi"
$STRIP "$bin/vi"
cp "$src/man/vi.1" "$man/vi.1"
for name in ex view; do
	ln -sfn vi "$bin/$name"
	ln -sfn vi.1 "$man/$name.1"
done
for name in vi ex view; do
	echo "system/commands/$name"
	echo "system/resources/man/man1/$name.1"
done >"$PORT_INFO/image"
