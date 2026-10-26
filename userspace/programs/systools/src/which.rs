// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! which(1): where in PATH each command is.

use std::ffi::{CString, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::args::{Arg, Args};
use crate::util::{self, Fail};

/// Whether `path` is a file this program may run.
fn executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() {
        return false;
    }
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: `c` is a C string.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Where `name` is: itself if it has a slash, else the first (or with
/// `all` every) directory of PATH that has it.
fn find(name: &OsString, path: &OsString, all: bool) -> Vec<PathBuf> {
    if name.as_bytes().contains(&b'/') {
        let p = PathBuf::from(name);
        return if executable(&p) { vec![p] } else { Vec::new() };
    }
    let mut found = Vec::new();
    for dir in std::env::split_paths(path) {
        // An empty entry is the working directory.
        let dir = if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir };
        let candidate = dir.join(name);
        if executable(&candidate) {
            found.push(candidate);
            if !all {
                break;
            }
        }
    }
    found
}

/// which(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut all = false;
    let mut silent = false;
    let mut names = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('a') => all = true,
            Arg::Long(l) if l == "all" => all = true,
            Arg::Short('s') => silent = true,
            Arg::Operand(name) => names.push(name),
            other => return Err(args.unknown(&other)),
        }
    }
    if names.is_empty() {
        return Err(util::fail(String::new()));
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/system/commands".into());
    let mut out = io::stdout().lock();
    let mut code = 0;
    for name in &names {
        let found = find(name, &path, all);
        if found.is_empty() {
            code = 1;
        }
        if !silent {
            for p in found {
                out.write_all(p.as_os_str().as_bytes())?;
                out.write_all(b"\n")?;
            }
        }
    }
    Ok(code)
}
