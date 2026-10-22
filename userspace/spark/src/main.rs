// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Spark, the first program (Phases 6 and 7, spark(8)).
//!
//! The kernel starts Spark from the boot archive with the archive mapped
//! read-only, the root of the system's tree in `SLOT_ROOT` (an empty
//! tmpfs), and a memory object for the programs it starts
//! (`SLOT_FIRST_EXTRA`). Spark v1:
//!
//! 1. builds the directory hierarchy on the root (the plan's spec 7.1):
//!    `/system`, `/applications`, `/users`, `/data`, `/tmp`, `/run`,
//!    `/dev`, `/proc`, `/sys` and `/volumes`; and mounts the boot archive
//!    read-only on `/system`, tmpfs on `/system/etc`, `/system/var`,
//!    `/tmp` and `/run`, and the `dev`, `proc` and `sys` trees;
//! 2. mounts the FAT filesystem of each disk in `/dev` that holds one
//!    (each partition, or the whole disk if it has none) at
//!    `/volumes/<name>`;
//! 3. starts one program from `/system`, the one its arguments name
//!    (`/system/tests/hello` by default, with any arguments after it),
//!    with the root directory to read and write, logs over the kernel
//!    log, waits for the program to exit, reaps it and checks that all of
//!    its memory came back.
//!
//! Then it waits. Spark must not end: the kernel stops if it does. Later
//! phases make it the supervisor of the system's services.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libradix::abi::capability::{self, ObjectKind, Rights};
use libradix::abi::directory::{FileKind, FilesystemKind, MountFlags, OpenFlags};
use libradix::abi::errors::File as FileError;
use libradix::abi::notification::WaitFlags;
use libradix::abi::startup::{SLOT_FIRST_EXTRA, SLOT_PROCESS, Startup};
use libradix::abi::{memory, process, syscalls};
use radix_rt::bootfs::BootFs;
use radix_rt::fs::Dir;
use radix_rt::spawn::{self, Spawn};
use radix_rt::{SYS, args, entry, println, slots};

entry!(main);

/// The program Spark starts when its arguments name none.
const DEFAULT_PROGRAM: &str = "/system/tests/hello";

/// The memory the program it starts gets: 2^order pages.
const PROGRAM_MEMORY_ORDER: u32 = 9;

/// The most arguments it passes on.
const ARGS_MAX: usize = 16;

/// The directories of the root, every one a mount point or a place for
/// one (the plan's spec 7.1).
const HIERARCHY: [&str; 10] =
    ["system", "applications", "users", "data", "tmp", "run", "dev", "proc", "sys", "volumes"];

/// What is mounted where, in this order. `/system/etc`, `/system/var` and
/// `/system/boot` are directories of the boot archive's, for this.
const MOUNTS: [(&str, FilesystemKind, MountFlags); 8] = [
    ("system", FilesystemKind::Bootfs, MountFlags::READ_ONLY),
    ("system/etc", FilesystemKind::Tmpfs, MountFlags::NONE),
    ("system/var", FilesystemKind::Tmpfs, MountFlags::NONE),
    ("tmp", FilesystemKind::Tmpfs, MountFlags::NONE),
    ("run", FilesystemKind::Tmpfs, MountFlags::NONE),
    ("dev", FilesystemKind::Dev, MountFlags::NONE),
    ("proc", FilesystemKind::Proc, MountFlags::NONE),
    ("sys", FilesystemKind::Sys, MountFlags::NONE),
];

fn main(start: &Startup) -> u64 {
    let number = process::info(&SYS, SLOT_PROCESS).map_or(0, |info| info.number);
    println!("spark: Spark v1 is process {number}, moved by {:#x}", start.image_bias);
    run();
    // Spark never exits: it waits for good on a notification nothing
    // signals.
    println!("spark: nothing more to do; waiting");
    idle()
}

/// Builds the hierarchy, mounts the disks, then starts the program and
/// reaps it.
fn run() {
    let archive = match BootFs::mount() {
        Ok(archive) => archive,
        Err(err) => {
            println!("spark: {err}");
            return;
        }
    };
    println!(
        "spark: boot archive of {} KiB with {} files at {:#x}",
        archive.size().div_ceil(1024),
        archive.files(),
        libradix::startup().bootfs_address
    );
    let Some(root) = Dir::root() else {
        println!("spark: the kernel gave no root directory");
        return;
    };
    if !build(&root) {
        return;
    }
    mount_disks(&root);
    let mut words = args().skip(1);
    let path = words.next().unwrap_or(DEFAULT_PROGRAM);
    let mut rest = [""; ARGS_MAX];
    let mut count = 0;
    for word in words.take(ARGS_MAX) {
        rest[count] = word;
        count += 1;
    }
    let image = match root.read_file(path.trim_start_matches('/')) {
        Ok(image) => image,
        Err(err) => {
            println!("spark: cannot read {path}: {err}");
            return;
        }
    };
    let pool = SLOT_FIRST_EXTRA;
    let is_memory = capability::info(&SYS, pool).is_ok_and(|info| info.kind == ObjectKind::Memory);
    if !is_memory {
        println!("spark: the kernel gave no memory for programs");
        return;
    }
    let spawn = Spawn {
        path,
        image: &image,
        args: &rest[..count],
        pool,
        memory_order: PROGRAM_MEMORY_ORDER,
        share: &[],
        root: Some((root.slot(), Rights::READ.union(Rights::WRITE))),
    };
    let child = match spawn::spawn(&spawn) {
        Ok(child) => child,
        Err(err) => {
            println!("spark: could not start {path}: {err}");
            return;
        }
    };
    let number = child.number();
    println!("spark: started {path} as process {number}");
    match child.wait() {
        Ok(code) => println!("spark: process {number} ({path}) exited with code {code:#x}"),
        Err(err) => println!("spark: waiting for process {number} failed: {err}"),
    }
    child.reap();
    match memory::info(&SYS, pool) {
        Ok(info) if info.used == 0 => {
            println!("spark: reaped process {number}; all of its memory came back")
        }
        Ok(info) => {
            println!("spark: reaped process {number}, but {} pages are still used", info.used)
        }
        Err(status) => {
            println!("spark: reaped process {number}; memory info failed: {:#x}", status.0)
        }
    }
}

/// Makes the hierarchy's directories and mounts what goes on them.
/// Returns whether all of it worked.
fn build(root: &Dir) -> bool {
    for name in HIERARCHY {
        if let Err(err) = root.make_dir(name) {
            println!("spark: cannot make /{name}: {err}");
            return false;
        }
    }
    for (path, kind, flags) in MOUNTS {
        if let Err(err) = root.mount(path, kind, None, flags) {
            println!("spark: cannot mount {} on /{path}: {err}", kind.name());
            return false;
        }
    }
    println!(
        "spark: built the hierarchy of {} directories and mounted {} filesystems",
        HIERARCHY.len(),
        MOUNTS.len()
    );
    true
}

/// Mounts the FAT filesystem of each disk partition, or of each disk with
/// no partitions, at `/volumes/<name>`.
fn mount_disks(root: &Dir) {
    let devices: Vec<String> =
        match root.open_dir("dev", Rights::READ).and_then(|dev| dev.entries()) {
            Ok(entries) => entries
                .into_iter()
                .filter(|entry| entry.kind == FileKind::BlockDevice)
                .map(|entry| entry.name)
                .collect(),
            Err(err) => {
                println!("spark: cannot list /dev: {err}");
                return;
            }
        };
    let partitioned = |disk: &str| {
        devices.iter().any(|name| {
            name.strip_prefix(disk)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
    };
    for name in devices.iter().filter(|name| !partitioned(name)) {
        mount_disk(root, name);
    }
}

/// Mounts the FAT filesystem on `/dev/<name>` at `/volumes/<name>`,
/// read-only if the disk is.
fn mount_disk(root: &Dir, name: &str) {
    let rw = Rights::READ.union(Rights::WRITE);
    let (device, flags) = match root.open(&alloc::format!("dev/{name}"), OpenFlags::NONE, rw) {
        Ok(device) => (device, MountFlags::NONE),
        Err(err) if err.is(FileError::ReadOnly) => {
            match root.open(&alloc::format!("dev/{name}"), OpenFlags::NONE, Rights::READ) {
                Ok(device) => (device, MountFlags::READ_ONLY),
                Err(err) => return println!("spark: cannot open /dev/{name}: {err}"),
            }
        }
        Err(err) => return println!("spark: cannot open /dev/{name}: {err}"),
    };
    let at = alloc::format!("volumes/{name}");
    if let Err(err) = root.make_dir(&at) {
        return println!("spark: cannot make /{at}: {err}");
    }
    match root.mount(&at, FilesystemKind::Fat, Some(&device), flags) {
        Ok(()) => {
            let ro = if flags.contains(MountFlags::READ_ONLY) { ", read-only" } else { "" };
            println!("spark: mounted the FAT filesystem of {name} at /{at}{ro}");
        }
        Err(err) => {
            let _ = root.remove(&at);
            if err.is(FileError::UnknownFilesystem) {
                println!("spark: {name} holds no FAT filesystem; left alone");
            } else {
                println!("spark: cannot mount {name}: {err}");
            }
        }
    }
}

/// Waits for good.
fn idle() -> ! {
    let made = slots::take().filter(|&slot| {
        process::create(&SYS, SLOT_PROCESS, ObjectKind::Notification, 0, slot).is_ok()
    });
    loop {
        match made {
            Some(slot) => {
                let _ = syscalls::wait(&SYS, slot, WaitFlags::NONE, 0);
            }
            None => {
                let _ = syscalls::r#yield(&SYS);
            }
        }
    }
}
