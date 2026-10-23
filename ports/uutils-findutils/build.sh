#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds find and xargs of uutils' findutils for Radix with Cargo for
# Radix ($CARGO_RADIX, ob(1), PORTS), and their manual pages, which
# Dukovo writes in scdoc, as findutils has none. Lists the crates the
# programs link for the license manifest.
set -eu

cd "$PORT_SRC"
"$CARGO_RADIX" build --release --bin find --bin xargs --target-dir "$PORT_BUILD/radix" \
	--jobs "$JOBS"
mkdir -p "$SYSROOT/system/bin" "$SYSROOT/system/share/man/man1"
for program in find xargs; do
	$STRIP -o "$SYSROOT/system/bin/$program" \
		"$PORT_BUILD/radix/x86_64-unknown-radix/release/$program"
	scdoc <"$PORT_DIR/man/$program.1.scd" >"$SYSROOT/system/share/man/man1/$program.1"
done

# What the programs link, for the license manifest (ADR 0021): not the
# procedural macros, which run while they build and are not in them.
"$CARGO_RADIX" tree -e normal,no-proc-macro --prefix none --format '{p}|{l}' \
	>"$PORT_INFO/crates.txt"
cp Cargo.lock "$PORT_INFO/Cargo.lock"
