#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds git for Radix (ob(1), PORTS) with its own Makefile, which builds
# in its source, and installs it into the folders Dukovo names
# (radix-files(7)): git in /system/commands, its helper programs and
# scripts in /system/libraries/git-core, as Debian keeps them in
# /usr/lib/git-core, the templates of a new repository in
# /system/resources/git-core, its manual pages, and the system's
# gitconfig among the default settings Spark puts in /system/settings.
#
# This is git for repositories on this computer (ADR 0287): it is built
# without curl, so without https:// remotes, which come with libcurl,
# and without Perl, Python, Tcl/Tk and gettext, which Dukovo does not
# have, so `git svn`, `git p4`, `git send-email`, gitk and git gui say
# they are not built in. Its Rust part, still optional, is left out.
# The built-in commands are the git program itself, without a git-<name>
# link each; only the three a remote runs by name have one.
set -eu

cd "$PORT_SRC"
# git's Makefile picks its settings by the system it is built on
# (config.mak.uname); it is told it builds for another, whose answers
# follow. mlibc has no iconv and no strlcpy, and its regcomp has no
# REG_STARTEND, so git brings its own.
make="make -j$JOBS CC=$CC AR=$AR CFLAGS=-O2 uname_S=Radix uname_O=Radix uname_M=x86_64
	uname_P=x86_64 uname_R=0 uname_V=0
	prefix=/system bindir=/system/commands gitexecdir=/system/libraries/git-core
	sharedir=/system/resources template_dir=/system/resources/git-core/templates
	mandir=/system/resources/man sysconfdir=/system/settings perllibdir=/nonexistent
	NO_PERL=YesPlease NO_PYTHON=YesPlease NO_TCLTK=YesPlease NO_GETTEXT=YesPlease
	NO_CURL=YesPlease NO_EXPAT=YesPlease NO_OPENSSL=YesPlease NO_ICONV=YesPlease
	NO_RUST=YesPlease NO_STRLCPY=YesPlease NO_REGEX=NeedsStartEnd NO_INSTALL_HARDLINKS=YesPlease
	HAVE_ALLOCA_H=YesPlease HAVE_PATHS_H=YesPlease HAVE_DEV_TTY=YesPlease
	HAVE_CLOCK_GETTIME=YesPlease HAVE_CLOCK_MONOTONIC=YesPlease HAVE_GETDELIM=YesPlease
	CSPRNG_METHOD=getrandom FREAD_READS_DIRECTORIES=UnfortunatelyYes
	SKIP_DASHED_BUILT_INS=YesPlease INSTALL_SYMLINKS=YesPlease NO_GITWEB=YesPlease"
# shellcheck disable=SC2086 # one word per setting
$make all >"$PORT_BUILD/make.log" 2>&1 || {
	tail -n 60 "$PORT_BUILD/make.log" >&2
	exit 1
}
stage="$PORT_BUILD/stage"
rm -rf "$stage"
# shellcheck disable=SC2086 # one word per setting
$make DESTDIR="$stage" install >"$PORT_BUILD/install.log" 2>&1 || {
	tail -n 40 "$PORT_BUILD/install.log" >&2
	exit 1
}

# What a system without Perl, Python, curl, a web server or a mail
# server has no use for: gitweb's CGI and its pages, the HTTP backend,
# imap-send, scalar and the CVS server; and bash's completion, as Nukesh completes git
# itself.
core=system/libraries/git-core
rm -rf "$stage/system/resources/gitweb" "$stage/system/share" \
	"$stage/system/commands/scalar" "$stage/$core/scalar" \
	"$stage/system/commands/git-cvsserver" "$stage/$core/git-cvsserver" \
	"$stage/$core/git-http-backend" "$stage/$core/git-imap-send" "$stage/$core/git-instaweb"

for program in system/commands/git system/commands/git-shell $core/git-daemon \
	$core/git-sh-i18n--envsubst; do
	$STRIP "$stage/$program"
done

# The manual pages, but not those of what is not built.
mkdir -p "$stage/system/resources/man"
cp -R "$PORT_SRC/manpages/man1" "$PORT_SRC/manpages/man5" "$PORT_SRC/manpages/man7" \
	"$stage/system/resources/man/"
for page in man1/gitk.1 man1/git-gui.1 man1/git-citool.1 man1/gitweb.1 man5/gitweb.conf.5 \
	man1/git-instaweb.1 man1/git-http-backend.1 man1/git-http-fetch.1 man1/git-http-push.1 \
	man1/git-imap-send.1 man1/scalar.1 man1/git-svn.1 man1/git-p4.1 man1/git-cvsimport.1 \
	man1/git-cvsexportcommit.1 man1/git-cvsserver.1 man1/git-archimport.1 \
	man1/git-send-email.1; do
	rm -f "$stage/system/resources/man/$page"
done

defaults="$stage/system/resources/default-settings"
mkdir -p "$defaults"
cp "$PORT_DIR/gitconfig" "$defaults/gitconfig"

(cd "$stage" && find . ! -type d | sed 's|^\./||' | sort) >"$PORT_INFO/image"
cp -R "$stage/." "$SYSROOT/"
