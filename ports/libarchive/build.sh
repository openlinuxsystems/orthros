#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds libarchive for Radix (ob(1), PORTS) and installs bsdtar, bsdcpio,
# bsdcat and bsdunzip, as tar, cpio and unzip too, their manual pages and
# those of the formats. It uses zlib, bzip2, xz, lz4 and zstd; no
# cryptography library yet, so encrypted zip archives other than the old
# ZipCrypto ones, and mtree's checksums, are not supported; no XML
# library, so no xar archives; no iconv, which mlibc lacks, so names are
# converted between UTF-8 and other character sets only where libarchive
# does so itself; and no ACLs or extended attributes, which Radix lacks.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --disable-shared --enable-bsdtar=static \
	--enable-bsdcpio=static --enable-bsdcat=static --enable-bsdunzip=static \
	--enable-posix-regex-lib=libc --disable-acl --disable-xattr --without-iconv \
	--without-openssl --without-nettle --without-mbedtls --without-libb2 --without-lzo2 \
	--without-xml2 --without-expat >configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
# Every compressor's library must have been found: without one,
# libarchive runs the compressor's program instead, or fails.
for library in LIBZ LIBBZ2 LIBLZMA LIBLZ4 LIBZSTD ZSTD_compressStream; do
	grep -q "^#define HAVE_$library 1" config.h || {
		echo "libarchive's configure did not set HAVE_$library" >&2
		exit 1
	}
done
make -j"$JOBS" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null
rm -f "$SYSROOT/system/lib/libarchive.la"

bin="$SYSROOT/system/bin"
man="$SYSROOT/system/share/man"
: >"$PORT_INFO/image"
for program in bsdtar bsdcpio bsdcat bsdunzip; do
	$STRIP "$bin/$program"
	echo "system/bin/$program"
	echo "system/share/man/man1/$program.1"
done >>"$PORT_INFO/image"
# bsdcat keeps its name: cat is the core utilities'.
for name in tar cpio unzip; do
	ln -sfn "bsd$name" "$bin/$name"
	ln -sfn "bsd$name.1" "$man/man1/$name.1"
	echo "system/bin/$name"
	echo "system/share/man/man1/$name.1"
done >>"$PORT_INFO/image"
for page in cpio libarchive-formats mtree tar; do
	echo "system/share/man/man5/$page.5"
done >>"$PORT_INFO/image"
