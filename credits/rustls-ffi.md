<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

Section: In every image
| [rustls-ffi](https://github.com/rustls/rustls-ffi) 0.15.7 by the rustls project, with [rustls-platform-verifier](https://github.com/rustls/rustls-platform-verifier) 0.7.1 and [rustls-native-certs](https://github.com/rustls/rustls-native-certs) 0.8.5 by the rustls project, [openssl-probe](https://crates.io/crates/openssl-probe) 0.2.1 by Alex Crichton, [log](https://github.com/rust-lang/log) 0.4.34 by the Rust project, [once_cell](https://github.com/matklad/once_cell) 1.21.4 by Aleksey Kladov and [macro_rules_attribute](https://github.com/danielhenrymantilla/macro_rules_attribute-rs) 0.2.3 by Daniel Henry-Mantilla, from crates.io, on the ring and rustls of the TLS row | rustls's C interface, which curl and libcurl do TLS through, in the static `librustls.a` ([ADR 0195](docs/adr/0195-curl-and-libcurl-use-rustls-through-rustls-ffi.md)) | Apache-2.0 OR ISC OR MIT (rustls-ffi, rustls-native-certs); MIT OR Apache-2.0 (rustls-platform-verifier, openssl-probe, log, once_cell); Apache-2.0 OR MIT OR Zlib (macro_rules_attribute) |
