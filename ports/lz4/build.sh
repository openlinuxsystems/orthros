#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds LZ4 for Radix (ob(1), PORTS) with its own Makefiles, which build
# in its source, and installs lz4 under its other names, its manual
# page, and the static liblz4 with its headers for the ports built on it.
# -O3 is what upstream builds with. The Makefiles test the compiler for
# threads by linking a program; Radix has them, so lz4 compresses on
# several processors whichever way that test goes.
set -eu

cd "$PORT_SRC"
make="make -j$JOBS CC=$CC AR=$AR CFLAGS=-O3 PREFIX=/system DESTDIR=$SYSROOT BUILD_SHARED=no"
{
	$make -C lib liblz4.a liblz4.pc
	$make -C programs HAVE_PTHREAD=1 lz4
} >"$PORT_BUILD/make.log" 2>&1 || {
	tail -n 60 "$PORT_BUILD/make.log" >&2
	exit 1
}
$make -C lib install >/dev/null
$make -C programs HAVE_PTHREAD=1 install >/dev/null

$STRIP "$SYSROOT/system/bin/lz4"
for name in lz4 lz4c lz4cat unlz4; do
	echo "system/bin/$name"
	echo "system/share/man/man1/$name.1"
done >"$PORT_INFO/image"
