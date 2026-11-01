// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Passes the smoke boot's linker script to the linker, for bare-metal
//! RISC-V builds of the program (ADR 0270).

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    println!("cargo:rerun-if-changed=build.rs");
    if target_os == "none" && target_arch == "riscv64" {
        let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
        println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
        println!("cargo:rerun-if-changed=linker.ld");
    }
}
