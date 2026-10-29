// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! quota(8): turns a Btrfs filesystem's simple quotas on and off, shows
//! its quota groups and sets their limits, as Linux's `btrfs quota
//! enable --simple`, `btrfs quota disable`, `btrfs qgroup show` and
//! `btrfs qgroup limit` do (ADR 0170), through the directory interface
//! (radix-directory(2)).

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use radix_abi::Status;
use radix_abi::directory::{self, Qgroup as Q};
use radix_abi::errors::{File as FileError, Kernel};

use crate::args::{Arg, Args};
use crate::sys::{self, SYS};
use crate::util::{self, Fail};

/// The most quota groups `show` lists.
const MAX_GROUPS: usize = 4096;

/// A quota group, as `qgroups` writes it.
struct Group {
    id: u64,
    referenced: u64,
    exclusive: u64,
    max_referenced: u64,
    max_exclusive: u64,
}

/// What a failed call on `path` says, as btrfs-progs words it.
fn message(status: Status, path: &str) -> String {
    match (Kernel::from_status(status), FileError::from_status(status)) {
        (Some(Kernel::NotSupported), _) => {
            format!("{path}: quotas not enabled, or not a btrfs filesystem with simple quotas")
        }
        (_, Some(FileError::NotFound)) => format!("{path}: no such quota group"),
        (_, Some(FileError::ReadOnly)) => format!("{path}: read-only file system"),
        _ => format!("{path}: {}", util::strip_os_error(&sys::io_error(status))),
    }
}

/// A size as btrfs-progs prints it: two decimals and a unit in powers of
/// 1024, `16.00KiB`, or bytes for `raw`.
fn size(bytes: u64, raw: bool) -> String {
    if raw {
        return bytes.to_string();
    }
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut unit = 0;
    let mut scale = 1u128;
    while u128::from(bytes) >= scale * 1024 && unit + 1 < units.len() {
        scale *= 1024;
        unit += 1;
    }
    let hundredths = (u128::from(bytes) * 100 + scale / 2) / scale;
    format!("{}.{:02}{}", hundredths / 100, hundredths % 100, units[unit])
}

/// A limit as btrfs-progs reads it: `none`, or bytes with an optional
/// unit, k, m, g, t, p or e, in powers of 1024.
fn limit(text: &str) -> Option<u64> {
    if text == "none" {
        return Some(u64::MAX);
    }
    let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let number: u64 = number.parse().ok()?;
    let shift = match unit.to_ascii_lowercase().as_str() {
        "" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        "t" => 40,
        "p" => 50,
        "e" => 60,
        _ => return None,
    };
    number.checked_mul(1u64.checked_shl(shift)?).filter(|&n| n != u64::MAX)
}

/// A quota group's ID as btrfs-progs writes it, `level/id`, or a
/// subvolume's ID alone, which is level 0.
fn qgroup_id(text: &str) -> Option<u64> {
    match text.split_once('/') {
        Some((level, id)) => {
            let (level, id) = (level.parse::<u64>().ok()?, id.parse::<u64>().ok()?);
            (level < 1 << 16 && id < 1 << 48).then_some(level << 48 | id)
        }
        None => text.parse().ok().filter(|&id| id < 1 << 48),
    }
}

/// `id` as `level/id`.
fn shown_id(id: u64) -> String {
    format!("{}/{}", id >> 48, id & ((1 << 48) - 1))
}

/// The quota groups of the filesystem whose directory is in `dir`.
fn groups(dir: &std::fs::File, path: &str) -> Result<Vec<Group>, Fail> {
    let mut buffer = vec![0u8; MAX_GROUPS * Q::SIZE];
    let done = directory::qgroups(&SYS, sys::slot_of(dir.as_raw_fd()), &mut buffer)
        .map_err(|status| util::fail(message(status, path)))?;
    let count = usize::try_from(done.count).unwrap_or(usize::MAX).min(MAX_GROUPS);
    let field = |record: &[u8], at: usize| {
        u64::from_le_bytes(record[at..at + 8].try_into().unwrap_or([0; 8]))
    };
    Ok(buffer
        .as_chunks::<{ Q::SIZE }>()
        .0
        .iter()
        .take(count)
        .map(|r| Group {
            id: field(r, Q::OFFSET_ID),
            referenced: field(r, Q::OFFSET_REFERENCED),
            exclusive: field(r, Q::OFFSET_EXCLUSIVE),
            max_referenced: field(r, Q::OFFSET_MAX_REFERENCED),
            max_exclusive: field(r, Q::OFFSET_MAX_EXCLUSIVE),
        })
        .collect())
}

/// `show`'s table, as `btrfs qgroup show` prints it, with the limits
/// `max_referenced` and `max_exclusive` ask for.
fn table(groups: &[Group], max_referenced: bool, max_exclusive: bool, raw: bool) -> String {
    let limit = |max: u64| if max == u64::MAX { "none".to_string() } else { size(max, raw) };
    let mut heads = vec!["Qgroupid", "Referenced", "Exclusive"];
    if max_referenced {
        heads.push("Max referenced");
    }
    if max_exclusive {
        heads.push("Max exclusive");
    }
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|g| {
            let mut row = vec![shown_id(g.id), size(g.referenced, raw), size(g.exclusive, raw)];
            if max_referenced {
                row.push(limit(g.max_referenced));
            }
            if max_exclusive {
                row.push(limit(g.max_exclusive));
            }
            row
        })
        .collect();
    let widths: Vec<usize> = (0..heads.len())
        .map(|c| rows.iter().map(|r| r[c].len()).chain([heads[c].len()]).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    let line = |out: &mut String, cells: &[String]| {
        for (c, cell) in cells.iter().enumerate() {
            let gap = if c == 0 { "" } else { " " };
            // The ID to the left, the sizes to the right, as btrfs-progs.
            if c == 0 {
                out.push_str(&format!("{cell:<w$}", w = widths[c]));
            } else {
                out.push_str(&format!("{gap}{cell:>w$}", w = widths[c]));
            }
        }
        out.push('\n');
    };
    line(&mut out, &heads.iter().map(ToString::to_string).collect::<Vec<_>>());
    line(&mut out, &heads.iter().map(|h| "-".repeat(h.len())).collect::<Vec<_>>());
    for row in &rows {
        line(&mut out, row);
    }
    out
}

/// quota(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let usage = || util::usage("say enable, disable, show or limit, and a path".into());
    let Some(action) = args.subcommand() else { return Err(usage()) };
    let action = action.to_string_lossy().into_owned();
    let (mut max_referenced, mut max_exclusive, mut raw, mut operands) =
        (false, false, false, Vec::new());
    while let Some(arg) = args.next()? {
        match arg {
            // Only simple quotas are kept, as --simple asks.
            Arg::Short('s') if action == "enable" => {}
            Arg::Long(long) if action == "enable" && long == "simple" => {}
            Arg::Short('r') if action == "show" => max_referenced = true,
            Arg::Short('e') if action == "show" || action == "limit" => max_exclusive = true,
            Arg::Long(long) if action == "show" && long == "raw" => raw = true,
            Arg::Operand(operand) => operands.push(operand),
            other => return Err(args.unknown(&other)),
        }
    }
    let wanted = match action.as_str() {
        "enable" | "disable" | "show" => 1,
        "limit" => 3,
        _ => return Err(usage()),
    };
    if operands.len() != wanted {
        return Err(usage());
    }
    let path = PathBuf::from(&operands[wanted - 1]);
    let shown = path.display().to_string();
    let dir = sys::open_control(&path).map_err(|e| util::failed(&shown, e))?;
    let slot = sys::slot_of(dir.as_raw_fd());
    let fail = |status| util::fail(message(status, &shown));
    match action.as_str() {
        "enable" => directory::quota(&SYS, slot, true).map_err(fail)?,
        "disable" => directory::quota(&SYS, slot, false).map_err(fail)?,
        "show" => {
            let groups = groups(&dir, &shown)?;
            let text = table(&groups, max_referenced, max_exclusive, raw);
            let _ = io::stdout().write_all(text.as_bytes());
        }
        _ => {
            let bytes = operands[0].to_string_lossy();
            let bytes = limit(&bytes)
                .ok_or_else(|| util::usage(format!("invalid size argument: {bytes}")))?;
            let id = operands[1].to_string_lossy();
            let id = qgroup_id(&id)
                .ok_or_else(|| util::usage(format!("invalid qgroupid or subvolume path: {id}")))?;
            let (referenced, exclusive) =
                if max_exclusive { (u64::MAX, bytes) } else { (bytes, u64::MAX) };
            // The other limit stays as it is.
            let old = groups(&dir, &shown)?.into_iter().find(|g| g.id == id);
            let (referenced, exclusive) = match old {
                Some(g) if max_exclusive => (g.max_referenced, exclusive),
                Some(g) => (referenced, g.max_exclusive),
                None => (referenced, exclusive),
            };
            directory::limit_qgroup(&SYS, slot, id, referenced, exclusive).map_err(fail)?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_and_print_as_btrfs_progs() {
        assert_eq!(size(16384, false), "16.00KiB");
        assert_eq!(size(0, false), "0.00B");
        assert_eq!(size(1536 << 10, false), "1.50MiB");
        assert_eq!(size(1536 << 10, true), "1572864");
        assert_eq!(limit("100m"), Some(100 << 20));
        assert_eq!(limit("2G"), Some(2 << 30));
        assert_eq!(limit("none"), Some(u64::MAX));
        assert_eq!(limit("3x"), None);
        assert_eq!(qgroup_id("0/257"), Some(257));
        assert_eq!(qgroup_id("1/100"), Some((1 << 48) + 100));
        assert_eq!(qgroup_id("256"), Some(256));
        assert_eq!(shown_id((1 << 48) + 100), "1/100");
    }

    #[test]
    fn the_table_lines_up() {
        let groups = [
            Group {
                id: 5,
                referenced: 16384,
                exclusive: 16384,
                max_referenced: u64::MAX,
                max_exclusive: u64::MAX,
            },
            Group {
                id: 256,
                referenced: 1 << 20,
                exclusive: 1 << 20,
                max_referenced: 2 << 20,
                max_exclusive: u64::MAX,
            },
        ];
        assert_eq!(
            table(&groups, true, false, false),
            "Qgroupid Referenced Exclusive Max referenced\n\
             -------- ---------- --------- --------------\n\
             0/5        16.00KiB  16.00KiB           none\n\
             0/256       1.00MiB   1.00MiB        2.00MiB\n"
        );
    }
}
