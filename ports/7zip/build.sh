#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds 7zz, 7-Zip's standalone program, for Radix (ob(1), PORTS; ADR
# 0090) with 7-Zip's own makefile for Clang, without the RAR code, which
# Debian's source leaves out, and without 7-Zip's assembly, which needs
# an assembler for MASM's syntax: its C does the same, a little slower.
# 7-Zip's makefile turns on every Clang warning; the usual ones are
# enough. Installs 7zz with 7z as another name for it, and a page for 7z
# that leads to 7zz's, which is Dukovo's own (docs/man/7zz.1.scd).
set -eu

cd "$PORT_SRC/CPP/7zip/Bundles/Alone2"
make -j"$JOBS" -f ../../cmpl_clang.mak CC="$CC" CXX="$CXX" O="$PORT_BUILD" \
	DISABLE_RAR=1 CFLAGS_WARN= CFLAGS_WARN_WALL="-Wall -Wextra" \
	>"$PORT_BUILD/make.log" 2>&1 || {
	tail -n 60 "$PORT_BUILD/make.log" >&2
	exit 1
}

bin="$SYSROOT/system/bin"
man="$SYSROOT/system/share/man/man1"
mkdir -p "$bin" "$man"
cp "$PORT_BUILD/7zz" "$bin/7zz"
$STRIP "$bin/7zz"
ln -sfn 7zz "$bin/7z"
echo ".so man1/7zz.1" >"$man/7z.1"
for file in bin/7zz bin/7z share/man/man1/7z.1; do
	echo "system/$file"
done >"$PORT_INFO/image"
