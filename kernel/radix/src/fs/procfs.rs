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
//!   since Phase 9 also its place in the tree, its CPU time, scheduling,
//!   memory, page faults, I/O and working directory, what ps(1) and
//!   top(1) show;
//! - `/proc/<number>/cmdline`: the arguments its program started with,
//!   each ending with a NUL, as on Linux;
//! - `/proc/<number>/threads`: a line per thread, of `key=value` fields;
//! - `/proc/<number>/files`: a line per capability it holds, of
//!   `key=value` fields, with a file's or directory's path last: what
//!   lsof(1) lists;
//! - `/proc/mounts`: one line per mount, `source /path kind rw|ro`, with
//!   the filesystem's label if it has one; a space, tab, newline or
//!   backslash in a field is written as Linux writes it, `\040` and so on;
//! - `/proc/meminfo`: memory in KiB, in all, free, and by owner;
//! - `/proc/stat`: what each CPU has done, the threads ready to run and
//!   waiting for a disk, and what the disks have read and written;
//! - `/proc/loadavg`: the load averages;
//! - `/proc/uptime`: nanoseconds since boot.
//!
//! A newline or a backslash in a name or a path is written the same way,
//! so that it cannot end its line early.

use alloc::vec::Vec;
use core::fmt::{self, Write};

use radix_abi::process::{IoClass, ProcessState};
use radix_cap::{Capability, ObjectKind, Rights, SlotRef};
use radix_klog::TextBuf;
use radix_mm::{FRAMES, Kmalloc, Owner, PAGE_SIZE};
use radix_vfs::synthetic::{Generated, Generator, ListedDir, Listing, SyntheticFs};
use radix_vfs::{Error, FsKind, FsRef, Kind, NodeRef, Result, Text, mem};

use super::FileRef;
use crate::sched::{self, Run, ThreadRef};
use crate::user::UserRef;
use crate::user::objects::{self, Object};
use crate::user::posix;
use crate::user::process::ProcessRef;

/// The bytes a name or a path escapes in a line of fields: a newline
/// would end the line early, and a backslash begins an escape.
pub const LINE_SPECIAL: &[u8] = b"\n\\";

/// The bytes a field of `/proc/mounts` escapes, as on Linux.
const FIELD_SPECIAL: &[u8] = b" \t\n\\";

/// Writes `bytes`, with each of those in `special` as `/proc` on Linux
/// writes it: a backslash and three octal digits (`\040` for a space).
fn write_escaped(text: &mut Text, bytes: &[u8], special: &[u8]) -> fmt::Result {
    let mut start = 0;
    for (i, &byte) in bytes.iter().enumerate() {
        if special.contains(&byte) {
            text.write_bytes(&bytes[start..i])?;
            write!(text, "\\{byte:03o}")?;
            start = i + 1;
        }
    }
    text.write_bytes(&bytes[start..])
}

/// [`write_escaped`] for text, to any writer: the special bytes are
/// ASCII, so the text is cut only between characters.
pub fn write_escaped_str(out: &mut dyn Write, text: &str, special: &[u8]) -> fmt::Result {
    let mut start = 0;
    for (i, byte) in text.bytes().enumerate() {
        if special.contains(&byte) {
            out.write_str(&text[start..i])?;
            write!(out, "\\{byte:03o}")?;
            start = i + 1;
        }
    }
    out.write_str(&text[start..])
}

/// The files of `/proc` besides the processes' directories.
const FILES: [&str; 5] = ["loadavg", "meminfo", "mounts", "stat", "uptime"];

/// The files of a process's directory.
const PROCESS_FILES: [&str; 4] = ["cmdline", "files", "status", "threads"];

/// Node numbers: fixed ones for the root and its files, and one per
/// process and per file of its directory above them.
const ROOT_ID: u64 = 1;
const PROCESS_ID_BASE: u64 = 16;
const PER_PROCESS: u64 = 1 + PROCESS_FILES.len() as u64;

fn file_id(name: &str) -> u64 {
    FILES.iter().position(|f| *f == name).map_or(0, |i| 2 + i as u64)
}

fn heap() -> Kmalloc {
    radix_mm::HEAP.with(radix_mm::Flags::MAY_SLEEP)
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
        numbers.dedup();
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
            "loadavg" => loadavg,
            "meminfo" => meminfo,
            "mounts" => mounts,
            "stat" => stat,
            "uptime" => uptime,
            _ => {
                let number: u64 = name.parse().map_err(|_| Error::NotFound)?;
                if name.starts_with('0') || objects::process_by_number(number).is_none() {
                    return Err(Error::NotFound);
                }
                let listing: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(ProcessDir { number })?;
                return ListedDir::make(PROCESS_ID_BASE + PER_PROCESS * number, listing);
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
        for name in PROCESS_FILES {
            if !f(name, Kind::File) {
                return;
            }
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let index = PROCESS_FILES.iter().position(|f| *f == name).ok_or(Error::NotFound)?;
        let write: fn(&ProcessRef, &mut Text) -> fmt::Result = match name {
            "cmdline" => cmdline,
            "files" => files,
            "status" => status,
            _ => threads,
        };
        let number = self.number;
        let generate: Generator =
            mem::boxed(move |text: &mut Text| match objects::process_by_number(number) {
                Some(process) => write(&process, text),
                // It has gone since: an empty file.
                None => Ok(()),
            })?;
        Generated::make(PROCESS_ID_BASE + PER_PROCESS * number + 1 + index as u64, generate)
    }
}

/// A process's threads, with their program sides: taken out first, so
/// that what is written of them takes no lock of the process's.
fn threads_of(process: &ProcessRef) -> Vec<(ThreadRef, UserRef), Kmalloc> {
    let mut threads = Vec::new_in(heap());
    // Threads that start meanwhile are left out.
    if threads.try_reserve(process.thread_count()).is_err() {
        return threads;
    }
    process.for_each_thread(&mut |thread, user| {
        let _ = threads.push_within_capacity((thread.clone(), user.clone()));
    });
    threads
}

/// What a process or thread is doing, as ps(1)'s STAT names it in words.
fn run_name(run: Run) -> &'static str {
    match run {
        Run::Ready => "running",
        Run::Blocked("disk") => "disk",
        Run::Blocked(_) => "sleeping",
        Run::New => "new",
        Run::Dead => "dead",
    }
}

/// The name of a thread's scheduling policy.
fn policy_name(user: &UserRef) -> (&'static str, u8, i8) {
    let scheduling = *user.scheduling.lock();
    (scheduling.policy.name(), scheduling.priority, scheduling.nice)
}

/// `/proc/<number>/status`.
fn status(process: &ProcessRef, text: &mut Text) -> fmt::Result {
    process.describe(text)?;
    let number = process.number();
    let (phase, _) = process.state();
    let place = posix::tree_place(number);
    if let Some((parent, group, session, _)) = place {
        writeln!(text, "parent: {parent}")?;
        writeln!(text, "group: {group}")?;
        writeln!(text, "session: {session}")?;
    }
    match super::tty::controlling(process) {
        Some(tty) => writeln!(text, "terminal: pts/{}", tty.number())?,
        None => writeln!(text, "terminal: none")?,
    }
    let threads = threads_of(process);
    let run = if place.is_some_and(|(.., zombie)| zombie) || phase == ProcessState::Exited {
        "zombie"
    } else if process.stopped.load(core::sync::atomic::Ordering::Acquire) {
        "stopped"
    } else {
        // The most active of its threads' states.
        let runs = threads.iter().map(|(thread, _)| thread.run());
        let mut state = "sleeping";
        for run in runs {
            match run_name(run) {
                "running" => {
                    state = "running";
                    break;
                }
                "disk" => state = "disk",
                _ => {}
            }
        }
        state
    };
    writeln!(text, "run: {run}")?;
    writeln!(text, "started_ns: {}", process.started_ns())?;
    let (user_ns, system_ns) = process.cpu_times();
    writeln!(text, "cpu_ns: {}", user_ns + system_ns)?;
    writeln!(text, "user_ns: {user_ns}")?;
    writeln!(text, "system_ns: {system_ns}")?;
    // The scheduling of its first thread, as Linux shows a process's.
    let first = threads.iter().find(|(thread, _)| thread.id() == number).or(threads.first());
    if let Some((_, user)) = first {
        let (policy, priority, nice) = policy_name(user);
        writeln!(text, "policy: {policy}")?;
        writeln!(text, "priority: {priority}")?;
        writeln!(text, "nice: {nice}")?;
    }
    let (class, level) = process.io_priority();
    let class = IoClass::from_raw(class).unwrap_or(IoClass::None);
    // No class is served as best effort at level 4.
    let level = if class == IoClass::None { 4 } else { level };
    writeln!(text, "io_class: {}", class.name())?;
    writeln!(text, "io_level: {level}")?;
    if let Some(space) = process.space() {
        let kib = PAGE_SIZE as u64 / 1024;
        let (mapped, resident) = space.sizes();
        let (minor, major) = space.faults();
        writeln!(text, "virtual_memory: {} KiB", mapped * kib)?;
        writeln!(text, "resident_memory: {} KiB", resident * kib)?;
        writeln!(text, "minor_faults: {minor}")?;
        writeln!(text, "major_faults: {major}")?;
    }
    let (read, written) = process.io_bytes();
    writeln!(text, "read_bytes: {read}")?;
    writeln!(text, "written_bytes: {written}")?;
    let (voluntary, involuntary) = process.switch_counts();
    writeln!(text, "voluntary_switches: {voluntary}")?;
    writeln!(text, "involuntary_switches: {involuntary}")?;
    writeln!(text, "traced: {}", if process.traced() { "yes" } else { "no" })?;
    let directory = process.posix.lock().directory.clone();
    if let Some(directory) = directory {
        text.write_str("directory: ")?;
        write_escaped(text, &directory, LINE_SPECIAL)?;
        text.write_char('\n')?;
    }
    Ok(())
}

/// `/proc/<number>/cmdline`.
fn cmdline(process: &ProcessRef, text: &mut Text) -> fmt::Result {
    let mut result = Ok(());
    process.write_args(&mut |bytes| {
        if result.is_ok() {
            result = text.write_bytes(bytes);
        }
    });
    result
}

/// `/proc/<number>/threads`.
fn threads(process: &ProcessRef, text: &mut Text) -> fmt::Result {
    for (thread, user) in threads_of(process) {
        let (user_ns, system_ns) = thread.cpu_times();
        let (voluntary, involuntary) = thread.switch_counts();
        let (policy, priority, nice) = policy_name(&user);
        let run = thread.run();
        write!(
            text,
            "number={} run={} cpu={} user_ns={user_ns} system_ns={system_ns} \
             voluntary_switches={voluntary} involuntary_switches={involuntary} \
             policy={policy} priority={priority} nice={nice}",
            thread.id(),
            run_name(run),
            thread.cpu(),
        )?;
        if let Run::Blocked(reason) = run {
            // What it waits for, with no spaces: ps(1)'s WCHAN.
            text.write_str(" waits=")?;
            for c in reason.chars() {
                text.write_char(if c == ' ' { '_' } else { c })?;
            }
        }
        text.write_str(" name=")?;
        write_escaped_str(text, thread.name().as_str(), LINE_SPECIAL)?;
        text.write_char('\n')?;
    }
    Ok(())
}

/// The capabilities `process` holds, by slot.
fn held_caps(process: &ProcessRef) -> Vec<(u32, Capability), Kmalloc> {
    let mut caps = Vec::new_in(heap());
    let space = process.cspace();
    let Ok(Ok(slots)) = crate::caps::with(|store| store.slots(space)) else { return caps };
    let mut first = 0;
    while first < slots {
        let Ok(Ok(mut used)) = crate::caps::with(|store| store.used_slots(space, first)) else {
            break;
        };
        while used != 0 {
            let slot = first + used.trailing_zeros();
            used &= used - 1;
            let Ok(cap) = crate::caps::get(SlotRef::new(space, slot)) else { continue };
            if caps.try_reserve(1).is_ok() {
                let _ = caps.push_within_capacity((slot, cap));
            }
        }
        first += 64;
    }
    caps
}

/// The name of an object's kind in `/proc/<number>/files`, as one word.
fn object_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::AddressSpace => "address_space",
        ObjectKind::Irq => "irq",
        ObjectKind::IoPorts => "io_ports",
        ObjectKind::Mmio => "mmio",
        ObjectKind::Dma => "dma",
        ObjectKind::KernelLog => "kernel_log",
        kind => kind.name(),
    }
}

/// The name of a node's kind in `/proc/<number>/files`.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Directory => "directory",
        Kind::BlockDevice => "block",
        Kind::CharDevice => "char",
        Kind::Pipe => "pipe",
        Kind::Socket => "socket",
        Kind::Symlink => "link",
    }
}

/// `/proc/<number>/files`.
fn files(process: &ProcessRef, text: &mut Text) -> fmt::Result {
    let fd_base = radix_abi::startup::SLOT_FD_BASE;
    for (slot, cap) in held_caps(process) {
        if slot >= fd_base {
            write!(text, "fd={}", slot - fd_base)?;
        } else {
            write!(text, "slot={slot}")?;
        }
        let rights = cap.rights();
        let file: FileRef = match objects::object_of(&cap) {
            Ok(Object::File(file) | Object::Directory(file)) => file,
            // Any other capability: what it names, and its rights.
            _ => {
                writeln!(text, " kind={} rights={rights}", object_name(cap.kind()))?;
                continue;
            }
        };
        let mode = match (rights.contains(Rights::READ), rights.contains(Rights::WRITE)) {
            (true, true) => "rw",
            (true, false) => "r",
            (false, true) => "w",
            (false, false) => "-",
        };
        let stat = file.at.node.stat();
        write!(
            text,
            " kind={} access={mode} id={} size={} fs={} mount={} rights={rights}",
            kind_name(stat.kind),
            stat.id,
            stat.size,
            file.at.mount.fs.kind().name(),
            file.at.mount.id,
        )?;
        // A read or write that waits holds the offset: none is shown then.
        if let Some(offset) = file.offset.try_lock() {
            write!(text, " offset={}", *offset)?;
        }
        if let Some((path, cut)) = file.opened.path() {
            if cut {
                text.write_str(" cut=yes")?;
            }
            text.write_str(" path=")?;
            write_escaped(text, path, LINE_SPECIAL)?;
        }
        text.write_char('\n')?;
    }
    Ok(())
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
            write_escaped(text, mount.source().as_bytes(), FIELD_SPECIAL)?;
            text.write_str(" /")?;
            write_escaped(text, mount.path().as_bytes(), FIELD_SPECIAL)?;
            let access = if mount.read_only() { "ro" } else { "rw" };
            write!(text, " {} {access}", mount.fs.kind().name())?;
            if !label.as_str().is_empty() {
                text.write_str(" label=")?;
                write_escaped(text, label.as_str().as_bytes(), FIELD_SPECIAL)?;
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

/// `/proc/stat`: each online CPU's times, as `cpuN: key=value` lines, and
/// their sums.
fn stat(text: &mut Text) -> fmt::Result {
    let mut sum = sched::CpuTimes::default();
    let mut cpus = 0;
    for cpu in 0..radix_hal::MAX_CPUS {
        let Some(times) = sched::cpu_times(cpu) else { continue };
        cpus += 1;
        writeln!(
            text,
            "cpu{cpu}: user_ns={} system_ns={} idle_ns={} switches={}",
            times.user_ns, times.system_ns, times.idle_ns, times.switches
        )?;
        sum.user_ns += times.user_ns;
        sum.system_ns += times.system_ns;
        sum.idle_ns += times.idle_ns;
        sum.running += times.running;
        sum.switches += times.switches;
    }
    writeln!(text, "cpus: {cpus}")?;
    writeln!(text, "user_ns: {}", sum.user_ns)?;
    writeln!(text, "system_ns: {}", sum.system_ns)?;
    writeln!(text, "idle_ns: {}", sum.idle_ns)?;
    writeln!(text, "switches: {}", sum.switches)?;
    writeln!(text, "running: {}", sum.running)?;
    writeln!(
        text,
        "disk_waiting: {}",
        sched::DISK_WAITERS.load(core::sync::atomic::Ordering::Relaxed)
    )?;
    let count = |counter: &core::sync::atomic::AtomicU64| {
        counter.load(core::sync::atomic::Ordering::Relaxed)
    };
    writeln!(text, "disk_reads: {}", count(&super::disk::READS))?;
    writeln!(text, "disk_read_sectors: {}", count(&super::disk::READ_SECTORS))?;
    writeln!(text, "disk_writes: {}", count(&super::disk::WRITES))?;
    writeln!(text, "disk_written_sectors: {}", count(&super::disk::WRITTEN_SECTORS))?;
    let now = <crate::Arch as radix_hal::Timer>::wall_clock_ns().unwrap_or(0);
    writeln!(text, "boot_time_ns: {}", now.saturating_sub(sched::now_ns()))
}

/// `/proc/loadavg`: the averages, with two decimals as Linux prints them.
fn loadavg(text: &mut Text) -> fmt::Result {
    let averages = sched::load_average().hundredths();
    for (minutes, hundredths) in [1, 5, 15].iter().zip(averages) {
        writeln!(text, "load_{minutes}: {}.{:02}", hundredths / 100, hundredths % 100)?;
    }
    Ok(())
}

/// `/proc/uptime`.
fn uptime(text: &mut Text) -> fmt::Result {
    writeln!(text, "uptime_ns: {}", crate::sched::now_ns())
}
