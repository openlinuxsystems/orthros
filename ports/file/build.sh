#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds file for Radix (ob(1), PORTS) and installs it, the compiled
# magic database and their manual pages, magic's in section 5 as on
# other systems. The database is compiled by file itself, and one for
# Radix cannot run here, so file is first built for the build machine,
# with nothing but what compiling the database needs.
set -eu

mkdir -p "$PORT_BUILD/host" "$PORT_BUILD/radix"
cd "$PORT_BUILD/host"
env -u PKG_CONFIG_LIBDIR -u PKG_CONFIG_SYSROOT_DIR CC=cc AR=ar RANLIB=ranlib NM=nm STRIP=strip \
	"$PORT_SRC/configure" --disable-shared --disable-zlib --disable-bzlib --disable-xzlib \
	--disable-zstdlib --disable-lzlib --disable-lrziplib --disable-libseccomp \
	>configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
# magic.h is made from magic.h.in, which make does first only for all.
env -u PKG_CONFIG_LIBDIR -u PKG_CONFIG_SYSROOT_DIR make -j"$JOBS" -C src magic.h file \
	>make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}

cd "$PORT_BUILD/radix"
x86_64-radix-configure "$PORT_SRC" --disable-shared --enable-fsect-man5 --disable-lzlib \
	--disable-lrziplib --disable-libseccomp >configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
# Every compression library must have been found: file without one
# cannot look inside such files, and says nothing of it.
for library in ZLIBSUPPORT BZLIBSUPPORT XZLIBSUPPORT ZSTDLIBSUPPORT; do
	grep -q "^#define $library 1" config.h || {
		echo "file's configure did not set $library" >&2
		exit 1
	}
done
make -j"$JOBS" FILE_COMPILE="$PORT_BUILD/host/src/file" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null
rm -f "$SYSROOT/system/libraries/libmagic.la"

$STRIP "$SYSROOT/system/commands/file"
for path in commands/file resources/misc/magic.mgc resources/man/man1/file.1 \
	resources/man/man5/magic.5; do
	echo "system/$path"
done >"$PORT_INFO/image"
