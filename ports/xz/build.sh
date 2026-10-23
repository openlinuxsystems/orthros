#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds XZ Utils for Radix (ob(1), PORTS), without translations, and
# installs xz under its other names, lzmainfo, the xzgrep, xzdiff, xzless
# and xzmore scripts under theirs, their manual pages, and the static
# liblzma with its headers for the ports built on it. xzdec and lzmadec,
# smaller programs that only decompress, are left out: xz does what they
# do.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --disable-nls --disable-doc --disable-shared \
	--disable-xzdec --disable-lzmadec >configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null
# libtool's notes on the library name its place without the sysroot.
rm -f "$SYSROOT/system/lib/liblzma.la"

$STRIP "$SYSROOT/system/bin/xz" "$SYSROOT/system/bin/lzmainfo"
for name in xz unxz xzcat lzma unlzma lzcat lzmainfo xzdiff xzcmp lzdiff lzcmp xzgrep xzegrep \
	xzfgrep lzgrep lzegrep lzfgrep xzless lzless xzmore lzmore; do
	echo "system/bin/$name"
	echo "system/share/man/man1/$name.1"
done >"$PORT_INFO/image"
