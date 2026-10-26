#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds diff, cmp, diff3 and sdiff for Radix (ob(1), PORTS), without
# translations, and installs them and their manual pages; not gnulib's
# tests, which are not run here.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --disable-nls >configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" SUBDIRS="lib src doc man" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install SUBDIRS="lib src doc man" DESTDIR="$SYSROOT" >/dev/null

: >"$PORT_INFO/image"
for program in diff cmp diff3 sdiff; do
	$STRIP "$SYSROOT/system/commands/$program"
	echo "system/commands/$program" >>"$PORT_INFO/image"
	echo "system/resources/man/man1/$program.1" >>"$PORT_INFO/image"
done
