#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Installs the patched crates (ADR 0080) into $TOOLCHAIN/rust/crates, each
# with the `[patch]` configuration that gives it to programs in place of
# crates.io's: $TOOLCHAIN/rust/patches/<name>-<version>.toml, which Cargo
# for Radix passes to Cargo for each crate a workspace's Cargo.lock names
# (ob(1), PORTS). `ob` runs it with PORT_SRC, where each crate is unpacked
# as <name>-<version> with its patch applied, and TOOLCHAIN set.
set -eu

crates="$TOOLCHAIN/rust/crates"
patches="$TOOLCHAIN/rust/patches"
rm -rf "$crates"
mkdir -p "$crates" "$patches"
for dir in "$PORT_SRC"/*/; do
	crate=$(basename "$dir")
	name=${crate%-*}
	version=${crate##*-}
	cp -R "$dir" "$crates/$crate"
	cat >"$patches/$crate.toml" <<END
# Written by ports/rust-crates: $name $version, patched for Radix.
[patch.crates-io.$name-$(echo "$version" | tr . _)]
package = "$name"
path = "$crates/$crate"
END
done
