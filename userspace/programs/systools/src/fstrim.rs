// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! fstrim(8): has a mounted filesystem discard on its disks what it does
//! not use (ADR 0158), as util-linux's fstrim does with FITRIM, through
//! the directory interface's `trim` (radix-directory(2)).

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use radix_abi::directory;
use radix_abi::errors::{File as FileError, Kernel};

use crate::args::{Arg, Args};
use crate::df;
use crate::proc;
use crate::sys::{self, SYS};
use crate::util::{self, Fail, usage};

/// util-linux's exit statuses for `-a`: every filesystem failed, or some.
const ALL_FAILED: i32 = 32;
const SOME_FAILED: i32 = 64;

/// `bytes` as util-linux prints a size: `1.2 GiB`, `0 B`.
fn human(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        let text = format!("{value:.1}");
        format!("{} {}", text.strip_suffix(".0").unwrap_or(&text), units[unit])
    }
}

/// Trims the filesystem `path` is on: the bytes discarded.
fn trim(path: &Path, minimum: u64) -> Result<u64, String> {
    let dir = sys::open_control(path).map_err(|e| util::strip_os_error(&e))?;
    match directory::trim(&SYS, sys::slot_of(dir.as_raw_fd()), minimum) {
        Ok(results) => Ok(results.trimmed),
        Err(status) if Kernel::from_status(status) == Some(Kernel::NotSupported) => {
            Err("the discard operation is not supported".into())
        }
        Err(status) if FileError::from_status(status) == Some(FileError::ReadOnly) => {
            Err("cannot trim a read-only filesystem".into())
        }
        Err(status) => Err(util::strip_os_error(&sys::io_error(status))),
    }
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut all, mut verbose, mut quiet, mut minimum, mut path) =
        (false, false, false, 0, None::<PathBuf>);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('a' | 'A') => all = true,
            Arg::Long(long) if long == "all" || long == "fstab" => all = true,
            Arg::Short('v') => verbose = true,
            Arg::Long(long) if long == "verbose" => verbose = true,
            Arg::Short('q') => quiet = true,
            Arg::Long(long) if long == "quiet-unsupported" => quiet = true,
            Arg::Short('m') => minimum = minimum_of(&args.text()?)?,
            Arg::Long(long) if long == "minimum" => minimum = minimum_of(&args.text()?)?,
            Arg::Operand(operand) if path.is_none() => path = Some(PathBuf::from(operand)),
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    if !all {
        let Some(path) = path else { return Err(usage("no mountpoint given".into())) };
        return match trim(&path, minimum) {
            Ok(trimmed) => {
                if verbose {
                    writeln!(
                        out,
                        "{}: {} ({trimmed} bytes) trimmed",
                        path.display(),
                        human(trimmed)
                    )?;
                }
                Ok(0)
            }
            Err(why) => Err(util::fail(format!("{}: {why}", path.display()))),
        };
    }
    if path.is_some() {
        return Err(usage("a mountpoint and --all".into()));
    }
    // Every mount of a filesystem on a disk, each filesystem once.
    let mounts = proc::mounts().map_err(|e| util::failed(&"/processes/mounts", e))?;
    let (mut done, mut failed) = (0, 0);
    let mut seen: Vec<String> = Vec::new();
    for mount in &mounts {
        if crate::probe::block_text(&mount.source, "size").is_none() || mount.read_only {
            continue;
        }
        if seen.contains(&mount.source) {
            continue;
        }
        seen.push(mount.source.clone());
        let at = df::mount_path(mount);
        match trim(&at, minimum) {
            Ok(trimmed) => {
                done += 1;
                if verbose {
                    let (size, on) = (human(trimmed), &mount.source);
                    writeln!(
                        out,
                        "{}: {size} ({trimmed} bytes) trimmed on /devices/{on}",
                        at.display()
                    )?;
                }
            }
            Err(why) if why.starts_with("the discard") => {
                if !quiet {
                    eprintln!("fstrim: {}: {why}", at.display());
                }
            }
            Err(why) => {
                failed += 1;
                eprintln!("fstrim: {}: {why}", at.display());
            }
        }
    }
    Ok(match (done, failed) {
        (_, 0) => 0,
        (0, _) => ALL_FAILED,
        _ => SOME_FAILED,
    })
}

/// `-m`'s size.
fn minimum_of(text: &str) -> Result<u64, Fail> {
    util::parse_size(text).ok_or_else(|| usage(format!("invalid minimum: {text}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_and_print_as_util_linux_has_them() {
        assert_eq!(util::parse_size("4096"), Some(4096));
        assert_eq!(util::parse_size("1M"), Some(1 << 20));
        assert_eq!(util::parse_size("2MiB"), Some(2 << 20));
        assert_eq!(util::parse_size("1MB"), Some(1_000_000));
        assert_eq!(util::parse_size("1X"), None);
        assert_eq!(human(0), "0 B");
        assert_eq!(human(27_841_662_976), "25.9 GiB");
        assert_eq!(human(1 << 20), "1 MiB");
    }
}
