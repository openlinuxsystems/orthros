#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds jq for Radix (ob(1), PORTS), with the Oniguruma its release
# carries, and installs it and its manual page, which the release has
# made already: making it again needs Python and the documentation's
# tools.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --disable-shared --disable-docs --disable-valgrind \
	--with-oniguruma=builtin >configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null
rm -f "$SYSROOT/system/libraries/libjq.la" "$SYSROOT/system/libraries/libonig.la"

$STRIP "$SYSROOT/system/commands/jq"
echo system/commands/jq >"$PORT_INFO/image"
echo system/resources/man/man1/jq.1 >>"$PORT_INFO/image"
