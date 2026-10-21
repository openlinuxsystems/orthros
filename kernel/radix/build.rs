// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Passes the architecture's linker script to the linker for bare-metal
//! builds, and asks it to keep the relocations, from which `ob build`
//! makes the table that moves the kernel at boot (KASLR, `radix-relocs`).

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os == "none" && target_arch == "x86_64" {
        let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
        println!("cargo:rustc-link-arg-bins=-T{dir}/../arch/x86_64/linker.ld");
        println!("cargo:rustc-link-arg-bins=--emit-relocs");
        println!("cargo:rerun-if-changed=../arch/x86_64/linker.ld");
    }
}
