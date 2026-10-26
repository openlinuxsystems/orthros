#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds awk for Radix (ob(1), PORTS) with its own makefile, and installs
# it and its manual page. The grammar is compiled with the host's bison,
# and the table of its operators by a program built for the host.
set -eu

cp -R "$PORT_SRC/." "$PORT_BUILD"
cd "$PORT_BUILD"
make -j"$JOBS" CC="$CC" HOSTCC=cc CFLAGS="$CFLAGS" YACC="bison -d" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
mkdir -p "$SYSROOT/system/commands" "$SYSROOT/system/resources/man/man1"
$STRIP -o "$SYSROOT/system/commands/awk" a.out
cp awk.1 "$SYSROOT/system/resources/man/man1/awk.1"
printf '%s\n' system/commands/awk system/resources/man/man1/awk.1 >"$PORT_INFO/image"
