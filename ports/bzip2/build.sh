#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds bzip2 for Radix (ob(1), PORTS) with its own Makefile, which has
# no configure script and builds in its source, and installs bzip2,
# bunzip2 and bzcat, bzip2recover, the bzgrep, bzmore and bzdiff scripts
# under all their names, their manual pages, and the static libbz2 with
# its header for the ports built on it. Its install target is not used:
# it copies bzip2 three times and links with absolute paths.
set -eu

cd "$PORT_SRC"
make -j"$JOBS" CC="$CC" AR="$AR" RANLIB="$RANLIB" CFLAGS="$CFLAGS -D_FILE_OFFSET_BITS=64" \
	libbz2.a bzip2 bzip2recover >"$PORT_BUILD/make.log" 2>&1 || {
	tail -n 60 "$PORT_BUILD/make.log" >&2
	exit 1
}

bin="$SYSROOT/system/commands"
man="$SYSROOT/system/resources/man/man1"
mkdir -p "$bin" "$man" "$SYSROOT/system/libraries" "$SYSROOT/system/include"
cp libbz2.a "$SYSROOT/system/libraries/"
cp bzlib.h "$SYSROOT/system/include/"
for program in bzip2 bzip2recover; do
	cp "$program" "$bin/"
	$STRIP "$bin/$program"
done
for script in bzgrep bzmore bzdiff; do
	cp "$script" "$bin/"
	chmod 755 "$bin/$script"
	cp "$script.1" "$man/"
done
cp bzip2.1 "$man/"

# Each other name, a link to the program or script it runs, and to the
# page that tells of it.
: >"$PORT_INFO/image"
for name in bzip2 bunzip2:bzip2 bzcat:bzip2 bzip2recover:bzip2 bzgrep bzegrep:bzgrep \
	bzfgrep:bzgrep bzmore bzless:bzmore bzdiff bzcmp:bzdiff; do
	link=${name%%:*}
	target=${name#*:}
	if [ "$link" != "$target" ]; then
		[ "$link" = bzip2recover ] || ln -sfn "$target" "$bin/$link"
		ln -sfn "$target.1" "$man/$link.1"
	fi
	echo "system/commands/$link"
	echo "system/resources/man/man1/$link.1"
done >>"$PORT_INFO/image"
