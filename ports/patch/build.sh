#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds patch for Radix (ob(1), PORTS), without extended attributes,
# which Radix does not have yet, and installs it and its manual page.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --disable-xattr >configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null

: >"$PORT_INFO/image"
$STRIP "$SYSROOT/system/bin/patch"
echo "system/bin/patch" >>"$PORT_INFO/image"
echo "system/share/man/man1/patch.1" >>"$PORT_INFO/image"
