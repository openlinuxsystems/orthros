// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Links the programs with the user-program linker script.

fn main() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../link.ld");
    println!("cargo::rerun-if-changed={}", script.display());
    println!("cargo::rustc-link-arg=-T{}", script.display());
}
