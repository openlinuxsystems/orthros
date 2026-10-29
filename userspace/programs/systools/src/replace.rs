// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! replace(8): puts a new disk in the place of one of a Btrfs
//! filesystem's, there or missing, and rebuilds onto it, as Linux's
//! `btrfs replace start -B` does (ADR 0155), through the directory
//! interface (radix-directory(2)).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use radix_abi::directory::{self, ScrubProgress as P};
use radix_abi::errors::{File as FileError, Kernel};

use crate::args::{Arg, Args};
use crate::sys::{self, SYS};
use crate::util::{self, Fail};

/// Where a disk's first superblock is, and its magic and device ID in it.
const SUPERBLOCK: u64 = 0x10000;
const MAGIC_AT: usize = 0x40;
const DEVID_AT: usize = 0xc9;
const MAGIC: &[u8; 8] = b"_BHRfS_M";

/// The superblock of the disk `file`, if it holds a Btrfs.
fn superblock(file: &File) -> Option<[u8; 0x100]> {
    let mut bytes = [0; 0x100];
    file.read_exact_at(&mut bytes, SUPERBLOCK).ok()?;
    (&bytes[MAGIC_AT..MAGIC_AT + 8] == MAGIC).then_some(bytes)
}

/// The time `ns` as btrfs-progs prints a replace's start and end.
fn time(ns: u64) -> String {
    util::local_time((ns / 1_000_000_000) as i64, "%e.%b %T").trim_start().to_string()
}

/// replace(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let usage = || {
        util::usage("say start, the disk to replace or its devid, the new disk and a path".into())
    };
    let Some(Arg::Operand(action)) = args.next()? else { return Err(usage()) };
    if action != "start" {
        return Err(usage());
    }
    let (mut force, mut operands) = (false, Vec::new());
    while let Some(arg) = args.next()? {
        match arg {
            // It always waits for the rebuild, as -B asks.
            Arg::Short('B') => {}
            Arg::Short('f') => force = true,
            // Reading from the disk being replaced is all it does anyway.
            Arg::Short('r') => {}
            Arg::Operand(operand) if operands.len() < 3 => operands.push(operand),
            other => return Err(args.unknown(&other)),
        }
    }
    let [source, target, path] = &operands[..] else { return Err(usage()) };
    let path = PathBuf::from(path);
    let (shown, target_shown) = (source.to_string_lossy(), target.to_string_lossy());
    let devid = match shown.parse::<u64>() {
        Ok(devid) => devid,
        Err(_) => {
            let file = File::open(source).map_err(|e| util::failed(&shown, e))?;
            let bytes = superblock(&file)
                .ok_or_else(|| util::fail(format!("{shown}: not a btrfs device")))?;
            u64::from_le_bytes(bytes[DEVID_AT..DEVID_AT + 8].try_into().unwrap_or([0; 8]))
        }
    };
    let dir = sys::open_control(&path).map_err(|e| util::failed(&path.display(), e))?;
    let disk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(target)
        .map_err(|e| util::failed(&target_shown, e))?;
    if !force && superblock(&disk).is_some() {
        return Err(util::fail(format!(
            "{target_shown} appears to contain an existing filesystem (btrfs)\nUse the -f option \
             to force overwrite of {target_shown}"
        )));
    }
    let started = util::now_ns();
    let mut progress = [0u8; P::SIZE];
    let dir_slot = sys::slot_of(dir.as_raw_fd());
    let disk_slot = sys::slot_of(disk.as_raw_fd());
    if let Err(status) = directory::replace_disk(&SYS, dir_slot, devid, disk_slot, &mut progress) {
        let message = match (Kernel::from_status(status), FileError::from_status(status)) {
            (_, Some(FileError::NotFound)) => {
                format!("'{devid}' is not a valid devid for filesystem '{}'", path.display())
            }
            (Some(Kernel::NotSupported), _) => format!(
                "{}: not a btrfs filesystem of disks that hold copies of each other's",
                path.display()
            ),
            (_, Some(FileError::NoSpace)) => "target device smaller than source device".to_string(),
            (_, Some(FileError::InUse)) => format!("{target_shown} is mounted"),
            _ => return Err(util::failed(&path.display(), sys::io_error(status))),
        };
        return Err(util::fail(message));
    }
    let word = |at: usize| u64::from_le_bytes(progress[at..at + 8].try_into().unwrap_or([0; 8]));
    let uncorrectable = word(P::OFFSET_UNCORRECTABLE_ERRORS);
    let mut out = io::stdout();
    let _ = writeln!(
        out,
        "Started on {}, finished on {}, 0 write errs, {uncorrectable} uncorr. read errs",
        time(started),
        time(util::now_ns())
    );
    let _ = out.flush();
    Ok(if uncorrectable > 0 { 1 } else { 0 })
}
