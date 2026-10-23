#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds bc and dc for Radix (ob(1), PORTS) with bc's own configure
# script, and installs them and their manual pages. Its line editing is
# its own; its messages are in English only (no catgets catalogs). The
# program that turns bc's built-in libraries into C is built for the
# host.
set -eu

cd "$PORT_SRC"
HOSTCC=cc HOSTCFLAGS=-O2 PREFIX=/system DESTDIR="$SYSROOT" \
	./configure.sh -O 2 -N -G -T -i >"$PORT_BUILD/configure.log" 2>&1 || {
	tail -n 40 "$PORT_BUILD/configure.log" >&2
	exit 1
}
make -j"$JOBS" >"$PORT_BUILD/make.log" 2>&1 || {
	tail -n 60 "$PORT_BUILD/make.log" >&2
	exit 1
}
make install >/dev/null

: >"$PORT_INFO/image"
for program in bc dc; do
	[ -L "$SYSROOT/system/bin/$program" ] || $STRIP "$SYSROOT/system/bin/$program"
	echo "system/bin/$program" >>"$PORT_INFO/image"
	echo "system/share/man/man1/$program.1" >>"$PORT_INFO/image"
done
