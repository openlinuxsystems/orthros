// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Generates the bindings from `abi/*.ridl` with ridlc.

use std::path::Path;

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../abi");
    println!("cargo::rerun-if-changed={}", dir.display());
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            println!("cargo::rerun-if-changed={}", entry.path().display());
        }
    }
    let abi = match ridlc::compile_dir(&dir) {
        Ok(abi) => abi,
        Err(errors) => {
            for error in &errors {
                eprintln!("ridlc: {error}");
            }
            panic!("abi/*.ridl has {} problems", errors.len());
        }
    };
    let out = Path::new(&std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR")).join("abi.rs");
    std::fs::write(&out, ridlc::rust(&abi)).expect("cannot write the bindings");
}
