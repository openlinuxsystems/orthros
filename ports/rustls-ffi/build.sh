#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds rustls-ffi's static library for Radix with Cargo for Radix
# ($CARGO_RADIX, ob(1), PORTS), on ring rather than aws-lc-rs, its
# default, and installs it as librustls.a with rustls.h and rustls.pc,
# the names its own installer (cargo-c) gives them. The library holds
# Rust's standard library and everything else it needs but the C
# library. Lists the crates it links for the license manifest.
set -eu

features=ring,capi
cd "$PORT_SRC"
"$CARGO_RADIX" build --release -p rustls-ffi --no-default-features --features "$features" \
	--target-dir "$PORT_BUILD" --jobs "$JOBS"

lib="$SYSROOT/system/libraries"
mkdir -p "$lib/pkgconfig" "$SYSROOT/system/include"
cp "$PORT_BUILD/x86_64-unknown-radix/release/librustls_ffi.a" "$lib/librustls.a"
cp librustls/src/rustls.h "$SYSROOT/system/include/rustls.h"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' librustls/Cargo.toml | head -n 1)
cat >"$lib/pkgconfig/rustls.pc" <<PC
prefix=/system
libdir=\${prefix}/libraries
includedir=\${prefix}/include

Name: rustls
Description: rustls's C interface, on ring
Version: $version
License: Apache-2.0 OR ISC OR MIT
Libs: -L\${libdir} -lrustls
Cflags: -I\${includedir}
PC

# What the library links, for the license manifest (ADR 0021).
"$CARGO_RADIX" tree -p rustls-ffi --no-default-features --features "$features" \
	-e normal,no-proc-macro --prefix none --format '{p}|{l}' >"$PORT_INFO/crates.txt"
cp Cargo.lock "$PORT_INFO/Cargo.lock"
