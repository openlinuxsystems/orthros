// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! lsblk(8): the disks and their partitions as a tree, from
//! `/kernel/block` (radix-sysfs(5)), what blkid finds on them and
//! `/processes/mounts`, with util-linux's columns and common options.
//! Radix gives disks no device numbers, so there is no MAJ:MIN.

use crate::args::{Arg, Args};
use crate::df;
use crate::probe::{self, BLOCKS};
use crate::proc::{self, Mount};
use crate::ps::print_table;
use crate::util::{self, Fail, usage};

/// The columns, as util-linux names them, and whether each is a number,
/// put to the right.
const COLUMNS: &[(&str, bool)] = &[
    ("NAME", false),
    ("KNAME", false),
    ("PATH", false),
    ("PKNAME", false),
    ("SIZE", true),
    ("RO", true),
    ("TYPE", false),
    ("MOUNTPOINT", false),
    ("MOUNTPOINTS", false),
    ("FSTYPE", false),
    ("FSVER", false),
    ("LABEL", false),
    ("UUID", false),
    ("FSAVAIL", true),
    ("FSSIZE", true),
    ("FSUSED", true),
    ("FSUSE%", true),
    ("PTUUID", false),
    ("PTTYPE", false),
    ("PARTUUID", false),
    ("PARTLABEL", false),
    ("PARTN", true),
    ("START", true),
    ("MODEL", false),
    ("SERIAL", false),
    ("REV", false),
    ("ROTA", true),
    ("LOG-SEC", true),
    ("PHY-SEC", true),
    ("MIN-IO", true),
    ("OPT-IO", true),
    ("DISC-GRAN", true),
    ("DISC-MAX", true),
];

const DEFAULT: &[&str] = &["NAME", "SIZE", "RO", "TYPE", "MOUNTPOINTS"];
const FILESYSTEMS: &[&str] =
    &["NAME", "FSTYPE", "FSVER", "LABEL", "UUID", "FSAVAIL", "FSUSE%", "MOUNTPOINTS"];

/// A disk or partition and what is known of it.
struct Device {
    name: String,
    /// The disk it is on, for a partition.
    disk: Option<String>,
    number: Option<u32>,
    tags: Vec<(&'static str, String)>,
    version: String,
    mounts: Vec<String>,
    /// The filesystem's size, used and available bytes, if mounted.
    space: Option<(u64, u64, u64)>,
}

impl Device {
    fn tag(&self, name: &str) -> String {
        self.tags.iter().find(|(tag, _)| *tag == name).map_or_default(|(_, v)| v.clone())
    }

    /// A value in `/kernel/block/<disk>/<what>`, for a partition its
    /// disk's.
    fn of_disk(&self, what: &str) -> String {
        probe::block_text(self.disk.as_deref().unwrap_or(&self.name), what).unwrap_or_default()
    }

    fn cell(&self, column: &str, bytes: bool) -> String {
        let size = |n: u64| if bytes { n.to_string() } else { util::human(n, false) };
        let number = |what: &str| self.of_disk(what).parse::<u64>().ok();
        match column {
            "NAME" | "KNAME" => self.name.clone(),
            "PATH" => format!("/devices/{}", self.name),
            "PKNAME" => self.disk.clone().unwrap_or_default(),
            "SIZE" => probe::block_text(&self.name, "size")
                .and_then(|n| n.parse::<u64>().ok())
                .map_or_else(String::new, |sectors| size(sectors * 512)),
            "RO" => probe::block_text(&self.name, "ro").unwrap_or_default(),
            "TYPE" => if self.disk.is_some() { "part" } else { "disk" }.into(),
            "MOUNTPOINT" => self.mounts.first().cloned().unwrap_or_default(),
            "MOUNTPOINTS" => self.mounts.join("\n"),
            "FSTYPE" => self.tag("TYPE"),
            "FSVER" => self.version.clone(),
            "LABEL" | "UUID" | "PTUUID" | "PTTYPE" | "PARTUUID" | "PARTLABEL" => self.tag(column),
            "FSAVAIL" => self.space.map_or_else(String::new, |(_, _, avail)| size(avail)),
            "FSSIZE" => self.space.map_or_else(String::new, |(all, _, _)| size(all)),
            "FSUSED" => self.space.map_or_else(String::new, |(_, used, _)| size(used)),
            "FSUSE%" => self.space.map_or_else(String::new, |(all, used, _)| match all {
                0 => "-".into(),
                all => format!("{}%", (used * 100).div_ceil(all)),
            }),
            "PARTN" => self.number.map_or_else(String::new, |n| n.to_string()),
            "START" => probe::block_text(&self.name, "start").unwrap_or_default(),
            "MODEL" | "SERIAL" | "REV" if self.disk.is_some() => String::new(),
            "MODEL" => self.of_disk("device/model"),
            "SERIAL" => self.of_disk("device/serial"),
            "REV" => self.of_disk("device/firmware_rev"),
            "ROTA" => self.of_disk("queue/rotational"),
            "LOG-SEC" => self.of_disk("queue/logical_block_size"),
            "PHY-SEC" => self.of_disk("queue/physical_block_size"),
            "MIN-IO" => self.of_disk("queue/minimum_io_size"),
            "OPT-IO" => self.of_disk("queue/optimal_io_size"),
            "DISC-GRAN" => number("queue/discard_granularity").map_or_else(String::new, size),
            "DISC-MAX" => number("queue/discard_max_bytes").map_or_else(String::new, size),
            _ => String::new(),
        }
    }
}

/// What lsblk knows of `name`: its tags, the filesystem's version, and
/// where it is mounted.
fn device(name: &str, is_partition: bool, mounts: &[Mount]) -> Device {
    let number = probe::block_text(name, "partition").and_then(|n| n.parse().ok());
    let disk = is_partition.then(|| probe::disk_of(name).to_string());
    let version = probe::reader(name)
        .and_then(|mut read| probe::filesystem(&mut read))
        .map_or_default(|fs| fs.version);
    let mut paths: Vec<String> = Vec::new();
    for mount in mounts.iter().filter(|m| m.source == name) {
        let path = df::mount_path(mount).to_string_lossy().into_owned();
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    // A swap area in use shows as Linux shows it.
    if swapped().iter().any(|swap| swap == name) {
        paths.push("[SWAP]".into());
    }
    let space = mounts
        .iter()
        .find(|m| m.source == name)
        .and_then(|m| df::statvfs(df::mount_path(m).as_os_str()).ok())
        .map(|s| (s.size, s.size.saturating_sub(s.free), s.available));
    Device {
        name: name.into(),
        disk,
        number,
        tags: crate::blkid::tags(name),
        version,
        mounts: paths,
        space,
    }
}

/// The devices in use as swap areas, from `/processes/swaps`.
fn swapped() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(format!("{}/swaps", proc::PROC)) else {
        return Vec::new();
    };
    text.lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(|path| path.strip_prefix("/devices/").unwrap_or(path).to_string())
        .collect()
}

/// Sets `columns` from `-o`: a list of columns, or more after `+`.
fn add_columns(columns: &mut Vec<String>, text: &str) -> Result<(), Fail> {
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

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut bytes, mut disks_only, mut headings, mut list, mut paths) =
        (false, false, true, false, false);
    let mut columns: Vec<String> = DEFAULT.iter().map(|c| (*c).to_string()).collect();
    let mut names = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('b') => bytes = true,
            Arg::Long(long) if long == "bytes" => bytes = true,
            Arg::Short('d') => disks_only = true,
            Arg::Long(long) if long == "nodeps" => disks_only = true,
            Arg::Short('f') => columns = FILESYSTEMS.iter().map(|c| (*c).to_string()).collect(),
            Arg::Long(long) if long == "fs" => {
                columns = FILESYSTEMS.iter().map(|c| (*c).to_string()).collect();
            }
            Arg::Short('n') => headings = false,
            Arg::Long(long) if long == "noheadings" => headings = false,
            Arg::Short('l') => list = true,
            Arg::Long(long) if long == "list" => list = true,
            Arg::Short('p') => paths = true,
            Arg::Long(long) if long == "paths" => paths = true,
            Arg::Short('o') => add_columns(&mut columns, &args.text()?)?,
            Arg::Long(long) if long == "output" => add_columns(&mut columns, &args.text()?)?,
            Arg::Short('a') => {}
            Arg::Long(long) if long == "all" => {}
            Arg::Operand(name) => {
                let shown = name.to_string_lossy();
                let name = shown.strip_prefix("/devices/").or_else(|| shown.strip_prefix("/dev/"));
                names.push(name.unwrap_or(&shown).to_string());
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let all = probe::devices().map_err(|e| util::failed(&BLOCKS, e))?;
    let mut status = 0;
    for name in &names {
        if !all.iter().any(|(n, _)| n == name) {
            eprintln!("lsblk: /devices/{name}: not a block device");
            status = 32;
        }
    }
    let mounts = proc::mounts().unwrap_or_default();
    // The devices shown: a named disk with its partitions, a named
    // partition alone.
    let shown: Vec<&(String, bool)> = all
        .iter()
        .filter(|(name, part)| {
            names.is_empty()
                || names.contains(name)
                || (*part && names.iter().any(|n| n == probe::disk_of(name)))
        })
        .filter(|(_, part)| !(disks_only && *part))
        .collect();
    if shown.is_empty() {
        return Ok(if status == 0 { 1 } else { status });
    }
    let right: Vec<bool> = columns
        .iter()
        .map(|c| COLUMNS.iter().find(|(name, _)| name == c).is_some_and(|(_, r)| *r))
        .collect();
    let mut table = Vec::new();
    if headings {
        table.push(columns.clone());
    }
    let (branch, last) = if util::utf8_locale() { ("├─", "└─") } else { ("|-", "`-") };
    for (i, (name, part)) in shown.iter().enumerate() {
        let device = device(name, *part, &mounts);
        let next_is_partition = shown.get(i + 1).is_some_and(|(_, p)| *p);
        let mut row: Vec<String> = columns.iter().map(|c| device.cell(c, bytes)).collect();
        for (cell, column) in row.iter_mut().zip(&columns) {
            if column != "NAME" {
                continue;
            }
            if paths {
                *cell = format!("/devices/{cell}");
            }
            // A partition alone, as `lsblk sda1` shows it, has no branch.
            if *part && !list && shown.len() > 1 && !names.contains(name) {
                *cell = format!("{}{cell}", if next_is_partition { branch } else { last });
            }
        }
        // More mount points go on lines of their own, as util-linux has them.
        let extra: Vec<Vec<String>> = row
            .iter()
            .zip(&columns)
            .filter(|(_, c)| *c == "MOUNTPOINTS")
            .flat_map(|(cell, _)| cell.lines().skip(1).map(str::to_string).collect::<Vec<_>>())
            .map(|path| {
                columns
                    .iter()
                    .map(|c| if c == "MOUNTPOINTS" { path.clone() } else { String::new() })
                    .collect()
            })
            .collect();
        for (cell, column) in row.iter_mut().zip(&columns) {
            if column == "MOUNTPOINTS" {
                *cell = cell.lines().next().unwrap_or_default().to_string();
            }
        }
        table.push(row);
        table.extend(extra);
    }
    print_table(&table, &right, None)?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_set_of_columns_is_known() {
        for name in DEFAULT.iter().chain(FILESYSTEMS) {
            assert!(COLUMNS.iter().any(|(c, _)| c == name), "{name}");
        }
    }
}
