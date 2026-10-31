#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds nano for Radix (ob(1), PORTS), on ncurses' wide-character
# library and without translations, and installs it, rnano (its
# restricted mode), their manual pages, the syntax files, Dukovo's own
# for TOML among them (ADR 0216), and the system nanorc. The nanorc goes
# among the defaults Spark puts in /system/settings at boot (spark(8)),
# as nano reads it from there.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --disable-nls --enable-utf8 --disable-libmagic \
	>configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null
cp "$PORT_DIR/toml.nanorc" "$SYSROOT/system/resources/nano/toml.nanorc"

defaults="$SYSROOT/system/resources/default-settings"
mkdir -p "$defaults"
cp "$PORT_DIR/nanorc" "$defaults/nanorc"

: >"$PORT_INFO/image"
$STRIP "$SYSROOT/system/commands/nano"
for file in commands/nano commands/rnano resources/man/man1/nano.1 resources/man/man1/rnano.1 \
	resources/man/man5/nanorc.5 resources/default-settings/nanorc; do
	echo "system/$file" >>"$PORT_INFO/image"
done
for syntax in "$SYSROOT"/system/resources/nano/*.nanorc; do
	echo "system/resources/nano/${syntax##*/}" >>"$PORT_INFO/image"
done
