#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds zlib's static library for Radix (ob(1), PORTS) and installs it,
# its headers and its pkg-config file into the sysroot, for the ports
# built on it. zlib's configure is its own, not autoconf's: CHOST makes
# it build for another system, with the toolchain's programs.
set -eu

cd "$PORT_BUILD"
CHOST="$HOST" "$PORT_SRC/configure" --prefix=/system --static >configure.log
make -j"$JOBS" libz.a >make.log
make install DESTDIR="$SYSROOT" >/dev/null
