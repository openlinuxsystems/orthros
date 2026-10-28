// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! storage: the test program of the `storage` boot test, from
//! `/system/tests/storage` in the boot archive. Spark starts it with the
//! root directory, after mounting each FAT, exFAT or Btrfs disk at
//! `/volumes/<name>`.
//!
//! - `storage write` writes a set of files to every volume (short and long
//!   names, a file of many clusters, a directory of many entries, a
//!   rename, a removal, appends and a cut, and on Btrfs a file larger
//!   than its first data chunk), syncs, and checks `/tmp`, `/devices`,
//!   `/processes` and `/kernel`;
//! - `storage check`, after a reboot, reads every volume's files back and
//!   checks each byte, and the file the host wrote between the boots
//!   (with mtools, from `ob test`; none on exFAT), then checks
//!   `/processes` again;
//! - `storage raw <disk>...` reads and writes each disk in `/devices` raw at
//!   odd offsets, discards part of it, flushes it, and asks it Linux's
//!   block requests (the `disk-raw` boot test, [`disk::raw`]);
//! - `storage errors <disk>` reads and writes the sectors QEMU fails on
//!   it (the `disk-errors` boot test, [`disk::errors`]);
//! - `storage list` prints every file, directory and link of every volume,
//!   with each file's size and FNV-1a hash, and checks that the volumes
//!   refuse writes: the `btrfs` boot test's host compares what it prints
//!   with what Linux's `mkfs.btrfs` put there ([`list_volume`]);
//! - `storage churn` writes, syncs and renames a file at a time on the one
//!   volume, removing older ones, until the `btrfs-power-cut` boot test
//!   cuts the power ([`churn`]); `storage churned`, after the reboot,
//!   checks what the cut left and writes the volume again ([`churned`]).
//!
//! It prints a line per step, `storage: FAILED: ...` for each check that
//! fails, and exits with 0 only if none did.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

mod disk;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libradix::abi::capability::Rights;
use libradix::abi::directory::{FileKind, FilesystemKind, OpenFlags};
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
/// The size of `grow.bin`, which a Btrfs volume gets too: more than the
/// 8 MiB of the first data chunk `mkfs.btrfs` makes on the tests' disks,
/// so that Radix makes another.
const GROW: usize = 12 * 1024 * 1024 + 4321;
/// The size of `words.txt`, text a Btrfs volume gets too, which
/// compresses (ADR 0134): four extents' worth and a part.
const WORDS: usize = 4 * 128 * 1024 + 70_001;
/// The text of each 64-byte line of `words.txt`, after its number.
const WORDS_LINE: &[u8; 56] = b" words of a file that compresses, alike on each of them\n";
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

/// The byte at `offset` of `grow.bin`.
fn grow_byte(offset: usize) -> u8 {
    big_byte(offset).wrapping_add(1)
}

/// The byte at `offset` of `words.txt`: lines of 64 bytes, each its
/// number in eight digits and [`WORDS_LINE`].
fn words_byte(offset: usize) -> u8 {
    let (line, at) = (offset / 64, offset % 64);
    match at.checked_sub(8) {
        Some(at) => WORDS_LINE[at],
        None => b'0' + (line / 10usize.pow(7 - at as u32) % 10) as u8,
    }
}

/// How many files the churn keeps: each round removes the file of the
/// round this many before.
const CHURN_KEEP: usize = 8;
/// The churn's rounds, if the power is not cut first.
const CHURN_ROUNDS: usize = 2000;

/// The size of the churn's file of round `round`: from a byte to 192 KiB.
fn churn_size(round: usize) -> usize {
    1 + round * 37_813 % (192 * 1024)
}

/// The byte at `offset` of the churn's file of round `round`.
fn churn_byte(round: usize, offset: usize) -> u8 {
    big_byte(offset + round * 4099)
}

/// The round of a churn file's name, and whether it is the file written
/// before the rename (`temp-N`) rather than after it (`file-N`).
fn churn_name(name: &str) -> Option<(usize, bool)> {
    let (number, temp) = match name.strip_prefix("file-") {
        Some(number) => (number, false),
        None => (name.strip_prefix("temp-")?, true),
    };
    Some((number.parse().ok()?, temp))
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
    if volumes.is_empty() && matches!(mode, "write" | "check" | "list") {
        fail("no volumes are mounted under /volumes");
    }
    if volumes.len() != 1 && matches!(mode, "churn" | "churned") {
        fail(&format!("`{mode}` takes one volume, not {}", volumes.len()));
    }
    match mode {
        "raw" | "errors" => {
            let disks: Vec<&str> = args().skip(2).collect();
            if disks.is_empty() {
                fail(&format!("`{mode}` takes the names of disks in /devices"));
            }
            for name in disks {
                if mode == "raw" { disk::raw(&root, name) } else { disk::errors(&root, name) }
            }
        }
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
        "list" => {
            for name in &volumes {
                list_volume(&root, name);
            }
        }
        "churn" => {
            for name in &volumes {
                churn(&root, name);
            }
        }
        "churned" => {
            for name in &volumes {
                churned(&root, name);
            }
        }
        _ => {
            println!(
                "storage: FAILED: say `write`, `check`, `list`, `churn`, `churned`, `raw` or \
                 `errors`, not `{mode}`"
            );
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

/// FNV-1a's 64-bit start, the hash of nothing.
const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a, 64 bits, of `hash`'s bytes followed by `data`: the hash the
/// `btrfs` boot test's host takes of each file too.
fn fnv1a(hash: u64, data: &[u8]) -> u64 {
    data.iter().fold(hash, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

/// The length and FNV-1a hash of the file `path` of `dir`, read a piece
/// at a time: a program's heap is too small for some files whole.
fn hash_file(dir: &Dir, path: &str) -> radix_rt::fs::Result<(u64, u64)> {
    let file = dir.open_read(path)?;
    let mut piece = alloc::vec![0u8; 16 * 1024];
    let (mut len, mut hash) = (0u64, FNV_START);
    loop {
        let n = file.read_at(len, &mut piece)?;
        if n == 0 {
            return Ok((len, hash));
        }
        hash = fnv1a(hash, &piece[..n]);
        len += n as u64;
    }
}

/// Prints every file, directory and link of the read-only volume at
/// `/volumes/<name>`, a line each, in name order and depth first: a
/// file's size and hash, or the error reading it gave, or the file it is
/// another name of; a link's target. Then checks that a write fails.
fn list_volume(root: &Dir, name: &str) {
    let dir = match root.open_dir(&format!("volumes/{name}"), Rights::READ) {
        Ok(dir) => dir,
        Err(err) => return fail(&format!("cannot open /volumes/{name}: {err}")),
    };
    let mut seen = Vec::new();
    let mut count = 0;
    list_dir(&dir, name, "", &mut seen, &mut count);
    println!("storage: list {name}: {count} entries");
    let rw = Rights::READ.union(Rights::WRITE);
    let write = root.open_dir(&format!("volumes/{name}"), rw);
    match write.and_then(|dir| dir.write_file("new.txt", b"refused")) {
        Err(err) if err.is(FileError::ReadOnly) => {
            println!("storage: list {name}: writes fail with read_only");
        }
        Err(err) => fail(&format!("a write to /volumes/{name} failed with {err}, not read_only")),
        Ok(()) => fail(&format!("/volumes/{name} took a write")),
    }
}

/// [`list_volume`]'s lines for the directory `dir`, at `at` in the
/// volume; `seen` holds each file's ID and the first path it was found
/// at.
fn list_dir(dir: &Dir, volume: &str, at: &str, seen: &mut Vec<(u64, String)>, count: &mut usize) {
    let mut entries = match dir.entries() {
        Ok(entries) => entries,
        Err(err) => return fail(&format!("cannot list /volumes/{volume}{at}: {err}")),
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        *count += 1;
        let path = format!("{at}/{}", entry.name);
        let line = |what: &str| println!("storage: list {volume}: {what}");
        match entry.kind {
            FileKind::Directory => {
                line(&format!("dir {path}"));
                match dir.open_dir(&entry.name, Rights::READ) {
                    Ok(inner) => list_dir(&inner, volume, &path, seen, count),
                    Err(err) => fail(&format!("cannot open /volumes/{volume}{path}: {err}")),
                }
            }
            FileKind::Symlink => match dir.read_link(&entry.name) {
                Ok(target) => {
                    line(&format!("link {path} -> {}", String::from_utf8_lossy(&target)));
                }
                Err(err) => line(&format!("link {path}: {err}")),
            },
            FileKind::File => {
                let stat = match dir.stat(&entry.name) {
                    Ok(stat) => stat,
                    Err(err) => {
                        fail(&format!("cannot stat /volumes/{volume}{path}: {err}"));
                        continue;
                    }
                };
                if let Some((_, first)) = seen.iter().find(|(id, _)| *id == stat.id) {
                    line(&format!("file {path}: the same file as {first}"));
                    continue;
                }
                seen.push((stat.id, path.clone()));
                match hash_file(dir, &entry.name) {
                    Ok((len, hash)) => {
                        check(len == stat.size, || {
                            format!(
                                "/volumes/{volume}{path} read {len} bytes, but its size is {}",
                                stat.size
                            )
                        });
                        line(&format!("file {path}: {len} bytes, FNV-1a {hash:016x}"));
                    }
                    Err(err) => line(&format!("file {path}: {err}")),
                }
            }
            FileKind::Pipe => line(&format!("pipe {path}")),
            _ => line(&format!("other {path}")),
        }
    }
}

/// Writes the test's files to the volume at `/volumes/<name>`.
fn write_volume(root: &Dir, name: &str) {
    let rw = Rights::READ.union(Rights::WRITE);
    let dir = match root.open_dir(&format!("volumes/{name}"), rw) {
        Ok(dir) => dir,
        Err(err) => return fail(&format!("cannot open /volumes/{name}: {err}")),
    };
    let btrfs = match dir.info() {
        Ok(info) => {
            println!(
                "storage: /volumes/{name}: {}, {} KiB free of {} KiB in {}-byte clusters",
                info.kind.name(),
                info.free * u64::from(info.block_size) / 1024,
                info.blocks * u64::from(info.block_size) / 1024,
                info.block_size
            );
            info.kind == FilesystemKind::Btrfs
        }
        Err(err) => {
            fail(&format!("/volumes/{name}: info: {err}"));
            false
        }
    };
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

    // On Btrfs, more data than its first chunk holds, 64 KiB at a time:
    // the program's heap has no room for much larger pieces.
    if btrfs {
        step(
            "grow.bin",
            dir.create("grow.bin").and_then(|file| {
                let mut piece = alloc::vec![0u8; 64 * 1024];
                let mut at = 0;
                while at < GROW {
                    let len = piece.len().min(GROW - at);
                    for (i, byte) in piece[..len].iter_mut().enumerate() {
                        *byte = grow_byte(at + i);
                    }
                    file.write_all_at(at as u64, &piece[..len])?;
                    at += len;
                }
                Ok(GROW)
            }),
        );
        // Text, which a volume that compresses writes compressed.
        step(
            "words.txt",
            dir.create("words.txt").and_then(|file| {
                let mut piece = alloc::vec![0u8; 64 * 1024];
                let mut at = 0;
                while at < WORDS {
                    let len = piece.len().min(WORDS - at);
                    for (i, byte) in piece[..len].iter_mut().enumerate() {
                        *byte = words_byte(at + i);
                    }
                    file.write_all_at(at as u64, &piece[..len])?;
                    at += len;
                }
                Ok(WORDS)
            }),
        );
    }

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
    // No tool on Linux writes a file to exFAT without mounting it, which
    // takes the administrator, so the host writes none there; the exFAT
    // driver's tests read a volume exfat-fuse wrote instead.
    if !dir.info().is_ok_and(|info| info.kind == FilesystemKind::Exfat) {
        same(FROM_HOST, FROM_HOST_TEXT);
    }
    same(&format!("notes/{LONG_NAME}"), format!("Saved on {name}.\n").as_bytes());
    same("notes/final.txt", b"final\n");
    same("log.txt", b"one\ntwo\n");
    same("cut.txt", &[b'x'; 1000]);
    for i in 0..MANY {
        same(&format!("many/file-{i:02}.txt"), format!("file {i}\n").as_bytes());
    }
    // The big files, read a piece at a time.
    checked += usize::from(check_pattern(&dir, name, "big.bin", BIG, big_byte));
    if dir.info().is_ok_and(|info| info.kind == FilesystemKind::Btrfs) {
        checked += usize::from(check_pattern(&dir, name, "grow.bin", GROW, grow_byte));
        checked += usize::from(check_pattern(&dir, name, "words.txt", WORDS, words_byte));
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

/// Checks that `path` in the volume `name` is `len` bytes of `byte`'s,
/// reading it 16 KiB at a time; whether it could be read.
fn check_pattern(dir: &Dir, name: &str, path: &str, len: usize, byte: fn(usize) -> u8) -> bool {
    let file = match dir.open_read(path) {
        Ok(file) => file,
        Err(err) => {
            fail(&format!("/volumes/{name}/{path}: {err}"));
            return false;
        }
    };
    let mut piece = alloc::vec![0u8; 16 * 1024];
    let mut at = 0;
    let mut wrong = None;
    loop {
        match file.read_at(at as u64, &mut piece) {
            Ok(0) => break,
            Ok(n) => {
                if wrong.is_none() {
                    wrong = (0..n).find(|&i| piece[i] != byte(at + i)).map(|i| at + i);
                }
                at += n;
            }
            Err(err) => {
                fail(&format!("/volumes/{name}/{path} at {at}: {err}"));
                break;
            }
        }
    }
    check(at == len, || format!("/volumes/{name}/{path} is {at} bytes, not {len}"));
    if let Some(offset) = wrong {
        fail(&format!("/volumes/{name}/{path} differs at byte {offset}"));
    }
    true
}

/// `storage churn`: in each round, writes a file as `temp-N`, syncs it,
/// renames it `file-N` and syncs the directory, as a program saves a file
/// whole; then removes the file of [`CHURN_KEEP`] rounds before and syncs
/// again. Each step's line comes once it is synced, so the
/// `btrfs-power-cut` test knows what the disk must hold after the cut.
fn churn(root: &Dir, name: &str) {
    let rw = Rights::READ.union(Rights::WRITE);
    let dir = match root.open_dir(&format!("volumes/{name}"), rw) {
        Ok(dir) => dir,
        Err(err) => return fail(&format!("cannot open /volumes/{name}: {err}")),
    };
    for round in 0..CHURN_ROUNDS {
        let (temp, file) = (format!("temp-{round:04}"), format!("file-{round:04}"));
        let data: Vec<u8> = (0..churn_size(round)).map(|at| churn_byte(round, at)).collect();
        let saved = dir
            .create(&temp)
            .and_then(|made| {
                made.write_all_at(0, &data)?;
                made.sync()
            })
            .and_then(|()| dir.rename(&temp, &dir, &file))
            .and_then(|()| dir.sync());
        if let Err(err) = saved {
            return fail(&format!("/volumes/{name}/{file}: {err}"));
        }
        println!("storage: churn: synced {file}");
        if let Some(old) = round.checked_sub(CHURN_KEEP) {
            let old = format!("file-{old:04}");
            if let Err(err) = dir.remove(&old).and_then(|()| dir.sync()) {
                return fail(&format!("/volumes/{name}/{old}: removing it: {err}"));
            }
            println!("storage: churn: removed {old}");
        }
    }
    println!("storage: churn: done");
}

/// `storage churned`, after the power was cut during [`churn`]: each
/// `file-N` must be whole and each `temp-N` a start of itself (the boot
/// test's host checks which must be there); then they are removed, and a
/// file written, synced and read back.
fn churned(root: &Dir, name: &str) {
    let rw = Rights::READ.union(Rights::WRITE);
    let dir = match root.open_dir(&format!("volumes/{name}"), rw) {
        Ok(dir) => dir,
        Err(err) => return fail(&format!("cannot open /volumes/{name}: {err}")),
    };
    let entries = match dir.entries() {
        Ok(entries) => entries,
        Err(err) => return fail(&format!("cannot list /volumes/{name}: {err}")),
    };
    let (mut whole, mut unfinished) = (0, 0);
    for entry in entries {
        let data = match dir.read_file(&entry.name) {
            Ok(data) => data,
            Err(err) => {
                fail(&format!("/volumes/{name}/{}: {err}", entry.name));
                continue;
            }
        };
        if entry.name == FROM_HOST {
            check(data == FROM_HOST_TEXT, || format!("/volumes/{name}/{FROM_HOST} reads wrong"));
            continue;
        }
        let Some((round, temp)) = churn_name(&entry.name) else {
            fail(&format!("/volumes/{name} holds {}, which the churn never made", entry.name));
            continue;
        };
        let size = churn_size(round);
        let start = data.iter().enumerate().all(|(at, &byte)| byte == churn_byte(round, at));
        let sized = if temp { data.len() <= size } else { data.len() == size };
        check(start && sized, || {
            format!(
                "/volumes/{name}/{} holds {} bytes, not what the churn wrote ({size} bytes)",
                entry.name,
                data.len()
            )
        });
        if temp {
            unfinished += 1;
        } else {
            whole += 1;
        }
        if let Err(err) = dir.remove(&entry.name) {
            fail(&format!("/volumes/{name}/{}: removing it: {err}", entry.name));
        }
    }
    println!(
        "storage: /volumes/{name}: after the power cut, {whole} files whole and {unfinished} \
         unfinished"
    );
    let again = dir.write_file("after.txt", HELLO).and_then(|()| dir.sync());
    match again.and_then(|()| dir.read_file("after.txt")) {
        Ok(data) if data == HELLO => {
            println!("storage: /volumes/{name}: written again after the power cut");
        }
        Ok(_) => fail(&format!("/volumes/{name}/after.txt reads back wrong")),
        Err(err) => fail(&format!("/volumes/{name}/after.txt: {err}")),
    }
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

/// Reads `/devices/zero` and `/devices/random`, and writes `/devices/null`.
fn check_dev(root: &Dir) {
    let mut bytes = [0xffu8; 64];
    match root.open_read("devices/zero").and_then(|zero| zero.read_at(0, &mut bytes)) {
        Ok(n) => check(n == 64 && bytes.iter().all(|&b| b == 0), || {
            String::from("/devices/zero is not zeros")
        }),
        Err(err) => fail(&format!("/devices/zero: {err}")),
    }
    let mut random = [0u8; 64];
    match root.open_read("devices/random").and_then(|dev| dev.read_at(0, &mut random)) {
        Ok(n) => check(n == 64 && random.iter().any(|&b| b != 0), || {
            String::from("/devices/random gave zeros")
        }),
        Err(err) => fail(&format!("/devices/random: {err}")),
    }
    let written = root
        .open("devices/null", OpenFlags::NONE, Rights::WRITE)
        .and_then(|null| null.write_at(0, &random));
    match written {
        Ok(n) => check(n == 64, || format!("/devices/null took {n} of 64 bytes")),
        Err(err) => fail(&format!("/devices/null: {err}")),
    }
    match root.open_dir("devices", Rights::READ).and_then(|dev| dev.entries()) {
        Ok(entries) => {
            let disks = entries.iter().filter(|entry| entry.kind == FileKind::BlockDevice).count();
            println!(
                "storage: /devices: {} entries, {disks} of them disks; zero, random and null work",
                entries.len()
            );
        }
        Err(err) => fail(&format!("/devices: {err}")),
    }
}

/// Reads the size of each volume's disk in `/kernel/block`, and where
/// each partition is on its disk, and the kernel's version.
fn check_sys(root: &Dir, volumes: &[String]) {
    for name in volumes {
        match root.read_file(&format!("kernel/block/{name}/size")) {
            Ok(text) => {
                let text = String::from_utf8_lossy(&text);
                let sectors: Option<u64> = text.trim_end().parse().ok();
                check(sectors.is_some_and(|s| s > 0), || {
                    format!("/kernel/block/{name}/size says `{text}`")
                });
                println!("storage: /kernel/block/{name}/size: {} sectors", text.trim_end());
            }
            Err(err) => fail(&format!("/kernel/block/{name}/size: {err}")),
        }
        // A partition says its number and first sector, and has no queue;
        // a disk has a queue.
        let read = |file: &str| root.read_file(&format!("kernel/block/{name}/{file}"));
        match (read("partition"), read("start")) {
            (Ok(number), Ok(start)) => {
                let (number, start) =
                    (String::from_utf8_lossy(&number), String::from_utf8_lossy(&start));
                println!(
                    "storage: /kernel/block/{name}: partition {} from sector {}",
                    number.trim_end(),
                    start.trim_end()
                );
                check(read("queue/rotational").is_err(), || {
                    format!("partition {name} has a queue in /kernel/block")
                });
            }
            (Err(_), Err(_)) => {
                check(read("queue/rotational").is_ok(), || {
                    format!("disk {name} has no queue in /kernel/block")
                });
                // A SATA or NVMe disk says what it is; a virtio one cannot.
                if name.starts_with("sd") || name.starts_with("nvme") {
                    let text = |file: &str| {
                        read(file)
                            .map(|text| String::from(String::from_utf8_lossy(&text).trim_end()))
                    };
                    match (text("device/model"), text("device/serial"), text("device/firmware_rev"))
                    {
                        (Ok(model), Ok(serial), Ok(firmware))
                            if !model.is_empty() && !serial.is_empty() =>
                        {
                            println!(
                                "storage: /kernel/block/{name}/device: {model}, serial {serial}, \
                                 firmware {firmware}"
                            );
                        }
                        other => fail(&format!("/kernel/block/{name}/device gives {other:?}")),
                    }
                }
            }
            _ => fail(&format!("/kernel/block/{name} has only one of `partition` and `start`")),
        }
    }
    match root.read_file("kernel/version") {
        Ok(text) => {
            println!("storage: /kernel/version: {}", String::from_utf8_lossy(&text).trim_end());
        }
        Err(err) => fail(&format!("/kernel/version: {err}")),
    }
    // Every `vd` disk is a virtio block device, PCI vendor 0x1af4, every
    // `sd` disk is on an AHCI controller, class 0x010601, and every
    // `nvme` one on an NVMe controller, class 0x010802.
    match root.open_dir("kernel/bus/pci/devices", Rights::READ).and_then(|dir| dir.entries()) {
        Ok(functions) => {
            let count = |file: &str, value: &[u8]| {
                functions
                    .iter()
                    .filter(|function| {
                        root.read_file(&format!("kernel/bus/pci/devices/{}/{file}", function.name))
                            .is_ok_and(|text| text == value)
                    })
                    .count()
            };
            let (virtio, ahci) = (count("vendor", b"0x1af4\n"), count("class", b"0x010601\n"));
            let nvme = count("class", b"0x010802\n");
            let on = |prefix: &str| volumes.iter().filter(|name| name.starts_with(prefix)).count();
            check(
                virtio >= on("vd") && (ahci > 0 || on("sd") == 0) && (nvme > 0 || on("nvme") == 0),
                || {
                    format!(
                        "/kernel/bus/pci/devices lists {virtio} virtio functions, {ahci} AHCI \
                         ones and {nvme} NVMe ones"
                    )
                },
            );
            println!(
                "storage: /kernel/bus/pci/devices: {} functions, {virtio} of them virtio, {ahci} \
                 AHCI, {nvme} NVMe",
                functions.len()
            );
        }
        Err(err) => fail(&format!("/kernel/bus/pci/devices: {err}")),
    }
}

/// Lists the processes in `/processes`, which must include this one and
/// Spark, and counts the mounts in `/processes/mounts`.
fn check_proc(root: &Dir, me: u64) {
    let entries = match root.open_dir("processes", Rights::READ).and_then(|proc| proc.entries()) {
        Ok(entries) => entries,
        Err(err) => return fail(&format!("/processes: {err}")),
    };
    let mut listed = String::new();
    let mut numbers = Vec::new();
    for entry in entries.iter().filter(|entry| entry.kind == FileKind::Directory) {
        let Ok(number) = entry.name.parse::<u64>() else { continue };
        let status = match root.read_file(&format!("processes/{number}/status")) {
            Ok(status) => String::from_utf8_lossy(&status).into_owned(),
            Err(err) => {
                fail(&format!("/processes/{number}/status: {err}"));
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
    check(numbers.contains(&1), || String::from("/processes does not list Spark, process 1"));
    check(numbers.contains(&me), || format!("/processes does not list this process, {me}"));
    println!("storage: /processes lists {} processes: {listed}", numbers.len());
    match root.read_file("processes/mounts") {
        Ok(text) => {
            let text = String::from_utf8_lossy(&text);
            let fat = text.lines().filter(|line| line.contains(" fat ")).count();
            println!(
                "storage: /processes/mounts lists {} mounts, {fat} of them FAT",
                text.lines().count()
            );
        }
        Err(err) => fail(&format!("/processes/mounts: {err}")),
    }
    let number = |text: &str, key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.trim().trim_end_matches(" KiB").parse::<u64>().ok())
    };
    match (root.read_file("processes/meminfo"), root.read_file("processes/uptime")) {
        (Ok(memory), Ok(uptime)) => {
            let (memory, uptime) =
                (String::from_utf8_lossy(&memory), String::from_utf8_lossy(&uptime));
            let total = number(&memory, "total:").unwrap_or(0);
            let free = number(&memory, "free:").unwrap_or(0);
            let cache = number(&memory, "page_cache:");
            let up = number(&uptime, "uptime_ns:").unwrap_or(0);
            check(total > free && free > 0 && cache.is_some() && up > 0, || {
                format!("/processes/meminfo or /processes/uptime is wrong: `{memory}` `{uptime}`")
            });
            println!(
                "storage: /processes/meminfo: {total} KiB in all, {free} KiB free, {} KiB of page \
                 cache; /processes/uptime: {} ms",
                cache.unwrap_or(0),
                up / 1_000_000
            );
        }
        (Err(err), _) => fail(&format!("/processes/meminfo: {err}")),
        (_, Err(err)) => fail(&format!("/processes/uptime: {err}")),
    }
}
