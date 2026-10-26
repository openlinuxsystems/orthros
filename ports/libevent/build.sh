#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds libevent for Radix (ob(1), PORTS): the static libraries, without
# OpenSSL, their headers and pkg-config files in the sysroot for the ports
# built on it. Nothing of it goes in the image by itself. Radix has poll
# and select, which libevent finds, and no epoll, eventfd or timerfd.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --disable-shared --disable-openssl --disable-samples \
	--disable-libevent-regress --disable-debug-mode >configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null
# libtool's notes on the libraries name their place without the sysroot,
# and event_rpcgen.py needs a Python Radix does not have yet.
rm -f "$SYSROOT"/system/libraries/libevent*.la "$SYSROOT/system/commands/event_rpcgen.py"

: >"$PORT_INFO/image"
