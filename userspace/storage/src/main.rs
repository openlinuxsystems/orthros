// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! storage: the test program of the `storage` boot test, from
//! `/system/tests/storage` in the boot archive. Spark starts it with the
//! root directory, after mounting each FAT disk at `/volumes/<name>`.
//!
//! - `storage write` writes a set of files to every volume (short and long
//!   names, a file of many clusters, a directory of many entries, a
//!   rename, a removal, appends and a cut), syncs, and checks `/tmp`,
//!   `/dev`, `/proc` and `/sys`;
//! - `storage check`, after a reboot, reads every volume's files back and
//!   checks each byte, and the file the host wrote between the boots
//!   (with mtools, from `ob test`), then checks `/proc` again.
//!
//! It prints a line per step, `storage: FAILED: ...` for each check that
//! fails, and exits with 0 only if none did.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libradix::abi::capability::Rights;
use libradix::abi::directory::{FileKind, OpenFlags};
use libradix::abi::errors::File as FileError;
use libradix::abi::file::APPEND;
use libradix::abi::process;
use libradix::abi::startup::{SLOT_PROCESS, Startup};
use radix_rt::fs::Dir;
use radix_rt::{SYS, args, entry, println};

entry!(main);

/// The text of `hello.txt`.
const HELLO: &[u8] = b"Written by Dukovo before a reboot.\n";
/// The size of `big.bin`: more than a cluster on every FAT this test
/// makes, and not a whole number of them.
const BIG: usize = 200 * 1024 + 123;
/// The files in `many/`: more than one directory cluster's entries.
const MANY: usize = 40;
/// The long name the test uses, which needs long-name entries.
const LONG_NAME: &str = "A file with a long name, and spaces.txt";
/// The file the boot test's host writes to each volume between the boots,
/// with mtools, and its text.
const FROM_HOST: &str = "Written on the host.txt";
const FROM_HOST_TEXT: &[u8] = b"Written on the host between the boots.\n";

/// Failures so far.
static FAILURES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Records a failed check.
fn fail(what: &str) {
    FAILURES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    println!("storage: FAILED: {what}");
}

/// Checks `ok`, recording `what` if it is false.
fn check(ok: bool, what: impl FnOnce() -> String) {
    if !ok {
        fail(&what());
    }
}

/// The byte at `offset` of `big.bin`: different in every 4 KiB page and
/// at every offset within one, so a page out of place shows.
fn big_byte(offset: usize) -> u8 {
    (offset as u32).wrapping_mul(2_654_435_761).wrapping_add((offset / 4096) as u32).to_le_bytes()
        [3]
}

fn main(_: &Startup) -> u64 {
    let number = process::info(&SYS, SLOT_PROCESS).map_or(0, |info| info.number);
    let mode = args().nth(1).unwrap_or("");
    let Some(root) = Dir::root() else {
        println!("storage: FAILED: no root directory was given");
        return 1;
    };
    let volumes = match volumes(&root) {
        Ok(volumes) => volumes,
        Err(err) => {
            println!("storage: FAILED: cannot list /volumes: {err}");
            return 1;
        }
    };
    if volumes.is_empty() {
        fail("no volumes are mounted under /volumes");
    }
    match mode {
        "write" => {
            for name in &volumes {
                write_volume(&root, name);
            }
            check_tmp(&root);
            check_dev(&root);
            check_sys(&root, &volumes);
        }
        "check" => {
            for name in &volumes {
                check_volume(&root, name);
            }
        }
        _ => {
            println!("storage: FAILED: say `write` or `check`, not `{mode}`");
            return 1;
        }
    }
    check_proc(&root, number);
    let failures = FAILURES.load(core::sync::atomic::Ordering::Relaxed);
    if failures == 0 {
        println!("storage: {mode}: every check passed");
        0
    } else {
        println!("storage: {mode}: {failures} checks failed");
        1
    }
}

/// The names under `/volumes`.
fn volumes(root: &Dir) -> radix_rt::fs::Result<Vec<String>> {
    let entries = root.open_dir("volumes", Rights::READ)?.entries()?;
    let mut names: Vec<String> = entries.into_iter().map(|entry| entry.name).collect();
    names.sort();
    Ok(names)
}

/// Writes the test's files to the volume at `/volumes/<name>`.
fn write_volume(root: &Dir, name: &str) {
    let rw = Rights::READ.union(Rights::WRITE);
    let dir = match root.open_dir(&format!("volumes/{name}"), rw) {
        Ok(dir) => dir,
        Err(err) => return fail(&format!("cannot open /volumes/{name}: {err}")),
    };
    match dir.info() {
        Ok(info) => println!(
            "storage: /volumes/{name}: {}, {} KiB free of {} KiB in {}-byte clusters",
            info.kind.name(),
            info.free * u64::from(info.block_size) / 1024,
            info.blocks * u64::from(info.block_size) / 1024,
            info.block_size
        ),
        Err(err) => fail(&format!("/volumes/{name}: info: {err}")),
    }
    let mut written = 0usize;
    let mut files = 0usize;
    let mut step = |what: &str, result: radix_rt::fs::Result<usize>| match result {
        Ok(bytes) => {
            written += bytes;
            files += 1;
        }
        Err(err) => fail(&format!("/volumes/{name}: {what}: {err}")),
    };

    step("hello.txt", dir.write_file("hello.txt", HELLO).map(|()| HELLO.len()));

    // A file of many clusters, written a piece at a time in pieces that
    // do not line up with pages.
    step(
        "big.bin",
        dir.create("big.bin").and_then(|file| {
            let mut piece = alloc::vec![0u8; 12_345];
            let mut at = 0;
            while at < BIG {
                let len = piece.len().min(BIG - at);
                for (i, byte) in piece[..len].iter_mut().enumerate() {
                    *byte = big_byte(at + i);
                }
                file.write_all_at(at as u64, &piece[..len])?;
                at += len;
            }
            Ok(BIG)
        }),
    );

    // A long name, in a directory.
    step("make notes/", dir.make_dir("notes").map(|()| 0));
    let text = format!("Saved on {name}.\n");
    step(
        LONG_NAME,
        dir.write_file(&format!("notes/{LONG_NAME}"), text.as_bytes()).map(|()| text.len()),
    );

    // A rename across directories, and a removal.
    step("draft.txt", dir.write_file("draft.txt", b"final\n").map(|()| 6));
    step("rename draft.txt", dir.rename("draft.txt", &dir, "notes/final.txt").map(|()| 0));
    step("scratch.txt", dir.write_file("scratch.txt", b"gone soon\n").map(|()| 0));
    step("remove scratch.txt", dir.remove("scratch.txt").map(|()| 0));

    // Appends, and a cut.
    step(
        "log.txt",
        dir.create("log.txt").and_then(|file| {
            file.write_all_at(APPEND, b"one\n")?;
            file.write_all_at(APPEND, b"two\n")?;
            Ok(8)
        }),
    );
    step(
        "cut.txt",
        dir.create("cut.txt").and_then(|file| {
            file.write_all_at(0, &[b'x'; 5000])?;
            file.set_size(1000)?;
            Ok(1000)
        }),
    );

    // A directory of more entries than a cluster holds.
    step("make many/", dir.make_dir("many").map(|()| 0));
    for i in 0..MANY {
        let body = format!("file {i}\n");
        step(
            "many/",
            dir.write_file(&format!("many/file-{i:02}.txt"), body.as_bytes()).map(|()| body.len()),
        );
    }

    match dir.sync() {
        Ok(()) => println!(
            "storage: /volumes/{name}: wrote {files} files ({} KiB) and synced",
            written.div_ceil(1024)
        ),
        Err(err) => fail(&format!("/volumes/{name}: sync: {err}")),
    }
    // As POSIX's fsync, a file open only for reading is synced too.
    if let Err(err) = dir.open("hello.txt", OpenFlags::NONE, Rights::READ).and_then(|f| f.sync()) {
        fail(&format!("/volumes/{name}/hello.txt: sync, open for reading: {err}"));
    }
    // What is in the cache reads back now, before the reboot too.
    match dir.read_file("hello.txt") {
        Ok(data) => check(data == HELLO, || format!("/volumes/{name}/hello.txt reads back wrong")),
        Err(err) => fail(&format!("/volumes/{name}/hello.txt: {err}")),
    }
}

/// Checks the files `write_volume` wrote to `/volumes/<name>`.
fn check_volume(root: &Dir, name: &str) {
    let dir = match root.open_dir(&format!("volumes/{name}"), Rights::READ) {
        Ok(dir) => dir,
        Err(err) => return fail(&format!("cannot open /volumes/{name}: {err}")),
    };
    let read = |path: &str| -> Option<Vec<u8>> {
        match dir.read_file(path) {
            Ok(data) => Some(data),
            Err(err) => {
                fail(&format!("/volumes/{name}/{path}: {err}"));
                None
            }
        }
    };
    let mut checked = 0usize;
    let mut same = |path: &str, want: &[u8]| {
        if let Some(data) = read(path) {
            checked += 1;
            check(data == want, || {
                format!(
                    "/volumes/{name}/{path} holds {} bytes, not the {} written",
                    data.len(),
                    want.len()
                )
            });
        }
    };
    same("hello.txt", HELLO);
    same(FROM_HOST, FROM_HOST_TEXT);
    same(&format!("notes/{LONG_NAME}"), format!("Saved on {name}.\n").as_bytes());
    same("notes/final.txt", b"final\n");
    same("log.txt", b"one\ntwo\n");
    same("cut.txt", &[b'x'; 1000]);
    for i in 0..MANY {
        same(&format!("many/file-{i:02}.txt"), format!("file {i}\n").as_bytes());
    }
    // The big file, read a piece at a time.
    match dir.open_read("big.bin") {
        Ok(file) => {
            let mut piece = alloc::vec![0u8; 16 * 1024];
            let mut at = 0;
            let mut wrong = None;
            loop {
                match file.read_at(at as u64, &mut piece) {
                    Ok(0) => break,
                    Ok(n) => {
                        if wrong.is_none() {
                            wrong = (0..n).find(|&i| piece[i] != big_byte(at + i)).map(|i| at + i);
                        }
                        at += n;
                    }
                    Err(err) => {
                        fail(&format!("/volumes/{name}/big.bin at {at}: {err}"));
                        break;
                    }
                }
            }
            checked += 1;
            check(at == BIG, || format!("/volumes/{name}/big.bin is {at} bytes, not {BIG}"));
            if let Some(offset) = wrong {
                fail(&format!("/volumes/{name}/big.bin differs at byte {offset}"));
            }
        }
        Err(err) => fail(&format!("/volumes/{name}/big.bin: {err}")),
    }
    // What was renamed and removed has gone.
    for gone in ["draft.txt", "scratch.txt"] {
        match dir.stat(gone) {
            Err(err) if err.is(FileError::NotFound) => {}
            Ok(_) => fail(&format!("/volumes/{name}/{gone} is still there")),
            Err(err) => fail(&format!("/volumes/{name}/{gone}: {err}")),
        }
    }
    match dir.open_dir("many", Rights::READ).and_then(|many| many.entries()) {
        Ok(entries) => check(entries.len() == MANY, || {
            format!("/volumes/{name}/many lists {} entries, not {MANY}", entries.len())
        }),
        Err(err) => fail(&format!("/volumes/{name}/many: {err}")),
    }
    println!(
        "storage: /volumes/{name}: {checked} files read back after the reboot, every byte as written"
    );
}

/// Writes, reads back and removes a file in `/tmp`.
fn check_tmp(root: &Dir) {
    let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let result = root.write_file("tmp/scratch", &data).and_then(|()| root.read_file("tmp/scratch"));
    match result {
        Ok(back) => check(back == data, || String::from("/tmp/scratch reads back wrong")),
        Err(err) => return fail(&format!("/tmp/scratch: {err}")),
    }
    if let Err(err) = root.remove("tmp/scratch") {
        return fail(&format!("removing /tmp/scratch: {err}"));
    }
    println!("storage: /tmp: a 10000-byte file written, read back and removed");
}

/// Reads `/dev/zero` and `/dev/random`, and writes `/dev/null`.
fn check_dev(root: &Dir) {
    let mut bytes = [0xffu8; 64];
    match root.open_read("dev/zero").and_then(|zero| zero.read_at(0, &mut bytes)) {
        Ok(n) => check(n == 64 && bytes.iter().all(|&b| b == 0), || {
            String::from("/dev/zero is not zeros")
        }),
        Err(err) => fail(&format!("/dev/zero: {err}")),
    }
    let mut random = [0u8; 64];
    match root.open_read("dev/random").and_then(|dev| dev.read_at(0, &mut random)) {
        Ok(n) => check(n == 64 && random.iter().any(|&b| b != 0), || {
            String::from("/dev/random gave zeros")
        }),
        Err(err) => fail(&format!("/dev/random: {err}")),
    }
    let written = root
        .open("dev/null", OpenFlags::NONE, Rights::WRITE)
        .and_then(|null| null.write_at(0, &random));
    match written {
        Ok(n) => check(n == 64, || format!("/dev/null took {n} of 64 bytes")),
        Err(err) => fail(&format!("/dev/null: {err}")),
    }
    match root.open_dir("dev", Rights::READ).and_then(|dev| dev.entries()) {
        Ok(entries) => {
            let disks = entries.iter().filter(|entry| entry.kind == FileKind::BlockDevice).count();
            println!(
                "storage: /dev: {} entries, {disks} of them disks; zero, random and null work",
                entries.len()
            );
        }
        Err(err) => fail(&format!("/dev: {err}")),
    }
}

/// Reads the size of each volume's disk in `/sys/block`, and the kernel's
/// version.
fn check_sys(root: &Dir, volumes: &[String]) {
    for name in volumes {
        match root.read_file(&format!("sys/block/{name}/size")) {
            Ok(text) => {
                let text = String::from_utf8_lossy(&text);
                let sectors: Option<u64> = text.trim_end().parse().ok();
                check(sectors.is_some_and(|s| s > 0), || {
                    format!("/sys/block/{name}/size says `{text}`")
                });
                println!("storage: /sys/block/{name}/size: {} sectors", text.trim_end());
            }
            Err(err) => fail(&format!("/sys/block/{name}/size: {err}")),
        }
    }
    match root.read_file("sys/kernel/version") {
        Ok(text) => {
            println!("storage: /sys/kernel/version: {}", String::from_utf8_lossy(&text).trim_end());
        }
        Err(err) => fail(&format!("/sys/kernel/version: {err}")),
    }
    // Every disk is a virtio block device, PCI vendor 0x1af4.
    match root.open_dir("sys/bus/pci/devices", Rights::READ).and_then(|dir| dir.entries()) {
        Ok(functions) => {
            let virtio = functions
                .iter()
                .filter(|function| {
                    root.read_file(&format!("sys/bus/pci/devices/{}/vendor", function.name))
                        .is_ok_and(|vendor| vendor == b"0x1af4\n")
                })
                .count();
            check(virtio >= volumes.len(), || {
                format!("/sys/bus/pci/devices lists {virtio} virtio functions")
            });
            println!(
                "storage: /sys/bus/pci/devices: {} functions, {virtio} of them virtio",
                functions.len()
            );
        }
        Err(err) => fail(&format!("/sys/bus/pci/devices: {err}")),
    }
}

/// Lists the processes in `/proc`, which must include this one and
/// Spark, and counts the mounts in `/proc/mounts`.
fn check_proc(root: &Dir, me: u64) {
    let entries = match root.open_dir("proc", Rights::READ).and_then(|proc| proc.entries()) {
        Ok(entries) => entries,
        Err(err) => return fail(&format!("/proc: {err}")),
    };
    let mut listed = String::new();
    let mut numbers = Vec::new();
    for entry in entries.iter().filter(|entry| entry.kind == FileKind::Directory) {
        let Ok(number) = entry.name.parse::<u64>() else { continue };
        let status = match root.read_file(&format!("proc/{number}/status")) {
            Ok(status) => String::from_utf8_lossy(&status).into_owned(),
            Err(err) => {
                fail(&format!("/proc/{number}/status: {err}"));
                continue;
            }
        };
        let field = |key: &str| {
            status.lines().find_map(|line| line.strip_prefix(key)).map_or("?", str::trim)
        };
        listed.push_str(&format!(
            "{}{number} {} ({})",
            if numbers.is_empty() { "" } else { ", " },
            field("name:"),
            field("state:")
        ));
        numbers.push(number);
    }
    check(numbers.contains(&1), || String::from("/proc does not list Spark, process 1"));
    check(numbers.contains(&me), || format!("/proc does not list this process, {me}"));
    println!("storage: /proc lists {} processes: {listed}", numbers.len());
    match root.read_file("proc/mounts") {
        Ok(text) => {
            let text = String::from_utf8_lossy(&text);
            let fat = text.lines().filter(|line| line.contains(" fat ")).count();
            println!(
                "storage: /proc/mounts lists {} mounts, {fat} of them FAT",
                text.lines().count()
            );
        }
        Err(err) => fail(&format!("/proc/mounts: {err}")),
    }
    let number = |text: &str, key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.trim().trim_end_matches(" KiB").parse::<u64>().ok())
    };
    match (root.read_file("proc/meminfo"), root.read_file("proc/uptime")) {
        (Ok(memory), Ok(uptime)) => {
            let (memory, uptime) =
                (String::from_utf8_lossy(&memory), String::from_utf8_lossy(&uptime));
            let total = number(&memory, "total:").unwrap_or(0);
            let free = number(&memory, "free:").unwrap_or(0);
            let cache = number(&memory, "page_cache:");
            let up = number(&uptime, "uptime_ns:").unwrap_or(0);
            check(total > free && free > 0 && cache.is_some() && up > 0, || {
                format!("/proc/meminfo or /proc/uptime is wrong: `{memory}` `{uptime}`")
            });
            println!(
                "storage: /proc/meminfo: {total} KiB in all, {free} KiB free, {} KiB of page \
                 cache; /proc/uptime: {} ms",
                cache.unwrap_or(0),
                up / 1_000_000
            );
        }
        (Err(err), _) => fail(&format!("/proc/meminfo: {err}")),
        (_, Err(err)) => fail(&format!("/proc/uptime: {err}")),
    }
}
