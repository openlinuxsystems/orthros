// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! df(1): the space of each mounted filesystem, from `/processes/mounts` and
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
pub struct Space {
    pub size: u64,
    pub free: u64,
    pub available: u64,
    pub files: u64,
    pub files_free: u64,
}

pub fn statvfs(path: &OsStr) -> io::Result<Space> {
    let path =
        CString::new(path.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: all zeros is a valid statvfs: numbers.
    let mut st: libc::statvfs = unsafe { util::zeroed() };
    // SAFETY: the path is a C string and `st` a statvfs for the call.
    if unsafe { libc::statvfs(path.as_ptr(), &raw mut st) } != 0 {
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
    /// Blocks of this many bytes, each number followed by the unit when
    /// -B named one without a number (`-BM` prints 258020M).
    Blocks(u64, Option<&'static str>),
    Human {
        si: bool,
    },
}

/// df(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut all = false;
    let mut unit = Unit::Blocks(1024, None);
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
            Arg::Short('k') => unit = Unit::Blocks(1024, None),
            Arg::Short('m') => unit = Unit::Blocks(1 << 20, None),
            Arg::Short('B') => {
                let (bytes, suffix) = block_size("-B", &args.text()?)?;
                unit = Unit::Blocks(bytes, suffix);
            }
            Arg::Long(l) if l == "block-size" => {
                let (bytes, suffix) = block_size("--block-size", &args.text()?)?;
                unit = Unit::Blocks(bytes, suffix);
            }
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
    let mounts = proc::mounts().map_err(|e| util::failed(&"/processes/mounts", e))?;
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
        (Unit::Blocks(bytes, _), true) => format!("{bytes}-blocks"),
        (Unit::Blocks(_, Some(suffix)), _) => format!("1{suffix}-blocks"),
        (Unit::Blocks(bytes, None), _) => format!("{}-blocks", block_name(bytes)),
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
        (0..columns).map(|i| i > usize::from(kinds) && i + 1 < columns).collect();
    // The last column is a path: line it up too, as df does.
    print_table(&table, &rights, None)?;
    Ok(code)
}

/// The path a mount is at, from the root.
pub fn mount_path(m: &Mount) -> PathBuf {
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
            Unit::Blocks(block, suffix) => {
                format!("{}{}", bytes.div_ceil(block), suffix.unwrap_or(""))
            }
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

/// The units a block size may name, from K up: as binary ones are
/// written alone and with iB, and as SI ones are written.
const UNITS: [[&str; 3]; 8] = [
    ["K", "KiB", "kB"],
    ["M", "MiB", "MB"],
    ["G", "GiB", "GB"],
    ["T", "TiB", "TB"],
    ["P", "PiB", "PB"],
    ["E", "EiB", "EB"],
    ["Z", "ZiB", "ZB"],
    ["Y", "YiB", "YB"],
];

/// A block size given to `option`, as GNU df reads one: a number, a unit
/// (`K` or `KiB` for 1024, `KB` or `kB` for 1000, then M, G, T, P and E,
/// in either case), or both. A unit given alone also follows every
/// number shown, and comes back too.
fn block_size(option: &str, text: &str) -> Result<(u64, Option<&'static str>), Fail> {
    let invalid = || usage(format!("invalid {option} argument '{text}'"));
    let large = || usage(format!("{option} argument '{text}' too large"));
    let body = text.strip_prefix('+').unwrap_or(text);
    let (number, suffix) = body.split_at(body.bytes().take_while(u8::is_ascii_digit).count());
    let unit = suffix.chars().next().and_then(|letter| {
        let power = "KMGTPEZY".find(letter.to_ascii_uppercase())?;
        let form = ["", "iB", "B"].iter().position(|&form| form == &suffix[1..])?;
        Some((power, form))
    });
    let (scale, name) = match unit {
        _ if suffix.is_empty() => (Some(1), None),
        Some((power, form)) => {
            let base: u64 = if form == 2 { 1000 } else { 1024 };
            (base.checked_pow(power as u32 + 1), Some(UNITS[power][form]))
        }
        None if number.is_empty() => return Err(invalid()),
        None => return Err(usage(format!("invalid suffix in {option} argument '{text}'"))),
    };
    if number.is_empty() && name.is_none() {
        return Err(invalid());
    }
    let n = if number.is_empty() { Some(1) } else { number.parse::<u64>().ok() };
    let bytes = n.zip(scale).and_then(|(n, scale)| n.checked_mul(scale)).ok_or_else(large)?;
    if bytes == 0 {
        return Err(invalid());
    }
    Ok((bytes, if number.is_empty() { name } else { None }))
}

/// A block size's name in a header, as GNU df writes it: in powers of
/// 1024 (`4K`) or of 1000 with a B (`3kB`, `512B`), whichever divides it
/// evenly more times (1000 when they do as often), rounded up as -h
/// rounds, with no `.0`.
fn block_name(bytes: u64) -> String {
    let (mut thousands, mut kibis) = (bytes, bytes);
    let binary = loop {
        let (by_1000, by_1024) = (thousands % 1000 == 0, kibis % 1024 == 0);
        if by_1000 != by_1024 || !by_1000 {
            break by_1024;
        }
        thousands /= 1000;
        kibis /= 1024;
    };
    let text = util::human(bytes, !binary).replace(".0", "");
    match text.strip_suffix('K') {
        _ if binary => text,
        Some(number) => format!("{number}kB"),
        None => format!("{text}B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_sizes() {
        let size = |text: &str| block_size("-B", text).map_err(|fail| fail.message);
        assert_eq!(size("1K"), Ok((1024, None)));
        assert_eq!(size("4M"), Ok((4 << 20, None)));
        assert_eq!(size("1000"), Ok((1000, None)));
        assert_eq!(size("+1"), Ok((1, None)));
        assert_eq!(size("1kiB"), Ok((1024, None)));
        assert_eq!(size("1mB"), Ok((1_000_000, None)));
        assert_eq!(size("15E"), Ok((15 << 60, None)));
        assert_eq!(size("18446744073709551615"), Ok((u64::MAX, None)));
        // A unit alone follows the numbers, written as GNU df writes it.
        assert_eq!(size("MB"), Ok((1_000_000, Some("MB"))));
        assert_eq!(size("k"), Ok((1024, Some("K"))));
        assert_eq!(size("KB"), Ok((1000, Some("kB"))));
        assert_eq!(size("miB"), Ok((1 << 20, Some("MiB"))));
        // As the host's df (coreutils 9.4) words them.
        for (text, message) in [
            ("0", "invalid -B argument '0'"),
            ("0K", "invalid -B argument '0K'"),
            ("", "invalid -B argument ''"),
            ("x", "invalid -B argument 'x'"),
            ("B", "invalid -B argument 'B'"),
            ("-1", "invalid -B argument '-1'"),
            ("3Q", "invalid suffix in -B argument '3Q'"),
            ("1.5K", "invalid suffix in -B argument '1.5K'"),
            ("1KIB", "invalid suffix in -B argument '1KIB'"),
            ("1KBB", "invalid suffix in -B argument '1KBB'"),
            ("99999999999T", "-B argument '99999999999T' too large"),
            ("16E", "-B argument '16E' too large"),
            ("Y", "-B argument 'Y' too large"),
            ("18446744073709551616", "-B argument '18446744073709551616' too large"),
        ] {
            assert_eq!(size(text), Err(message.to_string()), "{text}");
        }
        assert_eq!(
            block_size("--block-size", "0").map_err(|fail| fail.message),
            Err("invalid --block-size argument '0'".to_string())
        );
    }

    #[test]
    fn block_names() {
        for (bytes, name) in [
            (1, "1B"),
            (512, "512B"),
            (1000, "1kB"),
            (1024, "1K"),
            (1536, "1.6kB"),
            (3000, "3kB"),
            (4096, "4K"),
            (10240, "10K"),
            (1_000_000, "1MB"),
            (1 << 20, "1M"),
            (1_024_000, "1.1MB"),
            (u64::MAX, "19EB"),
        ] {
            assert_eq!(block_name(bytes), name, "{bytes}");
        }
    }

    #[test]
    fn files_are_on_their_longest_mount() {
        let m = |path: &str| Mount {
            source: path.into(),
            path: path.into(),
            kind: "tmpfs".into(),
            read_only: false,
            label: None,
        };
        let mounts = vec![m("/"), m("tmp"), m("/tmp/deep")];
        assert_eq!(mount_of(&mounts, Path::new("/tmp/x")).unwrap().source, "tmp");
        assert_eq!(mount_of(&mounts, Path::new("/tmp/deep/y")).unwrap().source, "/tmp/deep");
        assert_eq!(mount_of(&mounts, Path::new("/etc")).unwrap().source, "/");
    }
}
