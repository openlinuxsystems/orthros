// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! findmnt(8): the mounts, from `/processes/mounts` (radix-proc(5)), as
//! a tree of where each is, with util-linux's columns and common
//! options. A mount from a disk or partition names it by its path in
//! `/devices`.

use std::path::{Path, PathBuf};

use crate::args::{Arg, Args};
use crate::df;
use crate::probe;
use crate::proc::{self, Mount};
use crate::ps::print_table;
use crate::util::{self, Fail, usage};

/// The columns, and whether each is a number, put to the right.
const COLUMNS: &[(&str, bool)] = &[
    ("TARGET", false),
    ("SOURCE", false),
    ("FSTYPE", false),
    ("OPTIONS", false),
    ("LABEL", false),
    ("UUID", false),
    ("SIZE", true),
    ("USED", true),
    ("AVAIL", true),
    ("USE%", true),
];

const DEFAULT: &[&str] = &["TARGET", "SOURCE", "FSTYPE", "OPTIONS"];
/// `-D`, as df(1) has them.
const DF: &[&str] = &["SOURCE", "FSTYPE", "SIZE", "USED", "AVAIL", "USE%", "TARGET"];

/// The source as findmnt shows it: a disk or partition by its path.
fn source(mount: &Mount) -> String {
    if probe::block_text(&mount.source, "size").is_some() {
        format!("/devices/{}", mount.source)
    } else {
        mount.source.clone()
    }
}

/// A mount's kind as libmount names it: `vfat` for FAT.
fn kind(mount: &Mount) -> &str {
    if mount.kind == "fat" { "vfat" } else { &mount.kind }
}

fn cell(mount: &Mount, column: &str, bytes: bool) -> String {
    let size = |n: u64| if bytes { n.to_string() } else { util::human(n, false) };
    let space = || df::statvfs(df::mount_path(mount).as_os_str()).ok();
    match column {
        "TARGET" => df::mount_path(mount).to_string_lossy().into_owned(),
        "SOURCE" => source(mount),
        "FSTYPE" => kind(mount).into(),
        "OPTIONS" => if mount.read_only { "ro" } else { "rw" }.into(),
        "LABEL" => mount.label.clone().unwrap_or_default(),
        "UUID" => crate::blkid::tags(&mount.source)
            .into_iter()
            .find(|(tag, _)| *tag == "UUID")
            .map_or_default(|(_, uuid)| uuid),
        "SIZE" => space().map_or_else(String::new, |s| size(s.size)),
        "USED" => space().map_or_else(String::new, |s| size(s.size.saturating_sub(s.free))),
        "AVAIL" => space().map_or_else(String::new, |s| size(s.available)),
        "USE%" => space().map_or_else(String::new, |s| match s.size {
            0 => "-".into(),
            all => format!("{}%", (s.size.saturating_sub(s.free) * 100).div_ceil(all)),
        }),
        _ => String::new(),
    }
}

/// Whether `kind` passes `-t`: a list of kinds, or of kinds it must not
/// be when it starts with `no`.
fn kind_matches(types: &str, kind: &str) -> bool {
    let (negate, types) = match types.strip_prefix("no") {
        Some(rest) => (true, rest),
        None => (false, types),
    };
    util::list(types).any(|t| t == kind) != negate
}

/// Sets `columns` from `-o`: a list of columns, or more after `+`.
fn set_columns(columns: &mut Vec<String>, text: &str) -> Result<(), Fail> {
    let (append, text) = match text.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if !append {
        columns.clear();
    }
    for name in util::list(text) {
        let name = name.to_ascii_uppercase();
        if !COLUMNS.iter().any(|(c, _)| *c == name) {
            return Err(usage(format!("unknown column: {name}")));
        }
        columns.push(name);
    }
    Ok(())
}

/// The mount that `path` is on: the one at the longest path that holds
/// it.
fn holding<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .rev()
        .filter(|m| path.starts_with(df::mount_path(m)))
        .max_by_key(|m| df::mount_path(m).as_os_str().len())
}

/// The mount each mount is beneath: the last before it at the longest
/// path that holds its own.
fn parents(mounts: &[&Mount]) -> Vec<Option<usize>> {
    let paths: Vec<PathBuf> = mounts.iter().map(|m| df::mount_path(m)).collect();
    (0..mounts.len())
        .map(|i| {
            (0..mounts.len())
                .filter(|&j| j != i && paths[i].starts_with(&paths[j]) && paths[i] != paths[j])
                .max_by_key(|&j| paths[j].as_os_str().len())
        })
        .collect()
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut bytes, mut headings, mut list, mut first_only) = (false, true, false, false);
    let mut columns: Vec<String> = DEFAULT.iter().map(|c| (*c).to_string()).collect();
    let (mut types, mut sources, mut target, mut wanted) = (None, Vec::new(), None, Vec::new());
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('b') => bytes = true,
            Arg::Long(long) if long == "bytes" => bytes = true,
            Arg::Short('n') => headings = false,
            Arg::Long(long) if long == "noheadings" => headings = false,
            Arg::Short('l') => list = true,
            Arg::Long(long) if long == "list" => list = true,
            Arg::Short('f') => first_only = true,
            Arg::Long(long) if long == "first-only" => first_only = true,
            Arg::Short('D') => {
                columns = DF.iter().map(|c| (*c).to_string()).collect();
                list = true;
            }
            Arg::Long(long) if long == "df" => {
                columns = DF.iter().map(|c| (*c).to_string()).collect();
                list = true;
            }
            Arg::Short('o') => set_columns(&mut columns, &args.text()?)?,
            Arg::Long(long) if long == "output" => set_columns(&mut columns, &args.text()?)?,
            Arg::Short('t') => types = Some(args.text()?),
            Arg::Long(long) if long == "types" => types = Some(args.text()?),
            Arg::Short('S') => sources.push(args.text()?),
            Arg::Long(long) if long == "source" => sources.push(args.text()?),
            Arg::Short('T') => target = Some(args.text()?),
            Arg::Long(long) if long == "target" => target = Some(args.text()?),
            // Only the kernel's table is kept: there is no fstab.
            Arg::Short('k') => {}
            Arg::Long(long) if long == "kernel" => {}
            Arg::Operand(what) => wanted.push(what.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    let mounts = proc::mounts().map_err(|e| util::failed(&"/processes/mounts", e))?;
    // A name names a mount by its path or its source.
    let names_source = |m: &Mount, name: &str| {
        let name = name.strip_prefix("/devices/").or_else(|| name.strip_prefix("/dev/"));
        name.is_some_and(|name| name == m.source) || source(m) == name.unwrap_or_default()
    };
    let target_mount =
        target.as_ref().map(|path| holding(&mounts, Path::new(path)).map(df::mount_path));
    let mut shown: Vec<&Mount> = mounts
        .iter()
        .filter(|m| {
            types.as_deref().is_none_or(|t| kind_matches(t, kind(m)) || kind_matches(t, &m.kind))
        })
        .filter(|m| {
            sources.is_empty() || sources.iter().any(|s| names_source(m, s) || *s == m.source)
        })
        .filter(|m| match &target_mount {
            Some(Some(path)) => df::mount_path(m) == *path,
            Some(None) => false,
            None => true,
        })
        .filter(|m| {
            wanted.is_empty()
                || wanted.iter().any(|w| {
                    names_source(m, w) || *w == m.source || df::mount_path(m) == Path::new(w)
                })
        })
        .collect();
    if first_only {
        shown.truncate(1);
    }
    if shown.is_empty() {
        return Ok(1);
    }
    // A search shows a list, as util-linux does.
    let tree =
        !list && types.is_none() && sources.is_empty() && target.is_none() && wanted.is_empty();
    let right: Vec<bool> = columns
        .iter()
        .map(|c| COLUMNS.iter().find(|(name, _)| name == c).is_some_and(|(_, r)| *r))
        .collect();
    let mut table = Vec::new();
    if headings {
        table.push(columns.clone());
    }
    let prefixes = if tree { tree_prefixes(&shown) } else { vec![String::new(); shown.len()] };
    for (mount, prefix) in shown.iter().zip(prefixes) {
        let row = columns
            .iter()
            .map(|c| {
                let value = cell(mount, c, bytes);
                if c == "TARGET" { format!("{prefix}{value}") } else { value }
            })
            .collect();
        table.push(row);
    }
    print_table(&table, &right, None)?;
    Ok(0)
}

/// The lines before each mount's target, as a tree of where each is
/// beneath the mount it is on, in the order the mounts were made.
fn tree_prefixes(mounts: &[&Mount]) -> Vec<String> {
    let parents = parents(mounts);
    let (branch, last, down, blank) = if util::utf8_locale() {
        ("├─", "└─", "│ ", "  ")
    } else {
        ("|-", "`-", "| ", "  ")
    };
    let is_last = |i: usize| !(i + 1..mounts.len()).any(|j| parents[j] == parents[i]);
    (0..mounts.len())
        .map(|i| {
            let Some(parent) = parents[i] else { return String::new() };
            let mut prefix = String::from(if is_last(i) { last } else { branch });
            let mut at = parent;
            while let Some(up) = parents[at] {
                prefix.insert_str(0, if is_last(at) { blank } else { down });
                at = up;
            }
            prefix
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(path: &str) -> Mount {
        Mount {
            source: "tmpfs".into(),
            path: path.into(),
            kind: "tmpfs".into(),
            read_only: false,
            label: None,
        }
    }

    #[test]
    fn mounts_make_a_tree_of_their_paths() {
        let mounts = [mount("/"), mount("system"), mount("system/live"), mount("processes")];
        let refs: Vec<&Mount> = mounts.iter().collect();
        assert_eq!(parents(&refs), [None, Some(0), Some(1), Some(0)]);
        let prefixes = tree_prefixes(&refs);
        assert!(prefixes[0].is_empty());
        assert_eq!(prefixes[2].chars().count(), 4);
        assert!(kind_matches("btrfs,ext4", "ext4"));
        assert!(kind_matches("notmpfs", "btrfs"));
        assert!(!kind_matches("notmpfs,proc", "proc"));
    }
}
