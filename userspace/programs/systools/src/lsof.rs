// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! lsof(8): what each process holds, from `/proc/<n>/files`: its open
//! files and directories, with the paths they were opened by, and every
//! other capability in its capability space, with its rights. lsof's
//! common options select by process, command, file and descriptor.
//!
//! A path is the one the file was opened by, worked out from the
//! directory it was opened beneath without following links; a file
//! renamed since keeps its old path, and a path too long to keep shows
//! its start, marked `(cut)`.

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::args::{Arg, Args};
use crate::proc::{self, Held, Process};
use crate::sys;
use crate::util::{self, Fail, usage};

/// lsof's FD column for a capability: a descriptor with its access, or
/// what a startup slot holds.
fn fd_name(h: &Held) -> String {
    if let Some(fd) = h.fd {
        let mode = match h.access.as_deref() {
            Some("rw") => "u",
            Some("r") => "r",
            Some("w") => "w",
            _ => "",
        };
        return format!("{fd}{mode}");
    }
    let slot = h.slot.unwrap_or(0);
    match sys::slot_name(slot).as_str() {
        "root" => "rtd".into(),
        "process" => "prc".into(),
        "thread" => "thd".into(),
        "space" => "spc".into(),
        "random" => "rnd".into(),
        "log" => "log".into(),
        _ => format!("s{slot}"),
    }
}

/// lsof's TYPE column.
fn type_name(h: &Held) -> String {
    match h.kind.as_str() {
        "file" => "REG",
        "directory" => "DIR",
        "char" => "CHR",
        "block" => "BLK",
        "pipe" => "FIFO",
        "socket" => "unix",
        "link" => "LINK",
        "process" => "PROC",
        "thread" => "THRD",
        "address_space" => "SPACE",
        "endpoint" => "ENDP",
        "notification" => "NTFY",
        "memory" => "MEM",
        "frame" => "FRAME",
        "domain" => "DOMN",
        "random" => "RAND",
        "ring" => "RING",
        "kernel_log" => "KLOG",
        other => return other.to_ascii_uppercase(),
    }
    .into()
}

/// What selects the lines shown.
#[derive(Default)]
struct Select {
    pids: Vec<u64>,
    not_pids: Vec<u64>,
    commands: Vec<String>,
    users: Vec<u32>,
    fds: Vec<String>,
    files: Vec<(PathBuf, Option<u64>)>,
    dirs: Vec<(PathBuf, bool)>,
    and: bool,
    files_only: bool,
}

impl Select {
    fn process_chosen(&self, p: &Process) -> bool {
        if self.not_pids.contains(&p.number) {
            return false;
        }
        let tests = [
            (!self.pids.is_empty()).then(|| self.pids.contains(&p.number)),
            (!self.commands.is_empty())
                .then(|| self.commands.iter().any(|c| p.name.starts_with(c.as_str()))),
            (!self.users.is_empty()).then(|| self.users.contains(&0)),
        ];
        let given: Vec<bool> = tests.iter().flatten().copied().collect();
        if given.is_empty() {
            return true;
        }
        if self.and { given.iter().all(|&t| t) } else { given.iter().any(|&t| t) }
    }

    /// Whether the line of `h`, named `fd` in the FD column, is chosen.
    fn line_chosen(&self, h: &Held, fd: &str) -> bool {
        if self.files_only && !h.is_file() {
            return false;
        }
        let number = h.fd.map(|n| n.to_string());
        if !self.fds.is_empty() && !self.fds.iter().any(|f| f == fd || Some(f) == number.as_ref()) {
            return false;
        }
        if self.files.is_empty() && self.dirs.is_empty() {
            return true;
        }
        let path = h.path.as_ref().map(PathBuf::from);
        let by_file = self.files.iter().any(|(want, node)| {
            path.as_ref() == Some(want)
                || node.is_some_and(|n| n == h.id && h.is_file() && (path.is_none() || h.cut))
        });
        let in_dir = self.dirs.iter().any(|(dir, deep)| {
            path.as_ref().is_some_and(|p| {
                if *deep {
                    p.starts_with(dir) && p != dir
                } else {
                    p.parent() == Some(dir.as_path())
                }
            })
        });
        by_file || in_dir
    }
}

/// lsof(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    // `+D DIR` and `+d DIR` start with a plus: take them out first.
    let mut items = args.rest();
    let mut s = Select::default();
    let mut i = 0;
    while i < items.len() {
        let item = items[i].as_bytes().to_vec();
        if item == b"+D" || item == b"+d" {
            let dir =
                items.get(i + 1).cloned().ok_or_else(|| usage("+D needs a directory".into()))?;
            let dir = std::fs::canonicalize(&dir)
                .map_err(|e| util::failed(&Path::new(&dir).display(), e))?;
            s.dirs.push((dir, item == b"+D"));
            items.drain(i..i + 2);
        } else {
            i += 1;
        }
    }
    *args = Args::new(args.tool, items);
    let mut terse = false;
    let mut offsets = false;
    let mut sizes = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('p') => {
                for pid in util::list(&args.text()?) {
                    match pid.strip_prefix('^') {
                        Some(n) => s.not_pids.push(
                            n.parse().map_err(|_| usage(format!("illegal process ID: {pid}")))?,
                        ),
                        None => s.pids.push(
                            pid.parse().map_err(|_| usage(format!("illegal process ID: {pid}")))?,
                        ),
                    }
                }
            }
            Arg::Short('c') => s.commands.push(args.text()?),
            Arg::Short('u') => {
                for name in util::list(&args.text()?) {
                    s.users.push(
                        util::user_id(name)
                            .ok_or_else(|| usage(format!("can't get UID for {name}")))?,
                    );
                }
            }
            Arg::Short('d') => s.fds.extend(util::list(&args.text()?).map(String::from)),
            Arg::Short('a') => s.and = true,
            Arg::Short('t') => terse = true,
            Arg::Short('o') => offsets = true,
            Arg::Short('s') => sizes = true,
            Arg::Long(l) if l == "files" => s.files_only = true,
            // Network files: Radix has none until Phase 12.
            Arg::Short('i') => {
                let _ = args.joined_value();
                return Ok(1);
            }
            Arg::Short('n' | 'P' | 'l' | 'b' | 'w' | 'V' | 'U' | 'R') => {}
            Arg::Operand(file) => {
                let meta = std::fs::metadata(&file)
                    .map_err(|e| util::failed(&Path::new(&file).display(), e))?;
                let path = std::fs::canonicalize(&file).unwrap_or_else(|_| PathBuf::from(&file));
                s.files.push((path, Some(meta.ino())));
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let searching =
        !s.files.is_empty() || !s.dirs.is_empty() || !s.pids.is_empty() || !s.commands.is_empty();
    let processes = proc::processes().map_err(|e| util::failed(&"/proc", e))?;
    let user = util::user_name(0);
    let mut table = vec![
        [
            "COMMAND",
            "PID",
            "USER",
            "FD",
            "TYPE",
            "DEVICE",
            if offsets {
                "OFFSET"
            } else if sizes {
                "SIZE"
            } else {
                "SIZE/OFF"
            },
            "NODE",
            "NAME",
        ]
        .map(String::from)
        .to_vec(),
    ];
    let mut pids_found: Vec<u64> = Vec::new();
    for p in processes.iter().filter(|p| !p.zombie() && s.process_chosen(p)) {
        let Ok(held) = proc::files(p.number) else { continue };
        let mut lines: Vec<Vec<String>> = Vec::new();
        // Its working directory first, as lsof lists it.
        if let Some(dir) = &p.directory {
            let cwd = Held {
                fd: None,
                slot: None,
                kind: "directory".into(),
                access: Some("r".into()),
                path: Some(dir.clone()),
                ..Held::default()
            };
            if s.line_chosen(&cwd, "cwd") {
                let node = std::fs::metadata(dir).map(|m| m.ino()).unwrap_or(0);
                lines.push(row(p, &user, "cwd".into(), &cwd, node, offsets, sizes));
            }
        }
        for h in &held {
            let fd = fd_name(h);
            if s.line_chosen(h, &fd) {
                lines.push(row(p, &user, fd, h, h.id, offsets, sizes));
            }
        }
        if !lines.is_empty() {
            pids_found.push(p.number);
            table.extend(lines);
        }
    }
    let mut out = io::stdout().lock();
    if terse {
        for pid in &pids_found {
            writeln!(out, "{pid}")?;
        }
    } else if table.len() > 1 {
        drop(out);
        let rights = [false, true, false, true, false, false, true, true, false];
        crate::ps::print_table(&table, &rights, None)?;
    }
    Ok(if searching && pids_found.is_empty() { 1 } else { 0 })
}

/// A line of the table.
fn row(
    p: &Process,
    user: &str,
    fd: String,
    h: &Held,
    node: u64,
    offsets: bool,
    sizes: bool,
) -> Vec<String> {
    let file = h.is_file();
    let size_off = if !file {
        String::new()
    } else if offsets || (!sizes && matches!(h.kind.as_str(), "pipe" | "socket" | "char")) {
        h.offset.map_or(String::new(), |o| format!("0t{o}"))
    } else {
        h.size.to_string()
    };
    let name = match (&h.path, file) {
        (Some(path), _) => {
            let mut name = String::from_utf8_lossy(path.as_bytes()).into_owned();
            if h.cut {
                name.push_str(" (cut)");
            }
            name
        }
        (None, true) => match h.kind.as_str() {
            "pipe" => "pipe".into(),
            "socket" => "socket".into(),
            _ => String::new(),
        },
        (None, false) => h.rights.clone(),
    };
    let device = if file { format!("{}:{}", h.fs, h.mount) } else { String::new() };
    vec![
        p.name.clone(),
        p.number.to_string(),
        user.to_string(),
        fd,
        type_name(h),
        device,
        size_off,
        if file { node.to_string() } else { String::new() },
        name,
    ]
}
