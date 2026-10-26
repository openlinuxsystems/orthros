#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# x86_64-radix-configure SOURCE [ARGUMENT...]: runs SOURCE's GNU
# configure script for Radix, in the current directory, with the
# arguments after it (ob(1), PORTS). ob writes it into the toolchain, for
# a port's build.sh to run with the environment ob gives that.
#
# config.sub does not know Radix yet, so each config.sub in SOURCE is
# first put behind a script that names Radix and hands every other name
# to it. The build machine is given, so that configure knows it is cross
# compiling: a Radix program may well start on Linux, which runs another
# system's programs as its own, and do what its system calls mean there.
# CONFIG_SITE holds the answers configure cannot find out without
# running a program for Radix (config.site, beside this script's
# directory). CONFIG_SHELL keeps configure from choosing the host's bash
# for the scripts a package installs, which Radix runs with /bin/sh.
# A package installs into the folders Dukovo names (radix-files(7)):
# programs in commands, libraries in libraries, its other files in
# resources, settings in settings and changing data in state, under
# the prefix.
set -eu

src=$1
shift
toolchain=$(cd "$(dirname "$0")/.." && pwd)

find "$src" -name config.sub -type f | while IFS= read -r sub; do
	[ -e "$sub.upstream" ] && continue
	mv "$sub" "$sub.upstream"
	cat >"$sub" <<'EOF'
#!/bin/sh
# Written by x86_64-radix-configure: Radix's names, then config.sub's.
case ${1-} in
x86_64-radix | x86_64-*-radix) echo x86_64-pc-radix ;;
*) exec "$0.upstream" "$@" ;;
esac
EOF
	chmod +x "$sub"
done

# ob builds on x86-64 Linux only (ob(1)), which is what a package without
# config.guess, since it never asks configure for the build machine's
# name, is told.
guess=$(find "$src" -name config.guess -type f | head -n 1)
if [ -n "$guess" ]; then
	build=$(sh "$guess")
else
	build=x86_64-pc-linux-gnu
fi
CONFIG_SITE=$toolchain/autotools/config.site
CONFIG_SHELL=/bin/sh
export CONFIG_SITE CONFIG_SHELL
prefix=${PREFIX:-/system}
exec /bin/sh "$src/configure" --build="$build" --host=x86_64-radix --prefix="$prefix" \
	--bindir="$prefix/commands" --sbindir="$prefix/commands" --libdir="$prefix/libraries" \
	--libexecdir="$prefix/libraries" --datarootdir="$prefix/resources" \
	--sysconfdir="$prefix/settings" --localstatedir="$prefix/state" \
	CC="$CC" AR="$AR" RANLIB="$RANLIB" NM="$NM" STRIP="$STRIP" CFLAGS="$CFLAGS" "$@"
