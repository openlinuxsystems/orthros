#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds Rust's standard library for Radix (ADR 0070): the patched library
# of rust-src, as Rust's own build makes it (the sysroot crate, with
# backtraces and unwinding), offline from the crates rust-src vendors.
# Installs it as a sysroot for the x86_64-unknown-radix target in
# $TOOLCHAIN/rust, which ob builds programs in Rust against, with the
# patched libc crate beside it, which ob gives those programs in place of
# crates.io's, and what the license manifest needs to list the crates such
# a program links. `ob` runs it (ob(1), PORTS) with PORT_SRC, PORT_BUILD,
# TOOLCHAIN and RUST_TARGET set, and then checks the libc crate against
# the C library's headers.
set -eu

cd "$PORT_SRC"
# The patches change vendored crates, whose files no longer match the
# checksums Cargo was given for them.
for crate in vendor/libc-0.2.189 vendor/unwinding-0.2.10; do
	sed -i 's/"files":{[^}]*}/"files":{}/' "$crate/.cargo-checksum.json"
done

# Flags meant for other builds must not reach this one.
unset CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_TARGET CARGO_TARGET_DIR
target=x86_64-unknown-radix
RUSTFLAGS=-Zforce-unstable-if-unmarked "${CARGO:-cargo}" build --offline --release \
	-p sysroot --features backtrace --target "$RUST_TARGET" -Zjson-target-spec \
	--target-dir "$PORT_BUILD/target" --jobs "$JOBS"

sysroot="$TOOLCHAIN/rust"
lib="$sysroot/lib/rustlib/$target/lib"
rm -rf "$sysroot"
mkdir -p "$lib" "$sysroot/vendor"
# Each crate's library and its metadata, which Cargo keeps apart.
find "$PORT_BUILD/target/$target/release" \( -name '*.rlib' -o -name '*.rmeta' \) \
	-exec cp {} "$lib/" \;
twice=$(ls "$lib" | sed 's/-[0-9a-f]*\.r[a-z]*$//' | sort | uniq -c | awk '$1 > 2')
if [ -n "$twice" ]; then
	echo "rust-std: crates built more than once: $twice" >&2
	exit 1
fi
cp -r vendor/libc-0.2.189 "$sysroot/vendor/libc"

# The crates a program on it links, one "name vX.Y.Z (source)|license" line
# each, and the notice files of those rust-src vendors, for the license
# manifest (ADR 0021).
RUSTFLAGS=-Zforce-unstable-if-unmarked "${CARGO:-cargo}" tree --offline -p std \
	--features backtrace,panic-unwind -e normal --target "$RUST_TARGET" -Zjson-target-spec \
	--prefix none --format '{p}|{l}' | sed 's/ (\*)$//' | sort -u >"$sysroot/crates.txt"
sed -n 's/^\([^ ]*\) v\([^ |]*\)|.*/\1-\2/p' "$sysroot/crates.txt" | while read -r crate; do
	[ -d "vendor/$crate" ] || continue
	mkdir -p "$sysroot/notices/$crate"
	find "vendor/$crate" -maxdepth 1 -type f \( -name 'LICENSE*' -o -name 'LICENCE*' \
		-o -name 'COPYING*' -o -name 'COPYRIGHT*' -o -name 'NOTICE*' \) \
		-exec cp {} "$sysroot/notices/$crate/" \;
done
