#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds mandoc for Radix (ob(1), PORTS): mandoc, with man, apropos,
# whatis and makewhatis as links to it, demandoc and soelim, and their
# manual pages and those of the manual page languages.
#
# mandoc's configure finds out what the C library has by building small
# programs and running them, which cannot be done for Radix here, so
# configure.local answers every question for it.
set -eu

cd "$PORT_SRC"
cat >configure.local <<EOT
OSNAME="Dukovo"
OSENUM=MANDOC_OS_OTHER
PREFIX=/system
BINDIR=/system/bin
SBINDIR=/system/bin
MANDIR=/system/share/man
MANPATH_BASE=/system/share/man
MANPATH_DEFAULT=/system/share/man
UTF8_LOCALE=C.UTF-8
BINM_PAGER=less
LN="ln -sf"
CC="$CC"
AR="$AR"
CFLAGS="$CFLAGS -g0"
NEED_GNU_SOURCE=1
HAVE_WFLAG=1
HAVE_STATIC=0
HAVE_LESS_T=1
HAVE_WCHAR=1

# What mlibc has.
HAVE_ATTRIBUTE=1
HAVE_CMSG=1
HAVE_ENDIAN=1
HAVE_ERR=1
HAVE_GETLINE=1
HAVE_GETSUBOPT=1
HAVE_ISBLANK=1
HAVE_MKDTEMP=1
HAVE_MKSTEMPS=1
HAVE_NANOSLEEP=1
HAVE_NTOHL=1
HAVE_O_DIRECTORY=1
HAVE_PATH_MAX=1
HAVE_REALLOCARRAY=1
HAVE_RECVMSG=1
HAVE_REWB_SYSV=1
HAVE_STRCASESTR=1
HAVE_STRLCAT=1
HAVE_STRLCPY=1
HAVE_STRNDUP=1
HAVE_STRPTIME=1
HAVE_STRSEP=1
HAVE_VASPRINTF=1

# What it has not, which mandoc brings its own of or does without.
HAVE_DIRENT_NAMLEN=0
HAVE_EFTYPE=0
HAVE_FTS=0
HAVE_OHASH=0
HAVE_PLEDGE=0
HAVE_PROGNAME=0
HAVE_RECALLOCARRAY=0
HAVE_REWB_BSD=0
HAVE_SANDBOX_INIT=0
HAVE_STRINGLIST=0
HAVE_STRTONUM=0
HAVE_SYS_ENDIAN=0
EOT
./configure >"$PORT_BUILD/configure.log" 2>&1 || {
	cat "$PORT_BUILD/configure.log" >&2
	exit 1
}
make -j"$JOBS" >"$PORT_BUILD/make.log" 2>&1 || {
	tail -n 60 "$PORT_BUILD/make.log" >&2
	exit 1
}
make base-install DESTDIR="$SYSROOT" >/dev/null
$STRIP "$SYSROOT/system/bin/mandoc" "$SYSROOT/system/bin/demandoc" "$SYSROOT/system/bin/soelim"

: >"$PORT_INFO/image"
for program in mandoc man apropos whatis makewhatis demandoc soelim; do
	echo "system/bin/$program" >>"$PORT_INFO/image"
done
for page in man1/mandoc.1 man1/demandoc.1 man1/soelim.1 man1/man.1 man1/apropos.1 \
	man1/whatis.1 man5/man.conf.5 man5/mandoc.db.5 man7/man.7 man7/mdoc.7 \
	man7/roff.7 man7/eqn.7 man7/tbl.7 man7/mandoc_char.7 man8/makewhatis.8; do
	echo "system/share/man/$page" >>"$PORT_INFO/image"
done
