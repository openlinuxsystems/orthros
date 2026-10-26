#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds ncurses for Radix (ob(1), PORTS): the static library, with wide
# characters, as libncurses.a (and libcurses.a, libtinfo.a and the
# libncursesw.a that programs ask for under other names), its headers and
# pkg-config files in the sysroot for the ports built on it, and its
# programs and their manual pages for the image. Also builds tic for the
# build machine from the same source, to compile the terminal
# descriptions the image holds.
set -eu

# Runs a command with its output in a log, shown only if it fails.
quiet() {
	"$@" >"$PORT_BUILD/log" 2>&1 || {
		tail -n 60 "$PORT_BUILD/log" >&2
		return 1
	}
}

# tic for the build machine: the C library only, and tic.
mkdir -p "$PORT_BUILD/host"
cd "$PORT_BUILD/host"
quiet env CC=cc "$PORT_SRC/configure" --without-shared --without-debug --without-ada \
	--without-cxx --without-cxx-binding --without-tests --without-manpages \
	--disable-db-install
quiet make -j"$JOBS" -C include
quiet make -j"$JOBS" -C ncurses
quiet make -j"$JOBS" -C progs tic

# configure does not know Radix, so it would ask the C library for no
# more than POSIX; _GNU_SOURCE gets what it asks of glibc on Linux,
# whose interface Radix's C library follows (radix-posix(7)).
mkdir -p "$PORT_BUILD/radix"
cd "$PORT_BUILD/radix"
quiet x86_64-radix-configure "$PORT_SRC" CPPFLAGS=-D_GNU_SOURCE --with-build-cc=cc \
	--without-shared --with-normal --without-debug --without-ada \
	--without-cxx --without-cxx-binding --without-tests \
	--enable-widec --disable-lib-suffixes --enable-overwrite \
	--enable-ext-colors --enable-ext-mouse --enable-sigwinch \
	--with-default-terminfo-dir=/system/resources/terminfo \
	--with-terminfo-dirs=/system/resources/terminfo \
	--disable-db-install --disable-stripping \
	--enable-pc-files --with-pkg-config-libdir=/system/libraries/pkgconfig \
	--with-manpage-format=normal --without-manpage-renames
quiet make -j"$JOBS"
quiet make install DESTDIR="$SYSROOT"
for name in ncursesw curses tinfo; do
	ln -sf libncurses.a "$SYSROOT/system/libraries/lib$name.a"
done
ln -sf ncurses.pc "$SYSROOT/system/libraries/pkgconfig/ncursesw.pc"

# The programs without their symbols, and their manual pages: ncurses
# names those of tic and infocmp, which it counts as administration
# programs, tic.1m and infocmp.1m. reset is tset under another name.
: >"$PORT_INFO/image"
for program in clear infocmp reset tic tput tset; do
	[ -L "$SYSROOT/system/commands/$program" ] || $STRIP "$SYSROOT/system/commands/$program"
	echo "system/commands/$program" >>"$PORT_INFO/image"
	for page in "$SYSROOT/system/resources/man/man1/$program".1*; do
		echo "system/resources/man/man1/${page##*/}" >>"$PORT_INFO/image"
	done
done
for page in man5/terminfo.5 man5/term.5 man5/user_caps.5 man7/term.7; do
	echo "system/resources/man/$page" >>"$PORT_INFO/image"
done

# The terminals Dukovo is used from: its own console and serial line,
# terminal emulators and multiplexers.
terminals="ansi dumb linux linux-16color vt52 vt100 vt102 vt220 sun cons25
	xterm xterm-new xterm-r6 xterm-color xterm-256color xterm-direct xterm-mono
	screen screen-256color screen.xterm-256color tmux tmux-256color tmux-direct
	alacritty contour cygwin foot gnome gnome-256color iterm2 kitty konsole
	konsole-256color mintty ms-terminal putty putty-256color rxvt st-256color
	vte vte-256color wezterm"
terminfo="$SYSROOT/system/resources/terminfo"
rm -rf "$terminfo"
mkdir -p "$terminfo"
# shellcheck disable=SC2086 # one word per terminal
"$PORT_BUILD/host/progs/tic" -x -o "$terminfo" -e "$(echo $terminals | tr ' ' ,)" \
	"$PORT_SRC/misc/terminfo.src"
