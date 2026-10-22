// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Links the test programs with the user-program linker script, at the
//! fixed addresses it gives: the one image linked that way, so that the
//! kernel's loader keeps running fixed images too. (The target builds
//! static position-independent executables, which `-no-pie` turns off.)

fn main() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../link.ld");
    println!("cargo::rerun-if-changed={}", script.display());
    println!("cargo::rustc-link-arg=-T{}", script.display());
    println!("cargo::rustc-link-arg=-no-pie");
}
