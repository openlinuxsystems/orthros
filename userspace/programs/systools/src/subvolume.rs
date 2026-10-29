// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! subvolume(8): makes, snapshots and removes Btrfs subvolumes, as
//! Linux's `btrfs subvolume` does with `create`, `snapshot` and `delete`
//! (ADR 0138), through the directory interface (radix-directory(2)).

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use radix_abi::directory;

use crate::args::{Arg, Args};
use crate::sys::{self, SYS};
use crate::util::{self, Fail};

/// The directory that holds `path`, open, and the last name of `path`.
fn place(path: &Path) -> Result<(File, String), Fail> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(util::fail(format!("{}: not a name for a subvolume", path.display())));
    };
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let dir = File::open(parent).map_err(|e| util::failed(&parent.display(), e))?;
    Ok((dir, name.to_string()))
}

/// The slot of the directory `dir`, opened.
fn slot(dir: &File) -> u32 {
    sys::slot_of(dir.as_raw_fd())
}

/// Makes a new, empty subvolume at `path`.
fn create(path: &Path) -> Result<(), Fail> {
    let (dir, name) = place(path)?;
    directory::make_subvolume(&SYS, slot(&dir), name.as_bytes())
        .map_err(|s| util::failed(&path.display(), sys::io_error(s)))?;
    let _ = writeln!(io::stdout(), "Create subvolume '{}'", path.display());
    Ok(())
}

/// Makes a snapshot of the subvolume that holds `source` at `dest`, or
/// in `dest` with `source`'s name if it is a directory, read-only if
/// `read_only`.
fn snapshot(source: &Path, dest: &Path, read_only: bool) -> Result<(), Fail> {
    let from = File::open(source).map_err(|e| util::failed(&source.display(), e))?;
    let (dir, name, at) = if dest.is_dir() {
        let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
            return Err(util::fail(format!("{}: name the snapshot", source.display())));
        };
        let dir = File::open(dest).map_err(|e| util::failed(&dest.display(), e))?;
        (dir, name.to_string(), dest.join(name))
    } else {
        let (dir, name) = place(dest)?;
        (dir, name, dest.to_path_buf())
    };
    directory::snapshot(&SYS, slot(&from), b".", slot(&dir), name.as_bytes(), read_only)
        .map_err(|s| util::failed(&at.display(), sys::io_error(s)))?;
    let kind = if read_only { "a readonly snapshot" } else { "a snapshot" };
    let _ = writeln!(io::stdout(), "Create {kind} of '{}' in '{}'", source.display(), at.display());
    Ok(())
}

/// Removes the subvolume at `path`, with everything in it.
fn delete(path: &Path) -> Result<(), Fail> {
    let (dir, name) = place(path)?;
    directory::remove_subvolume(&SYS, slot(&dir), name.as_bytes())
        .map_err(|s| util::failed(&path.display(), sys::io_error(s)))?;
    let _ = writeln!(io::stdout(), "Delete subvolume '{}'", path.display());
    Ok(())
}

/// subvolume(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let usage = || util::usage("say create, snapshot or delete, and the paths".into());
    let Some(action) = args.subcommand() else { return Err(usage()) };
    let mut read_only = false;
    let mut paths = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('r') if action == "snapshot" => read_only = true,
            Arg::Operand(path) => paths.push(PathBuf::from(path)),
            other => return Err(args.unknown(&other)),
        }
    }
    match (action.to_str(), paths.as_slice()) {
        (Some("create"), paths) if !paths.is_empty() => {
            for path in paths {
                create(path)?;
            }
        }
        (Some("snapshot"), [source, dest]) => snapshot(source, dest, read_only)?,
        (Some("delete"), paths) if !paths.is_empty() => {
            for path in paths {
                delete(path)?;
            }
        }
        _ => return Err(usage()),
    }
    Ok(0)
}
