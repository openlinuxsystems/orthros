// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! df(1): the space of each mounted filesystem, from `/proc/mounts` and
//! statvfs(3), with GNU df's options.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::args::{Arg, Args};
use crate::proc::{self, Mount};
use crate::ps::print_table;
use crate::util::{self, Fail, usage};

/// What statvfs says of a filesystem, in bytes and files.
#[derive(Clone, Copy, Default)]
struct Space {
    size: u64,
    free: u64,
    available: u64,
    files: u64,
    files_free: u64,
}

fn statvfs(path: &OsStr) -> io::Result<Space> {
    let path =
        CString::new(path.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut st: libc::statvfs = util::unsafe_zeroed();
    // SAFETY: the path is a C string and `st` a statvfs for the call.
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let unit = if st.f_frsize > 0 { st.f_frsize } else { st.f_bsize };
    Ok(Space {
        size: st.f_blocks * unit,
        free: st.f_bfree * unit,
        available: st.f_bavail * unit,
        files: st.f_files,
        files_free: st.f_ffree,
    })
}

/// The kinds of filesystem that hold no files of their own, which df
/// leaves out unless `-a` is given.
const PSEUDO: &[&str] = &["proc", "sys", "dev"];

/// How sizes are shown.
#[derive(Clone, Copy)]
enum Unit {
    Blocks(u64),
    Human { si: bool },
}

/// df(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut all = false;
    let mut unit = Unit::Blocks(1024);
    let mut inodes = false;
    let mut kinds = false;
    let mut only: Vec<String> = Vec::new();
    let mut except: Vec<String> = Vec::new();
    let mut total = false;
    let mut posix = false;
    let mut files: Vec<OsString> = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('a') => all = true,
            Arg::Long(l) if l == "all" => all = true,
            Arg::Short('h') => unit = Unit::Human { si: false },
            Arg::Long(l) if l == "human-readable" => unit = Unit::Human { si: false },
            Arg::Short('H') => unit = Unit::Human { si: true },
            Arg::Long(l) if l == "si" => unit = Unit::Human { si: true },
            Arg::Short('k') => unit = Unit::Blocks(1024),
            Arg::Short('m') => unit = Unit::Blocks(1 << 20),
            Arg::Short('B') => unit = Unit::Blocks(block_size(&args.text()?)?),
            Arg::Long(l) if l == "block-size" => unit = Unit::Blocks(block_size(&args.text()?)?),
            Arg::Short('i') => inodes = true,
            Arg::Long(l) if l == "inodes" => inodes = true,
            Arg::Short('T') => kinds = true,
            Arg::Long(l) if l == "print-type" => kinds = true,
            Arg::Short('t') => only.push(args.text()?),
            Arg::Long(l) if l == "type" => only.push(args.text()?),
            Arg::Short('x') => except.push(args.text()?),
            Arg::Long(l) if l == "exclude-type" => except.push(args.text()?),
            Arg::Long(l) if l == "total" => total = true,
            Arg::Short('P') => posix = true,
            Arg::Long(l) if l == "portability" => posix = true,
            // Every filesystem is local, and statvfs never syncs.
            Arg::Short('l') => {}
            Arg::Long(l) if l == "local" || l == "sync" || l == "no-sync" => {}
            Arg::Operand(file) => files.push(file),
            other => return Err(args.unknown(&other)),
        }
    }
    let mounts = proc::mounts().map_err(|e| util::failed(&"/proc/mounts", e))?;
    // Each file given names the mount it is on: the longest mount path it
    // starts with.
    let mut chosen: Vec<Mount> = Vec::new();
    let mut code = 0;
    if files.is_empty() {
        chosen = mounts
            .iter()
            .filter(|m| all || !PSEUDO.contains(&m.kind.as_str()))
            .filter(|m| only.is_empty() || only.contains(&m.kind))
            .filter(|m| !except.contains(&m.kind))
            .cloned()
            .collect();
    } else {
        for file in &files {
            let path = match std::fs::canonicalize(file) {
                Ok(path) => path,
                Err(e) => {
                    eprintln!("df: {}: {}", Path::new(file).display(), util::strip_os_error(&e));
                    code = 1;
                    continue;
                }
            };
            if let Some(m) = mount_of(&mounts, &path) {
                chosen.push(m.clone());
            }
        }
    }
    let size_header = match (unit, posix) {
        (Unit::Human { .. }, _) => "Size".to_string(),
        (Unit::Blocks(bytes), true) => format!("{bytes}-blocks"),
        (Unit::Blocks(bytes), _) => format!("{}-blocks", block_name(bytes)),
    };
    let mut header = vec!["Filesystem".to_string()];
    if kinds {
        header.push("Type".into());
    }
    if inodes {
        header.extend(["Inodes", "IUsed", "IFree", "IUse%"].map(String::from));
    } else {
        let avail = if matches!(unit, Unit::Human { .. }) { "Avail" } else { "Available" };
        let used = if posix && !matches!(unit, Unit::Human { .. }) { "Capacity" } else { "Use%" };
        header.extend([size_header, "Used".into(), avail.into(), used.into()]);
    }
    header.push("Mounted on".into());
    // GNU df's narrowest columns: the source 14 wide, the kind 4, the
    // numbers 5 and the percentages 4.
    let last = header.len() - 1;
    let header: Vec<String> = header
        .into_iter()
        .enumerate()
        .map(|(i, cell)| match i {
            0 => format!("{cell:<14}"),
            1 if kinds => format!("{cell:<4}"),
            _ if i == last => cell,
            _ if cell.ends_with('%') || cell == "Capacity" => format!("{cell:>4}"),
            _ => format!("{cell:>5}"),
        })
        .collect();
    let mut table = vec![header];
    let mut sum = Space::default();
    for m in &chosen {
        let path = mount_path(m);
        let space = match statvfs(path.as_os_str()) {
            Ok(space) => space,
            Err(e) => {
                eprintln!("df: {}: {}", path.display(), util::strip_os_error(&e));
                code = 1;
                continue;
            }
        };
        if !all && files.is_empty() && space.size == 0 && space.files == 0 {
            continue;
        }
        sum.size += space.size;
        sum.free += space.free;
        sum.available += space.available;
        sum.files += space.files;
        sum.files_free += space.files_free;
        table.push(row(
            &m.source,
            kinds.then_some(m.kind.as_str()),
            &space,
            inodes,
            unit,
            &path.to_string_lossy(),
        ));
    }
    if total {
        table.push(row("total", kinds.then_some("-"), &sum, inodes, unit, "-"));
    }
    let columns = table[0].len();
    let rights: Vec<bool> =
        (0..columns).map(|i| i > (if kinds { 1 } else { 0 }) && i + 1 < columns).collect();
    // The last column is a path: line it up too, as df does.
    print_table(&table, &rights, None)?;
    Ok(code)
}

/// The path a mount is at, from the root.
fn mount_path(m: &Mount) -> PathBuf {
    let path = m.path.as_bytes();
    if path.starts_with(b"/") { PathBuf::from(&m.path) } else { Path::new("/").join(&m.path) }
}

/// The mount a path is on.
fn mount_of<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|m| path.starts_with(mount_path(m)))
        .max_by_key(|m| mount_path(m).as_os_str().len())
}

/// A row of the table.
fn row(
    source: &str,
    kind: Option<&str>,
    s: &Space,
    inodes: bool,
    unit: Unit,
    path: &str,
) -> Vec<String> {
    let mut row = vec![source.to_string()];
    if let Some(kind) = kind {
        row.push(kind.to_string());
    }
    let percent = |used: u64, of: u64| {
        if of == 0 { "-".to_string() } else { format!("{}%", (used * 100).div_ceil(of)) }
    };
    if inodes {
        let used = s.files.saturating_sub(s.files_free);
        row.extend([
            s.files.to_string(),
            used.to_string(),
            s.files_free.to_string(),
            percent(used, s.files),
        ]);
    } else {
        let used = s.size.saturating_sub(s.free);
        let show = |bytes: u64| match unit {
            Unit::Blocks(block) => bytes.div_ceil(block).to_string(),
            // Coreutils writes SI's kilo as k.
            Unit::Human { si: true } => {
                let text = util::human(bytes, true);
                match text.strip_suffix('K') {
                    Some(number) => format!("{number}k"),
                    None => text,
                }
            }
            Unit::Human { si: false } => util::human(bytes, false),
        };
        // Use% is of what users can have: used and available.
        row.extend([
            show(s.size),
            show(used),
            show(s.available),
            percent(used, used + s.available),
        ]);
    }
    row.push(path.to_string());
    row
}

/// A block size: a number with an optional unit (`K`, `M`, `G`, `KB`...).
fn block_size(text: &str) -> Result<u64, Fail> {
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    let suffix = &text[digits.len()..];
    let n: u64 = if digits.is_empty() {
        1
    } else {
        digits.parse().map_err(|_| usage(format!("invalid block size: '{text}'")))?
    };
    let scale: u64 = match suffix {
        "" => 1,
        "K" | "k" | "KiB" => 1 << 10,
        "M" | "MiB" => 1 << 20,
        "G" | "GiB" => 1 << 30,
        "T" | "TiB" => 1 << 40,
        "KB" | "kB" => 1000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => return Err(usage(format!("invalid block size: '{text}'"))),
    };
    Ok(n * scale)
}

/// A block size's name in a header: `1K`, `1M`, or bytes.
fn block_name(bytes: u64) -> String {
    for (scale, unit) in [(1u64 << 40, "T"), (1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K")] {
        if bytes.is_multiple_of(scale) {
            return format!("{}{unit}", bytes / scale);
        }
    }
    bytes.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_sizes() {
        assert_eq!(block_size("1K").unwrap(), 1024);
        assert_eq!(block_size("4M").unwrap(), 4 << 20);
        assert_eq!(block_size("1000").unwrap(), 1000);
        assert_eq!(block_size("MB").unwrap(), 1_000_000);
        assert!(block_size("3Q").is_err());
        assert_eq!(block_name(1024), "1K");
        assert_eq!(block_name(512), "512");
    }

    #[test]
    fn files_are_on_their_longest_mount() {
        let m = |path: &str| Mount { source: path.into(), path: path.into(), kind: "tmpfs".into() };
        let mounts = vec![m("/"), m("tmp"), m("/tmp/deep")];
        assert_eq!(mount_of(&mounts, Path::new("/tmp/x")).unwrap().source, "tmp");
        assert_eq!(mount_of(&mounts, Path::new("/tmp/deep/y")).unwrap().source, "/tmp/deep");
        assert_eq!(mount_of(&mounts, Path::new("/etc")).unwrap().source, "/");
    }
}
