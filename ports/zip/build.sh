#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds Zip for Radix (ob(1), PORTS) and installs zip, zipcloak, zipnote
# and zipsplit and their manual pages. Its Makefile builds in its source.
# The flags are those its configure script would choose for Radix, which
# it finds by running programs, as it cannot here: bzip2 compression,
# 32-bit user and group IDs, 64-bit file sizes, Unicode names, and
# dirent.h and termios.h; no assembly.
set -eu

cd "$PORT_SRC"
make -f unix/Makefile -j"$JOBS" zips CC="$CC" BIND="$CC" CPP="$CC -E" \
	CFLAGS="$CFLAGS -I. -DUNIX -DBZIP2_SUPPORT -DUIDGID_NOT_16BIT -DLARGE_FILE_SUPPORT \
-DUNICODE_SUPPORT -DHAVE_DIRENT_H -DHAVE_TERMIOS_H" \
	OBJA= OCRCU8=crc32_.o OCRCTB= LFLAGS1= LFLAGS2=-lbz2 >"$PORT_BUILD/make.log" 2>&1 || {
	tail -n 60 "$PORT_BUILD/make.log" >&2
	exit 1
}

bin="$SYSROOT/system/commands"
man="$SYSROOT/system/resources/man/man1"
mkdir -p "$bin" "$man"
for program in zip zipcloak zipnote zipsplit; do
	cp "$program" "$bin/"
	$STRIP "$bin/$program"
	cp "man/$program.1" "$man/"
	echo "system/commands/$program"
	echo "system/resources/man/man1/$program.1"
done >"$PORT_INFO/image"
