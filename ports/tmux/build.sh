#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds tmux for Radix (ob(1), PORTS), on ncurses and libevent, and
# installs it and its manual page. Until users come in Phase 13 every
# directory shows mode 0755 (radix-posix(7)), so tmux is told that a
# socket directory others may read is safe (TMUX_SOCK_PERM): it would
# refuse /tmp/tmux-0 otherwise.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" CPPFLAGS="-D_GNU_SOURCE -DTMUX_SOCK_PERM=0" \
	>configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null

$STRIP "$SYSROOT/system/commands/tmux"
printf '%s\n' system/commands/tmux system/resources/man/man1/tmux.1 >"$PORT_INFO/image"
