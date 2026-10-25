// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Spark, the first program (Phases 6, 7 and 9, spark(8)).
//!
//! The kernel starts Spark from the boot archive with the archive mapped
//! read-only, the root of the system's tree in `SLOT_ROOT` (an empty
//! tmpfs), and from `SLOT_FIRST_EXTRA` on a memory object for the programs
//! it starts, a capability to the `services` resource domain and the
//! power capability. Spark v2:
//!
//! 1. builds the directory hierarchy on the root (the plan's spec 7.1):
//!    `/system`, `/applications`, `/users`, `/data`, `/tmp`, `/run`,
//!    `/dev`, `/proc`, `/sys` and `/volumes`; and mounts the boot archive
//!    read-only on `/system`, tmpfs on `/system/etc`, `/system/var`,
//!    `/tmp` and `/run`, and the `dev`, `proc` and `sys` trees; makes
//!    `/system/var/tmp`; then copies each default setting in
//!    `/system/lib/defaults/etc` that `/system/etc` lacks into it;
//! 2. mounts the FAT filesystem of each disk in `/dev` that holds one
//!    (each partition, or the whole disk if it has none) at
//!    `/volumes/<name>`;
//! 3. listens for requests at `/run/spark/control` and, with no
//!    arguments (a normal boot), starts the services
//!    `/system/etc/spark/services` describes (`supervisor`);
//! 4. with arguments, as the boot tests boot it, starts one program
//!    from `/system`, the one they name, with the arguments after it, and
//!    starts no services unless `--services` comes first, or
//!    `--service-dir DIR` to start those that DIR describes instead. The
//!    program gets the root directory to read and write and logs over the
//!    kernel log; Spark waits for it to exit, reaps it and checks that all
//!    of its memory came back. With `--posix` before the program, it is a
//!    POSIX one (Phase 8, radix-posix(7)): Spark runs it with `exec`, as
//!    its child, with a pseudo-terminal as its standard input, output and
//!    error, and copies what it writes there to the kernel log.
//!
//! Then it supervises: it starts services again when they end, answers
//! the control socket and shuts the machine down when asked. Spark must
//! not end: the kernel stops if it does.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

mod services;
mod supervisor;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libradix::abi::capability::{self, ObjectKind, Rights};
use libradix::abi::directory::{FileKind, FilesystemKind, MountFlags, OpenFlags};
use libradix::abi::errors::File as FileError;
use libradix::abi::notification::WaitFlags;
use libradix::abi::startup::{
    SLOT_FD_BASE, SLOT_FIRST_EXTRA, SLOT_LOG, SLOT_PROCESS, SLOT_RANDOM, SLOT_ROOT, Startup,
};
use libradix::abi::{file, memory, process, syscalls, terminal};
use radix_rt::bootfs::BootFs;
use radix_rt::fs::{Dir, File};
use radix_rt::spawn::{self, Spawn};
use radix_rt::{SYS, args, entry, println, slots};
use supervisor::{POSIX_SLOTS, Program, SERVICES_DIR, Supervisor, exec_block};

entry!(main);

/// The memory the program it starts gets: 2^order pages.
const PROGRAM_MEMORY_ORDER: u32 = 9;

/// The most arguments it passes on.
const ARGS_MAX: usize = 16;

/// The directories of the root, every one a mount point or a place for
/// one (the plan's spec 7.1).
const HIERARCHY: [&str; 10] =
    ["system", "applications", "users", "data", "tmp", "run", "dev", "proc", "sys", "volumes"];

/// Symbolic links in the root to where Dukovo keeps what Unix programs
/// look for at the usual places: `/bin/sh`, `#!/usr/bin/env`,
/// `/etc/hosts`, `/usr/share/man` (ADR 0081).
const LINKS: [(&str, &str); 5] = [
    ("bin", "system/bin"),
    ("etc", "system/etc"),
    ("sbin", "system/bin"),
    ("usr", "system"),
    ("var", "system/var"),
];

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

/// Directories of the mounted tmpfs that Unix programs expect there:
/// `/var/tmp`, for temporary files that are to outlive a restart, such
/// as vi's recovery files, once `/system/var` is on a disk.
const MOUNTED_DIRS: [&str; 1] = ["system/var/tmp"];

fn main(start: &Startup) -> u64 {
    let number = process::info(&SYS, SLOT_PROCESS).map_or(0, |info| info.number);
    println!("spark: Spark v2 is process {number}, moved by {:#x}", start.image_bias);
    if let Some(mut supervisor) = run() {
        supervisor.run();
    }
    // Spark never exits: with nothing to supervise, it waits for good.
    println!("spark: nothing more to do; waiting");
    idle()
}

/// Builds the hierarchy and mounts the disks, then starts the services or
/// the program its arguments name; returns the supervision of them.
fn run() -> Option<Supervisor> {
    let archive = match BootFs::mount() {
        Ok(archive) => archive,
        Err(err) => {
            println!("spark: {err}");
            return None;
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
        return None;
    };
    if !build(&root) {
        return None;
    }
    seed_settings(&root);
    mount_disks(&root);
    let Ok(supervised) = Dir::at(root.slot()) else { return None };
    let mut supervisor = Supervisor::new(supervised);
    supervisor.listen();
    let mut words = args().skip(1).peekable();
    let (mut with_services, mut services) = (false, SERVICES_DIR);
    loop {
        if words.next_if_eq(&"--services").is_some() {
            with_services = true;
        } else if words.next_if_eq(&"--service-dir").is_some() {
            with_services = true;
            services = words.next().unwrap_or(SERVICES_DIR).trim_start_matches('/');
        } else {
            break;
        }
    }
    let posix = words.next_if_eq(&"--posix").is_some();
    let path = words.next();
    if with_services || path.is_none() {
        supervisor.start_services(services);
    }
    let Some(path) = path else { return Some(supervisor) };
    let mut rest = [""; ARGS_MAX];
    let mut count = 0;
    for word in words.take(ARGS_MAX) {
        rest[count] = word;
        count += 1;
    }
    if posix {
        match run_posix(&root, path, &rest[..count]) {
            Ok(program) => supervisor.watch_program(program),
            Err(what) => println!("spark: could not start {path}: {what}"),
        }
        return Some(supervisor);
    }
    run_native(&root, path, &rest[..count]);
    supervisor.note_idle();
    Some(supervisor)
}

/// Runs the native program at `path` with `args` and waits for it to end.
fn run_native(root: &Dir, path: &str, args: &[&str]) {
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
        args,
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
            println!("spark: reaped process {number}; all of its memory came back");
        }
        Ok(info) => {
            println!("spark: reaped process {number}, but {} pages are still used", info.used);
        }
        Err(status) => {
            println!("spark: reaped process {number}; memory info failed: {:#x}", status.0);
        }
    }
}

/// Starts the POSIX program at `path` with `args` (radix-posix(7)) as
/// Spark's child, on a new pseudo-terminal whose output goes to the
/// kernel log.
fn run_posix(root: &Dir, path: &str, args: &[&str]) -> Result<Program, String> {
    let (master, terminal) =
        open_pty(root).map_err(|what| format!("cannot make a pseudo-terminal: {what}"))?;
    let program = root
        .open(path.trim_start_matches('/'), OpenFlags::NONE, Rights::READ)
        .map_err(|err| format!("cannot open {path}: {err}"))?;
    let child = slots::take().ok_or("no free slot")?;
    let made = process::create(&SYS, SLOT_PROCESS, ObjectKind::Process, POSIX_SLOTS, child);
    if let Err(status) = made {
        slots::give_back(child);
        return Err(format!("cannot make a process: {:#x}", status.0));
    }
    // The root, the random source and the log as Spark has them, and the
    // terminal as descriptors 0, 1 and 2, all with the copy right, which
    // `fork` needs to copy them.
    let mut shares = [root.slot(), SLOT_RANDOM, SLOT_LOG]
        .into_iter()
        .zip([SLOT_ROOT, SLOT_RANDOM, SLOT_LOG])
        .chain((0..3).map(|fd| (terminal.slot(), SLOT_FD_BASE + fd)));
    let shared = shares.try_for_each(|(from, to)| {
        process::share(&SYS, child, from, to, Rights::ALL, 0).map_err(|status| (to, status))
    });
    let started = shared
        .map_err(|(to, status)| format!("sharing slot {to} failed: {:#x}", status.0))
        .and_then(|()| {
            process::exec(&SYS, child, program.slot(), &exec_block(path, args, &[]))
                .map_err(|status| format!("exec failed: {:#x}", status.0))
        });
    // The terminal is the program's now: the master reads its end once the
    // program and its children have closed it.
    drop(terminal);
    drop(program);
    let number = process::info(&SYS, child).map_or(0, |info| info.number);
    // The child stays in the tree after its capability goes, for `wait`.
    let _ = capability::delete(&SYS, child);
    slots::give_back(child);
    started?;
    println!("spark: started {path} as process {number}, on a pseudo-terminal");
    Ok(Program { path: path.into(), number, master, line: Vec::new(), hung_up: false })
}

/// Opens a new pseudo-terminal: its master, and the terminal (which does
/// not become Spark's controlling terminal).
fn open_pty(root: &Dir) -> Result<(File, File), String> {
    let rw = Rights::READ.union(Rights::WRITE);
    let master = root.open("dev/ptmx", OpenFlags::NONE, rw).map_err(|err| format!("{err}"))?;
    let mut number = [0u8; 4];
    let request = |request, buffer: &mut [u8; 4]| {
        let address = buffer.as_mut_ptr() as u64;
        file::control(&SYS, master.slot(), request, 0, address, 4)
            .map_err(|status| format!("request {request:#x} failed: {:#x}", status.0))
    };
    request(terminal::GET_PTY_NUMBER, &mut number)?;
    request(terminal::SET_PTY_LOCK, &mut [0; 4])?;
    let path = format!("dev/pts/{}", u32::from_le_bytes(number));
    let terminal =
        root.open(&path, OpenFlags::NO_TERMINAL, rw).map_err(|err| format!("/{path}: {err}"))?;
    Ok((master, terminal))
}

/// Makes the hierarchy's directories, mounts what goes on them and makes
/// the links to them. Returns whether all of it worked.
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
    for name in MOUNTED_DIRS {
        if let Err(err) = root.make_dir(name) {
            println!("spark: cannot make /{name}: {err}");
            return false;
        }
    }
    for (path, target) in LINKS {
        if let Err(err) = root.make_link(path, target) {
            println!("spark: cannot link /{path} to /{target}: {err}");
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

/// Where the system keeps its default settings, and where the settings
/// are, which start with each default that is not there yet: the plan's
/// way for updates to add settings without replacing edited ones
/// (spark(8)). `/system/etc` is a tmpfs until Btrfs, so each boot starts
/// it with all of them.
const DEFAULTS: &str = "system/lib/defaults/etc";
const SETTINGS: &str = "system/etc";

/// How deep the defaults' directories may nest.
const DEFAULTS_DEPTH: usize = 8;

/// Copies each default setting into `/system/etc` that is not there.
fn seed_settings(root: &Dir) {
    let Ok(defaults) = root.open_dir(DEFAULTS, Rights::READ) else {
        return;
    };
    let settings = match root.open_dir(SETTINGS, Rights::READ.union(Rights::WRITE)) {
        Ok(settings) => settings,
        Err(err) => {
            println!("spark: cannot open /{SETTINGS}: {err}");
            return;
        }
    };
    let mut added = 0;
    seed(&defaults, &settings, DEFAULTS_DEPTH, &mut added);
    println!("spark: added {added} default settings to /{SETTINGS}");
}

/// Copies the files beneath `from` that `to` lacks into it, with the
/// directories they are in, to `depth` levels; counts them in `added`.
fn seed(from: &Dir, to: &Dir, depth: usize, added: &mut usize) {
    let entries = match from.entries() {
        Ok(entries) => entries,
        Err(err) => {
            println!("spark: cannot list the default settings: {err}");
            return;
        }
    };
    for entry in entries {
        let name = entry.name.as_str();
        let done = match entry.kind {
            FileKind::Directory if depth > 1 => match to.make_dir(name) {
                Err(err) if !err.is(FileError::Exists) => Err(err),
                _ => from.open_dir(name, Rights::READ).and_then(|inner| {
                    let rw = Rights::READ.union(Rights::WRITE);
                    to.open_dir(name, rw).map(|out| seed(&inner, &out, depth - 1, added))
                }),
            },
            FileKind::File if to.stat(name).is_err() => {
                from.read_file(name).and_then(|data| to.write_file(name, &data)).map(|()| {
                    *added += 1;
                })
            }
            _ => Ok(()),
        };
        if let Err(err) = done {
            println!("spark: cannot add the default setting {name}: {err}");
        }
    }
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
pub(crate) fn idle() -> ! {
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
