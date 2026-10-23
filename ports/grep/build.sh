#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds grep for Radix (ob(1), PORTS), without translations, and
# installs it, its manual page and the egrep and fgrep scripts, which run
# it as grep -E and grep -F; not gnulib's tests, which are not run here.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --disable-nls --disable-perl-regexp >configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" SUBDIRS="lib doc src" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install SUBDIRS="lib doc src" DESTDIR="$SYSROOT" >/dev/null

: >"$PORT_INFO/image"
$STRIP "$SYSROOT/system/bin/grep"
for file in bin/grep bin/egrep bin/fgrep share/man/man1/grep.1; do
	echo "system/$file" >>"$PORT_INFO/image"
done
