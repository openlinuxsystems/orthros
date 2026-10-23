#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds gzip for Radix (ob(1), PORTS) and installs it, its scripts
# (gunzip, zcat, zgrep, zless, zdiff and the others) and their manual
# pages. configure looks for less on the build machine to decide whether
# to install zless; Radix has less, so it is told so.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" ac_cv_prog_LESS=less >configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
make -j"$JOBS" SUBDIRS="lib doc" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install SUBDIRS="lib doc" DESTDIR="$SYSROOT" >/dev/null

# The names with no page of their own are told of in another's.
man="$SYSROOT/system/share/man/man1"
ln -sfn gunzip.1 "$man/uncompress.1"
ln -sfn zgrep.1 "$man/zegrep.1"
ln -sfn zgrep.1 "$man/zfgrep.1"
$STRIP "$SYSROOT/system/bin/gzip"
for name in gzip gunzip uncompress zcat gzexe zcmp zdiff zegrep zfgrep zforce zgrep zless \
	zmore znew; do
	echo "system/bin/$name"
	echo "system/share/man/man1/$name.1"
done >"$PORT_INFO/image"
