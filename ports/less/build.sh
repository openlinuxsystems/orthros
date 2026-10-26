#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds less, lesskey and lessecho for Radix (ob(1), PORTS), on ncurses'
# terminal library and the C library's regular expressions, and installs
# them and their manual pages.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --with-regex=posix --with-editor=nano \
	>configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null

: >"$PORT_INFO/image"
for program in less lesskey lessecho; do
	$STRIP "$SYSROOT/system/commands/$program"
	echo "system/commands/$program" >>"$PORT_INFO/image"
	echo "system/resources/man/man1/$program.1" >>"$PORT_INFO/image"
done
