#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Compiles the C.UTF-8 locale from glibc's locale sources with the build
# host's localedef (ob doctor checks for it), into the directory of files
# mlibc reads. The directory is named with the codeset normalized, as
# glibc names it, /system/libraries/locale/C.utf8: the C library finds it for
# C.UTF-8, C.utf8 and C.UTF8 alike. `ob` runs it (ob(1), PORTS) with
# PORT_SRC and SYSROOT set.
set -eu

out="$SYSROOT/system/libraries/locale"
rm -rf "$out"
mkdir -p "$out"
# The locale's `copy` and `include` lines name files of the same
# directory, which I18NPATH points at.
I18NPATH="$PORT_SRC/localedata" localedef --no-archive \
	-i "$PORT_SRC/localedata/locales/C" \
	-f "$PORT_SRC/localedata/charmaps/UTF-8" \
	"$out/C.utf8"
echo "locales: C.UTF-8 in /system/libraries/locale/C.utf8"
