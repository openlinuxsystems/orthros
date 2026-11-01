#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Compiles the C.UTF-8 locale and English's UTF-8 locales from glibc's
# locale sources with the build host's localedef (ob doctor checks for
# it), into the directories of files mlibc reads. Each directory is
# named with the codeset normalized, as glibc names them,
# /system/libraries/locale/C.utf8 and en_US.utf8: the C library finds
# them for C.UTF-8, C.utf8 and C.UTF8 alike. `ob` runs it (ob(1), PORTS)
# with PORT_SRC, PORT_BUILD and SYSROOT set.
set -eu

# English's countries with a UTF-8 locale in glibc's localedata/SUPPORTED.
english="AG AU BW CA DK GB HK IE IL IN NG NZ PH SC SG US ZA ZM ZW"

out="$SYSROOT/system/libraries/locale"
rm -rf "$out"
mkdir -p "$out"
# The locales' `copy` and `include` lines name files of the same
# directory, which I18NPATH points at.
compile() {
	I18NPATH="$PORT_SRC/localedata" localedef --no-archive \
		-i "$PORT_SRC/localedata/locales/$1" \
		-f "$PORT_SRC/localedata/charmaps/UTF-8" \
		"$out/$1.utf8"
}
compile C
for country in $english; do
	compile "en_$country"
done

# localedef makes a file that is the same in two locales a hard link,
# which the boot archive would hold twice: each becomes a symbolic link
# to the first of them in name order, which is C.utf8's for the
# character classes.
sums="$PORT_BUILD/sums"
: >"$sums"
cd "$out"
find . -type f | sed 's|^\./||' | LC_ALL=C sort | while read -r file; do
	sum=$(sha256sum "$file" | cut -d ' ' -f 1)
	first=$(grep "^$sum " "$sums" | cut -d ' ' -f 2)
	if [ -n "$first" ]; then
		up=$(dirname "$file" | sed 's|[^/][^/]*|..|g')
		rm "$file"
		ln -s "$up/$first" "$file"
	else
		echo "$sum $file" >>"$sums"
	fi
done
echo "locales: C.UTF-8 and $(echo "$english" | wc -w) English locales in /system/libraries/locale, $(du -sk . | cut -f 1) KiB"
