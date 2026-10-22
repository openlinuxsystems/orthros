// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The `ridlc` command: compiles the `.ridl` files in a directory and
//! writes the Rust bindings, the C headers and the man pages.
//!
//! ```text
//! ridlc ABI_DIR [--rust FILE] [--c INCLUDE_DIR] [--man MAN_DIR]
//! ```
//!
//! With no output named it only checks the files. `ob build` and `ob docs`
//! use it as a library instead; this command is for working on the files.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    let (mut rust, mut c, mut man) = (None, None, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--rust" => rust = args.next().map(PathBuf::from),
            "--c" => c = args.next().map(PathBuf::from),
            "--man" => man = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("usage: ridlc ABI_DIR [--rust FILE] [--c INCLUDE_DIR] [--man MAN_DIR]");
                return ExitCode::SUCCESS;
            }
            _ if dir.is_none() => dir = Some(PathBuf::from(arg)),
            other => {
                eprintln!("ridlc: unexpected argument `{other}`");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(dir) = dir else {
        eprintln!("usage: ridlc ABI_DIR [--rust FILE] [--c INCLUDE_DIR] [--man MAN_DIR]");
        return ExitCode::FAILURE;
    };
    let abi = match ridlc::compile_dir(&dir) {
        Ok(abi) => abi,
        Err(errors) => {
            for error in errors {
                eprintln!("ridlc: {error}");
            }
            return ExitCode::FAILURE;
        }
    };
    let mut outputs = Vec::new();
    if let Some(path) = rust {
        outputs.push((path, ridlc::rust(&abi)));
    }
    if let Some(dir) = c {
        outputs
            .extend(ridlc::c_headers(&abi).into_iter().map(|(name, text)| (dir.join(name), text)));
    }
    if let Some(dir) = man {
        outputs
            .extend(ridlc::man_pages(&abi).into_iter().map(|(name, text)| (dir.join(name), text)));
    }
    for (path, text) in outputs {
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            eprintln!("ridlc: cannot create {}: {err}", parent.display());
            return ExitCode::FAILURE;
        }
        if let Err(err) = std::fs::write(&path, text) {
            eprintln!("ridlc: cannot write {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
