// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Raw access to disks: the `raw` and `errors` modes, for the `disk-raw`
//! and `disk-errors` boot tests. Each disk holds [`big_byte`]'s bytes
//! when the test starts, which the host checks afterwards with the same
//! lists of writes as these.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use libradix::abi::Status;
use libradix::abi::address_space;
use libradix::abi::block;
use libradix::abi::capability::Rights;
use libradix::abi::directory::OpenFlags;
use libradix::abi::errors::{File as FileError, Kernel};
use libradix::abi::file;
use libradix::abi::startup::SLOT_SPACE;
use radix_rt::fs::{Dir, Error, File};
use radix_rt::{SYS, println};

use crate::{big_byte, check, fail};

/// The writes of the `raw` mode: at these offsets, these many bytes, each
/// the complement of the one there before; then 20 bytes from 10 before
/// the end, of which 10 fit. tools/xtask/src/test.rs has the same list.
const RAW_WRITES: &[(u64, u64)] =
    &[(513, 3), (3 * 512 - 1, 2), (20 * 4096 + 100, 70_000), (1 << 20, 8192)];
/// Where the `raw` mode's transfers with anonymous memory go ([`lent`]),
/// past what it writes and discards, and which it leaves as it was: the
/// disks hold 8 MiB.
const LENT_AT: u64 = 5 << 20;
/// The range the `raw` mode discards, which the host finds zeroed.
const RAW_DISCARD: (u64, u64) = (4 << 20, 1 << 20);
/// The reads of the `raw` mode, before it writes: from these offsets
/// (back from the end, for the negative ones), these many bytes.
const RAW_READS: &[(i64, usize)] =
    &[(0, 1), (777, 1000), (3 * 4096 + 5, 200_000), (-100, 300), (0x7fff_0000, 512)];

/// The sectors of the `errors` mode's disk that QEMU fails: a read of
/// the first once, a read of the second and a write of the third every
/// time.
const FAILS_ONCE: u64 = 6000;
const FAILS_READING: u64 = 10_000;
const FAILS_WRITING: u64 = 12_000;

/// Sends block request `request` to `disk`, with the address of `value`,
/// which it reads or fills (radix-block(2)).
fn control(disk: &File, request: u32, value: &mut [u8]) -> Result<(), Status> {
    let address = value.as_mut_ptr() as u64;
    file::control(&SYS, disk.slot(), request, address, address, value.len() as u64).map(drop)
}

/// The number block request `request` writes, of `N` bytes.
fn ask<const N: usize>(disk: &File, request: u32) -> Result<u64, Status> {
    let mut value = [0u8; 8];
    control(disk, request, &mut value[..N])?;
    Ok(u64::from_le_bytes(value))
}

/// Discards `len` bytes of `disk` from `offset`.
fn discard(disk: &File, offset: u64, len: u64) -> Result<(), Status> {
    let mut range = [0u8; 16];
    range[..8].copy_from_slice(&offset.to_le_bytes());
    range[8..].copy_from_slice(&len.to_le_bytes());
    control(disk, block::DISCARD, &mut range)
}

/// Whether `status` is kernel error `error`.
fn is(status: Status, error: Kernel) -> bool {
    Kernel::from_status(status) == Some(error)
}

/// What a status names, for a failed check.
fn named(status: Status) -> String {
    format!("{}", Error::Status(status))
}

/// Opens `/devices/<name>` for reading and writing.
pub(crate) fn open(root: &Dir, name: &str) -> Option<File> {
    let rw = Rights::READ.union(Rights::WRITE);
    match root.open(&format!("devices/{name}"), OpenFlags::NONE, rw) {
        Ok(disk) => Some(disk),
        Err(err) => {
            fail(&format!("/devices/{name}: {err}"));
            None
        }
    }
}

/// Whether `bytes`, read from `offset`, are what the disk held before the
/// test, but the complement in the ranges of `written`.
fn as_held(offset: u64, bytes: &[u8], written: &[(u64, u64)]) -> bool {
    bytes.iter().zip(offset..).all(|(&byte, at)| {
        let flipped = written.iter().any(|&(from, len)| (from..from + len).contains(&at));
        let before = big_byte(at as usize);
        byte == if flipped { !before } else { before }
    })
}

/// The `raw` mode on `/devices/<name>`: Linux's block requests, raw reads and
/// writes at odd offsets, discards, flushes, and what is refused.
pub fn raw(root: &Dir, name: &str) {
    let Some(disk) = open(root, name) else { return };
    let asked = (|| {
        Ok::<_, Status>([
            ask::<8>(&disk, block::GET_SIZE_BYTES)?,
            ask::<8>(&disk, block::GET_SIZE_SECTORS)?,
            ask::<4>(&disk, block::GET_SECTOR_SIZE)?,
            ask::<4>(&disk, block::GET_PHYSICAL_BLOCK_SIZE)?,
            ask::<4>(&disk, block::GET_MINIMUM_IO)?,
            ask::<4>(&disk, block::GET_OPTIMAL_IO)?,
            ask::<4>(&disk, block::GET_ALIGNMENT_OFFSET)?,
            ask::<4>(&disk, block::GET_READ_ONLY)?,
            ask::<2>(&disk, block::GET_ROTATIONAL)?,
            ask::<4>(&disk, block::GET_DISCARD_ZEROES)?,
        ])
    })();
    let [
        size,
        sectors,
        sector,
        physical,
        minimum,
        optimal,
        alignment,
        read_only,
        rotational,
        zeroes,
    ] = match asked {
        Ok(values) => values,
        Err(status) => {
            return fail(&format!("/devices/{name}: a block request: {}", named(status)));
        }
    };
    println!(
        "storage: /devices/{name}: {size} bytes, {sectors} sectors of {sector} bytes, physical \
         blocks of {physical}, minimum and optimal I/O {minimum} and {optimal}, alignment \
         {alignment}, read-only {read_only}, rotational {rotational}, discarded sectors read as \
         zeros {zeroes}"
    );
    check(sectors * 512 == size, || format!("/devices/{name}: {sectors} sectors, {size} bytes"));
    let sys = |file: &str| {
        root.read_file(&format!("kernel/block/{name}/{file}"))
            .ok()
            .and_then(|text| String::from_utf8_lossy(&text).trim_end().parse::<u64>().ok())
    };
    for (file, want) in [
        ("size", Some(sectors)),
        ("queue/logical_block_size", Some(sector)),
        ("queue/physical_block_size", Some(physical)),
        ("queue/rotational", Some(rotational)),
        // Discards cover whole sectors, of 4 KiB on a disk of them.
        ("queue/discard_granularity", Some(sector)),
    ] {
        check(sys(file) == want, || format!("/kernel/block/{name}/{file} is {:?}", sys(file)));
    }
    let most = sys("queue/discard_max_bytes").unwrap_or(0);
    check(most >= sector, || format!("/kernel/block/{name}/queue/discard_max_bytes is {most}"));

    for &(from, len) in RAW_READS {
        let offset = if from < 0 { size - from.unsigned_abs() } else { from as u64 };
        let mut buffer = vec![0u8; len];
        let want = (len as u64).min(size.saturating_sub(offset)) as usize;
        match disk.read_at(offset, &mut buffer) {
            Ok(got) => check(got == want && as_held(offset, &buffer[..got], &[]), || {
                format!("/devices/{name}: a read of {len} bytes at {offset} gave {got}, or others")
            }),
            Err(err) => fail(&format!("/devices/{name}: a read of {len} bytes at {offset}: {err}")),
        }
    }
    println!("storage: /devices/{name}: raw reads at odd offsets read what it holds");

    let mut written = Vec::new();
    for &(offset, len) in RAW_WRITES.iter().chain(&[(size - 10, 20)]) {
        let data: Vec<u8> = (offset..offset + len).map(|at| !big_byte(at as usize)).collect();
        let fits = len.min(size - offset);
        match disk.write_at(offset, &data) {
            Ok(done) => check(done as u64 == fits, || {
                format!("/devices/{name}: a write of {len} bytes at {offset} wrote {done}")
            }),
            Err(err) => {
                fail(&format!("/devices/{name}: a write of {len} bytes at {offset}: {err}"));
            }
        }
        written.push((offset, fits));
        // What it wrote, and a sector and more each side, read back.
        let from = offset.saturating_sub(600);
        let mut buffer = vec![0u8; (offset + fits + 600).min(size).saturating_sub(from) as usize];
        match disk.read_at(from, &mut buffer) {
            Ok(got) => check(got == buffer.len() && as_held(from, &buffer, &written), || {
                format!("/devices/{name}: what was written at {offset} reads back otherwise")
            }),
            Err(err) => fail(&format!("/devices/{name}: a read at {from}: {err}")),
        }
    }
    match disk.write_at(size, b"x") {
        Err(err) if err.is(FileError::NoSpace) => {}
        other => fail(&format!("/devices/{name}: a write at the end: {other:?}, not no_space")),
    }
    println!("storage: /devices/{name}: raw writes at odd offsets read back");
    lent(&disk, name);

    for (offset, len) in [(513, 512), (512, 100), (size - 512, 1024)] {
        match discard(&disk, offset, len) {
            Err(status) if is(status, Kernel::InvalidArgument) => {}
            other => fail(&format!("/devices/{name}: a discard of {len} at {offset}: {other:?}")),
        }
    }
    match control(&disk, 0x5401, &mut [0u8; 60]) {
        Err(status) if is(status, Kernel::NotTerminal) => {}
        other => fail(&format!("/devices/{name}: TCGETS: {other:?}, not not_terminal")),
    }
    match root.open(&format!("devices/{name}"), OpenFlags::NONE, Rights::READ) {
        Ok(reader) => {
            match discard(&reader, 0, 512) {
                Err(status) if is(status, Kernel::MissingRights) => {}
                other => fail(&format!("/devices/{name}: a discard without writing: {other:?}")),
            }
            match reader.write_at(0, &[0; 512]) {
                Err(Error::Status(status)) if is(status, Kernel::MissingRights) => {}
                other => fail(&format!("/devices/{name}: a write without writing: {other:?}")),
            }
        }
        Err(err) => fail(&format!("/devices/{name}, to read: {err}")),
    }
    let (offset, len) = RAW_DISCARD;
    let discarded = discard(&disk, offset, len).map_err(named);
    check(discarded.is_ok(), || format!("/devices/{name}: a discard: {discarded:?}"));
    check(discard(&disk, 0, 0).is_ok(), || format!("/devices/{name}: an empty discard failed"));
    let synced = disk.sync();
    check(synced.is_ok(), || format!("/devices/{name}: fsync: {synced:?}"));
    let flushed = control(&disk, block::FLUSH, &mut []).map_err(named);
    check(flushed.is_ok(), || format!("/devices/{name}: BLKFLSBUF: {flushed:?}"));
    println!("storage: /devices/{name}: discards, flushes and refusals work");
}

/// Raw transfers between `disk` and anonymous memory, the memory C
/// programs have, which go straight between the disk and the memory when
/// both are on a sector's boundary (radix-files(7)): reads into pages not
/// touched yet and touched, from a page's start, a sector's and neither,
/// more than a mebibyte at a time; writes of the complement read back
/// through the heap, and the bytes before written back; and memory that
/// is read-only, which a read cannot fill. From [`LENT_AT`], which the
/// test leaves as it was.
fn lent(disk: &File, name: &str) {
    const LEN: usize = (2 << 20) + 4096 + 512;
    const LAST: usize = LEN.next_multiple_of(4096);
    let Some(memory) = radix_rt::vm::anonymous(LAST + 4096) else {
        return fail(&format!("/devices/{name}: no anonymous memory to lend"));
    };
    for (start, at, len) in [
        (0, LENT_AT, LEN),
        (0, LENT_AT + 4096, 1 << 20),
        (512, LENT_AT + 512, LEN),
        (4096 + 1, LENT_AT, 70_000),
        (0, LENT_AT + 100, 8192),
    ] {
        let buffer = &mut memory[start..start + len];
        match disk.read_at(at, buffer) {
            Ok(got) => check(got == len && as_held(at, buffer, &[]), || {
                format!(
                    "/devices/{name}: a read of {len} bytes at {at} into anonymous memory at {start}"
                )
            }),
            Err(err) => fail(&format!("/devices/{name}: a read of {len} bytes at {at}: {err}")),
        }
    }
    // Read back through the heap, which is copied, 64 KiB at a time.
    let mut back = vec![0u8; 64 << 10];
    for (start, at, len) in [(0, LENT_AT, LEN), (512, LENT_AT + 3 * 512, 100_000)] {
        for (flip, written) in [(true, &[(at, len as u64)][..]), (false, &[][..])] {
            let buffer = &mut memory[start..start + len];
            for (byte, at) in buffer.iter_mut().zip(at..) {
                *byte = if flip { !big_byte(at as usize) } else { big_byte(at as usize) };
            }
            let wrote = disk.write_at(at, buffer);
            check(wrote.as_ref().is_ok_and(|&n| n == len), || {
                format!(
                    "/devices/{name}: a write of {len} bytes at {at} from anonymous memory: {wrote:?}"
                )
            });
            for from in (at..at + len as u64).step_by(back.len()) {
                let n = back.len().min((at + len as u64 - from) as usize);
                let read = disk.read_at(from, &mut back[..n]);
                check(read.is_ok() && as_held(from, &back[..n], written), || {
                    format!(
                        "/devices/{name}: what was written at {at} from anonymous memory reads back \
                         otherwise at {from}"
                    )
                });
            }
        }
    }
    // Its last page read-only: written from, not read into.
    let last = &mut memory[LAST..];
    let address = last.as_ptr() as u64;
    let protected = address_space::protect(&SYS, SLOT_SPACE, address, 4096, Rights::READ);
    check(protected.is_ok(), || format!("/devices/{name}: making memory read-only: {protected:?}"));
    let wrote = disk.write_at(LENT_AT, last);
    check(wrote.as_ref().is_ok_and(|&n| n == 4096), || {
        format!("/devices/{name}: a write from read-only memory: {wrote:?}")
    });
    match disk.read_at(LENT_AT, last) {
        Err(Error::Status(status)) if is(status, Kernel::BadAddress) => {}
        other => fail(&format!("/devices/{name}: a read into read-only memory: {other:?}")),
    }
    let zeros = disk.read_at(LENT_AT, &mut back[..4096]);
    check(zeros.is_ok() && back[..4096].iter().all(|&byte| byte == 0), || {
        format!("/devices/{name}: what was written from read-only memory reads back otherwise")
    });
    let held: Vec<u8> = (0..4096).map(|at| big_byte(LENT_AT as usize + at)).collect();
    let restored = disk.write_at(LENT_AT, &held);
    check(restored.is_ok(), || format!("/devices/{name}: a write at {LENT_AT}: {restored:?}"));
    println!(
        "storage: /devices/{name}: raw transfers straight between the disk and anonymous memory"
    );
}

/// The `errors` mode on `/devices/<name>`: reads and writes of the sectors
/// QEMU fails, then of others.
pub fn errors(root: &Dir, name: &str) {
    let Some(disk) = open(root, name) else { return };
    let mut buffer = vec![0u8; 4096];
    let at = FAILS_ONCE * 512;
    match disk.read_at(at, &mut buffer) {
        Ok(4096) if as_held(at, &buffer, &[]) => println!(
            "storage: /devices/{name}: a read of sector {FAILS_ONCE}, which fails once, reads what it \
             holds"
        ),
        other => fail(&format!("/devices/{name}: the read that fails once: {other:?}")),
    }
    match disk.read_at(FAILS_READING * 512, &mut buffer) {
        Err(err) if err.is(FileError::Io) => println!(
            "storage: /devices/{name}: a read of sector {FAILS_READING}, which always fails, fails \
             with io"
        ),
        other => fail(&format!("/devices/{name}: the read that always fails: {other:?}")),
    }
    // Two requests: the first works, the second fails.
    let mut both = vec![0u8; 128 * 1024];
    let at = (FAILS_READING - 128) * 512;
    match disk.read_at(at, &mut both) {
        Ok(65536) if as_held(at, &both[..65536], &[]) => println!(
            "storage: /devices/{name}: a read up to sector {FAILS_READING} ends short, after 65536 \
             bytes"
        ),
        other => fail(&format!("/devices/{name}: the read up to the failing sector: {other:?}")),
    }
    match disk.write_at(FAILS_WRITING * 512, &[0; 4096]) {
        Err(err) if err.is(FileError::Io) => println!(
            "storage: /devices/{name}: a write of sector {FAILS_WRITING}, which always fails, fails \
             with io"
        ),
        other => fail(&format!("/devices/{name}: the write that always fails: {other:?}")),
    }
    // A write of what a sector holds already leaves the disk as the host
    // checks it.
    let at = 3000 * 512;
    let same: Vec<u8> = (at..at + 4096).map(|at| big_byte(at as usize)).collect();
    let wrote = disk.write_at(at, &same);
    let read = disk.read_at(2000 * 512, &mut buffer);
    if wrote == Ok(4096) && read == Ok(4096) && as_held(2000 * 512, &buffer, &[]) {
        println!("storage: /devices/{name}: reads and writes elsewhere work after the errors");
    } else {
        fail(&format!("/devices/{name}: after the errors, a write: {wrote:?}; a read: {read:?}"));
    }
}
