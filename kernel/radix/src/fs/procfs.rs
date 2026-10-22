// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/proc`: the processes, and what the kernel holds, as plain text.
//!
//! Every file is text of `key: value` lines (radix-proc(5)), written when
//! it is opened, so a program reading it in pieces sees one snapshot:
//!
//! - `/proc/<number>/status`: a process's number, name, state, threads,
//!   domain and the kernel memory charged for it, for every process the
//!   kernel holds (those still running, and those ended but not reaped);
//! - `/proc/mounts`: one line per mount, `source /path kind rw|ro`, with
//!   the filesystem's label if it has one;
//! - `/proc/meminfo`: memory in KiB, in all, free, and by owner;
//! - `/proc/uptime`: nanoseconds since boot.

use core::fmt::{self, Write};

use radix_klog::TextBuf;
use radix_mm::{FRAMES, Owner, PAGE_SIZE};
use radix_vfs::synthetic::{Generated, Generator, ListedDir, Listing, SyntheticFs};
use radix_vfs::{Error, FsKind, FsRef, Kind, NodeRef, Result, Text, mem};

use crate::user::objects;

/// The files of `/proc` besides the processes' directories.
const FILES: [&str; 3] = ["meminfo", "mounts", "uptime"];

/// Node numbers: fixed ones for the root and its files, and two per
/// process above them.
const ROOT_ID: u64 = 1;
const PROCESS_ID_BASE: u64 = 16;

fn file_id(name: &str) -> u64 {
    FILES.iter().position(|f| *f == name).map_or(0, |i| 2 + i as u64)
}

/// A `proc` filesystem.
pub fn make() -> Result<FsRef> {
    let listing: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(Root)?;
    SyntheticFs::make(FsKind::Proc, ListedDir::make(ROOT_ID, listing)?)
}

/// The root: the files, then a directory per process, by number.
struct Root;

impl Listing for Root {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        for name in FILES {
            if !f(name, Kind::File) {
                return;
            }
        }
        let mut numbers = objects::process_numbers();
        numbers.sort_unstable();
        for number in numbers.iter() {
            let mut name = TextBuf::<24>::new();
            let _ = write!(name, "{number}");
            if !f(name.as_str(), Kind::Directory) {
                return;
            }
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let generate: fn(&mut Text) -> fmt::Result = match name {
            "meminfo" => meminfo,
            "mounts" => mounts,
            "uptime" => uptime,
            _ => {
                let number: u64 = name.parse().map_err(|_| Error::NotFound)?;
                if name.starts_with('0') || objects::process_by_number(number).is_none() {
                    return Err(Error::NotFound);
                }
                let listing: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(ProcessDir { number })?;
                return ListedDir::make(PROCESS_ID_BASE + 2 * number, listing);
            }
        };
        Generated::from_fn(file_id(name), generate)
    }
}

/// A process's directory.
struct ProcessDir {
    number: u64,
}

impl Listing for ProcessDir {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        f("status", Kind::File);
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        if name != "status" {
            return Err(Error::NotFound);
        }
        let number = self.number;
        let generate: Generator =
            mem::boxed(move |text: &mut Text| match objects::process_by_number(number) {
                Some(process) => process.describe(text),
                // It has gone since: an empty file.
                None => Ok(()),
            })?;
        Generated::make(PROCESS_ID_BASE + 2 * number + 1, generate)
    }
}

/// `/proc/mounts`.
fn mounts(text: &mut Text) -> fmt::Result {
    let Some(vfs) = super::vfs() else { return Ok(()) };
    let mut result = Ok(());
    vfs.mounts(|mount| {
        if result.is_err() {
            return;
        }
        let mut label = TextBuf::<64>::new();
        let _ = mount.fs.label(&mut label);
        result = (|| {
            write!(
                text,
                "{} /{} {} {}",
                mount.source(),
                mount.path(),
                mount.fs.kind().name(),
                if mount.read_only() { "ro" } else { "rw" }
            )?;
            if !label.as_str().is_empty() {
                write!(text, " label={}", label.as_str())?;
            }
            text.write_char('\n')
        })();
    });
    result
}

/// `/proc/meminfo`.
fn meminfo(text: &mut Text) -> fmt::Result {
    let kib = |pages: u64| pages * PAGE_SIZE as u64 / 1024;
    writeln!(text, "total: {} KiB", kib(FRAMES.managed_pages()))?;
    writeln!(text, "free: {} KiB", kib(FRAMES.free_pages()))?;
    for owner in Owner::ALL {
        let mut key = TextBuf::<32>::new();
        for c in owner.name().chars() {
            let _ = key.write_char(if c == ' ' || c == '-' { '_' } else { c.to_ascii_lowercase() });
        }
        writeln!(text, "{}: {} KiB", key.as_str(), kib(FRAMES.owner_pages(owner)))?;
    }
    Ok(())
}

/// `/proc/uptime`.
fn uptime(text: &mut Text) -> fmt::Result {
    writeln!(text, "uptime_ns: {}", crate::sched::now_ns())
}
