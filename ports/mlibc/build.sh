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

# A static C library with the POSIX, glibc and BSD interfaces, using
# Clang's C headers and, to build, libstdc++'s freestanding C++ headers
# (which the cross file names). Programs link compiler-rt's builtins
# themselves.
meson setup "$PORT_BUILD" "$PORT_SRC" \
	--cross-file "$LIBC_CROSS_FILE" \
	--prefix=/system --libdir=lib --includedir=include \
	--buildtype=release \
	-Ddefault_library=static \
	-Duse_freestnd_hdrs=disabled \
	-Dlibgcc_dependency=false \
	-Ddefault_library_paths=/system/lib
meson compile -C "$PORT_BUILD" -j "$JOBS"
DESTDIR="$SYSROOT" meson install -C "$PORT_BUILD" --no-rebuild --quiet
