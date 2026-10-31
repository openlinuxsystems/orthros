// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Finds the boot tests in src/test/cases, one file each, so that no
//! list names them: two pull requests that each add a test add two files
//! and touch no line in common.

use std::fmt::Write;
use std::path::Path;

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/test/cases");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("reading src/test/cases")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?.strip_suffix(".rs")?.to_string();
            println!("cargo:rerun-if-changed={}", path.display());
            Some(name)
        })
        .collect();
    names.sort();
    let mut out = String::new();
    for name in &names {
        let module = name.replace('-', "_");
        let path = dir.join(format!("{name}.rs"));
        writeln!(out, "#[path = {:?}]\nmod {module};", path.display().to_string()).unwrap();
    }
    out.push_str("/// The cases of src/test/cases: each file's name, case and text.\n");
    out.push_str("pub const FILES: &[(&str, &Case, &str)] = &[\n");
    for name in &names {
        let module = name.replace('-', "_");
        let path = dir.join(format!("{name}.rs"));
        writeln!(
            out,
            "    ({name:?}, &{module}::CASE, include_str!({:?})),",
            path.display().to_string()
        )
        .unwrap();
    }
    out.push_str("];\n");
    let target = Path::new(&std::env::var("OUT_DIR").unwrap()).join("cases.rs");
    std::fs::write(target, out).unwrap();
}
