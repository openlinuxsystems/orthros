#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds mlibc for Radix and installs it into the sysroot. `ob ports`
# runs it (ob(1), PORTS) in a fresh copy of the source with the patches
# applied, with:
#   PORT_DIR         this directory
#   PORT_SRC         the source
#   PORT_BUILD       an empty build directory
#   SYSROOT          where to install (the image's root: /system is under it)
#   LIBC_CROSS_FILE  the Meson cross file for the C library
#   JOBS             how many jobs to run at once
set -eu

# The system layer, and its ABI headers: Linux's, but for those it has
# its own of.
rm -rf "$PORT_SRC/sysdeps/radix"
cp -R "$PORT_DIR/sysdeps/radix" "$PORT_SRC/sysdeps/radix"
mkdir -p "$PORT_SRC/sysdeps/radix/include/abi-bits"
sed -n "s|^		'include/abi-bits/\(.*\)',$|\1|p" "$PORT_SRC/sysdeps/radix/meson.build" |
	while read -r header; do
		[ -e "$PORT_SRC/sysdeps/radix/include/abi-bits/$header" ] ||
			ln -s "../../../../abis/linux/$header" "$PORT_SRC/sysdeps/radix/include/abi-bits/$header"
	done
# Headers of the Linux option that other systems have too and that need
# nothing of Linux's: getrandom's and the device number macros' (patch
# 0023), and the extended attribute calls' (0050, ADR 0256).
mkdir -p "$PORT_SRC/sysdeps/radix/include/sys"
for header in random.h sysmacros.h xattr.h; do
	ln -s "../../../../options/linux/include/sys/$header" "$PORT_SRC/sysdeps/radix/include/sys/$header"
done
# And malloc.h, of memalign (0035).
ln -s ../../../options/linux/include/malloc.h "$PORT_SRC/sysdeps/radix/include/malloc.h"

# Code from musl 1.2.5 that the patches use: exact floating-point
# conversions (0004), strftime (0006), the calendar arithmetic (0007),
# scanf's integers (0018), Internet addresses (0021), message catalogs
# (0028) and the time zone (0051).
cp "$PORT_DIR"/musl/*.hpp "$PORT_SRC/options/internal/include/mlibc/"

# The C library with the POSIX, glibc and BSD interfaces, using Clang's C
# headers and, to build, libstdc++'s freestanding C++ headers (which the
# cross file names): static, for system programs, and shared, with the
# dynamic linker (/system/libraries/ld.so), for bundles' programs (ADR 0071).
# Static programs link compiler-rt's builtins themselves; the cross file
# links them into the shared library and the dynamic linker.
meson setup "$PORT_BUILD" "$PORT_SRC" \
	--cross-file "$LIBC_CROSS_FILE" \
	--prefix=/system --libdir=libraries --includedir=include \
	--buildtype=release \
	-Ddefault_library=both \
	-Duse_freestnd_hdrs=disabled \
	-Dlibgcc_dependency=false \
	-Ddefault_library_paths=/system/libraries
meson compile -C "$PORT_BUILD" -j "$JOBS"
DESTDIR="$SYSROOT" meson install -C "$PORT_BUILD" --no-rebuild --quiet
