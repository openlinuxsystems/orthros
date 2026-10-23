#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds Zstandard for Radix (ob(1), PORTS) with its own Makefiles, which
# build in its source, and installs zstd under its other names, the
# zstdgrep and zstdless scripts, their manual pages, and the static
# libzstd, which compresses on several threads, with its headers for the
# ports built on it. -O3 is what upstream builds with. The Makefiles test
# the compiler for threads, zlib, liblzma and liblz4 by linking a
# program; their answers are given here instead, as Radix has them all.
set -eu

cd "$PORT_SRC"
make="make -j$JOBS CC=$CC AR=$AR CFLAGS=-O3 PREFIX=/system DESTDIR=$SYSROOT"
features="HAVE_PTHREAD=1 HAVE_ZLIB=1 HAVE_LZMA=1 HAVE_LZ4=1"
{
	$make -C lib libzstd.a-mt
	# shellcheck disable=SC2086 # one word per setting
	$make -C programs $features zstd-release
} >"$PORT_BUILD/make.log" 2>&1 || {
	tail -n 60 "$PORT_BUILD/make.log" >&2
	exit 1
}
$make -C lib install-static install-includes install-pc >/dev/null
# shellcheck disable=SC2086 # one word per setting
$make -C programs $features install >/dev/null

# zstdmt, zstd -T0, is told of in zstd's page as well.
ln -sfn zstd.1 "$SYSROOT/system/share/man/man1/zstdmt.1"
$STRIP "$SYSROOT/system/bin/zstd"
for name in zstd zstdcat unzstd zstdmt zstdgrep zstdless; do
	echo "system/bin/$name"
	echo "system/share/man/man1/$name.1"
done >"$PORT_INFO/image"
