#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds uutils' core utilities for Radix as one program,
# /system/bin/coreutils, with Cargo for Radix ($CARGO_RADIX, ob(1),
# PORTS), a symbolic link to it by each utility's name, which it runs as,
# and their manual pages with uutils' own generator, built for the host.
# Lists the crates the program links for the license manifest.
set -eu

# The utilities, each a Cargo feature of the coreutils package.
utilities="
	[ arch b2sum base32 base64 basename basenc cat cksum comm cp csplit cut
	date dd dir dircolors dirname du echo env expand expr factor false fmt
	fold groups head hostname id install join link ln logname ls md5sum
	mkdir mkfifo mknod mktemp more mv nice nl nohup nproc numfmt od paste
	pathchk pr printenv printf ptx pwd readlink realpath rm rmdir seq
	sha1sum sha224sum sha256sum sha384sum sha512sum shred shuf sleep sort
	split stat stty sum sync tac tail tee test timeout touch tr true
	truncate tsort tty uname unexpand uniq unlink vdir wc whoami yes
"
# `[` is `test`'s other name, not a feature of its own.
features=$(printf '%s\n' $utilities | grep -vx '\[' | paste -sd, -)

cd "$PORT_SRC"
"$CARGO_RADIX" build --release --no-default-features --features "$features" \
	--bin coreutils --target-dir "$PORT_BUILD/radix" --jobs "$JOBS"
mkdir -p "$SYSROOT/system/bin"
$STRIP -o "$SYSROOT/system/bin/coreutils" "$PORT_BUILD/radix/x86_64-unknown-radix/release/coreutils"
for utility in $utilities; do
	ln -sfn coreutils "$SYSROOT/system/bin/$utility"
	echo "system/bin/$utility"
done >"$PORT_INFO/image"

# A manual page for each utility, from its own argument definitions.
unset CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_TARGET
cargo build --release --no-default-features --features "uudoc,$features" --bin uudoc \
	--target-dir "$PORT_BUILD/host" --jobs "$JOBS"
# The generator dates each page today; the commit's date makes the same
# pages each build.
man="$SYSROOT/system/share/man/man1"
date=$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%d)
mkdir -p "$man"
for utility in $utilities; do
	"$PORT_BUILD/host/release/uudoc" manpage "$utility" |
		sed "1,/^\.TH /s/^\(\.TH [^ ]* 1\) \"[0-9-]*\"/\1 \"$date\"/" >"$man/$utility.1"
	echo "system/share/man/man1/$utility.1"
done >>"$PORT_INFO/image"

# What the program links, for the license manifest (ADR 0021): not the
# procedural macros, which run while it builds and are not in it.
"$CARGO_RADIX" tree --no-default-features --features "$features" -e normal,no-proc-macro \
	--prefix none --format '{p}|{l}' >"$PORT_INFO/crates.txt"
cp Cargo.lock "$PORT_INFO/Cargo.lock"
