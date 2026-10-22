#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds Rust's standard library for Radix (ADR 0070): the patched library
# of rust-src, as Rust's own build makes it (the sysroot crate, with
# backtraces and unwinding), offline from the crates rust-src vendors.
# Installs it as a sysroot for the x86_64-unknown-radix target in
# $TOOLCHAIN/rust, which ob builds programs in Rust against, with the
# patched libc crate beside it, which ob gives those programs in place of
# crates.io's. `ob` runs it (ob(1), PORTS) with PORT_SRC, PORT_BUILD,
# TOOLCHAIN and RUST_TARGET set.
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
