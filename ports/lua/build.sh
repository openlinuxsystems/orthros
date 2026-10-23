#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds the Lua interpreter, statically linked, with the POSIX parts of
# its libraries (LUA_USE_POSIX: mkstemp, popen, isatty) but without
# dlopen, and installs it with the language's test suite under
# /system/tests/lua. `ob` runs it (ob(1), PORTS) with CC, CFLAGS, STRIP,
# PORT_SRC, PORT_BUILD and SYSROOT set.
set -eu

# The interpreter and the libraries, as Lua's makefile builds them: every
# source but the test library (ltests.c), the one-file build (onelua.c)
# and the compiler (luac is not in the repository).
objects=
for name in lapi lcode lctype ldebug ldo ldump lfunc lgc llex lmem lobject \
	lopcodes lparser lstate lstring ltable ltm lundump lvm lzio \
	lauxlib lbaselib lcorolib ldblib liolib lmathlib loadlib loslib \
	lstrlib ltablib lutf8lib linit lua; do
	$CC $CFLAGS -std=c99 -Wall -DLUA_USE_POSIX -c "$PORT_SRC/$name.c" \
		-o "$PORT_BUILD/$name.o"
	objects="$objects $PORT_BUILD/$name.o"
done

out="$SYSROOT/system/tests/lua"
rm -rf "$out"
mkdir -p "$out/testes"
# shellcheck disable=SC2086 # one word per object
$CC -o "$PORT_BUILD/lua" $objects -lm
# The image's copy without symbols; the build's keeps them, for a debugger.
$STRIP -o "$out/lua" "$PORT_BUILD/lua"
cp "$PORT_SRC/testes/"*.lua "$out/testes/"
