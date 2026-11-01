#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds zic with the build host's C compiler and compiles the time zone
# database with it into /system/resources/zoneinfo: a TZif file for each
# zone, slim, as zic writes them by default (version 4, 64-bit
# transitions and a TZ string for what follows the last), and each of
# the database's links to a zone, such as US/Eastern to America/New_York,
# as a symbolic link, which the boot archive keeps (zic's hard links
# would be copies there). Beside them go the tables that list the zones
# by country, which timedatectl(1) reads, and tzdata.zi, the whole
# database as text. `ob` runs it (ob(1), PORTS) with PORT_SRC,
# PORT_BUILD and SYSROOT set.
set -eu

cp -R "$PORT_SRC/." "$PORT_BUILD/"
cd "$PORT_BUILD"
make -s CC=cc VERSION=2026f zic tzdata.zi

out="$SYSROOT/system/resources/zoneinfo"
rm -rf "$out"
mkdir -p "$out"
./zic -b slim -d "$out" tzdata.zi
# tzdata.zi's links are lines `L TARGET NAME`.
grep '^L ' tzdata.zi | while read -r _ target name; do
	rm "$out/$name"
	case $name in
	*/*) up=$(dirname "$name" | sed 's|[^/][^/]*|..|g')/ ;;
	*) up= ;;
	esac
	ln -s "$up$target" "$out/$name"
done
cp tzdata.zi zone1970.tab zone.tab iso3166.tab "$out/"
echo "tzdata: $(grep -c '^Z ' tzdata.zi) time zones and $(grep -c '^L ' tzdata.zi) links to them in /system/resources/zoneinfo"
