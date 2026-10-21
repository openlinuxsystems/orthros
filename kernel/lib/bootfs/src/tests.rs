// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use proptest::prelude::*;

use super::write::Writer;
use super::*;

/// The archive `ob` writes, in small.
fn sample() -> Vec<u8> {
    let mut w = Writer::new();
    w.directory("system", 0o755, 1).unwrap();
    w.directory("system/bin", 0o755, 1).unwrap();
    w.file("system/bin/spark", 0o755, 2, b"\x7fELF spark").unwrap();
    w.directory("system/tests", 0o755, 1).unwrap();
    w.file("system/tests/empty", 0o644, 3, b"").unwrap();
    w.file("system/tests/hello", 0o755, 3, &[0x5a; 700]).unwrap();
    w.finish()
}

#[test]
fn reads_what_it_writes() {
    let bytes = sample();
    assert_eq!(bytes.len() % BLOCK, 0);
    let archive = open(&bytes).unwrap();
    assert_eq!(archive.len(), 6);
    let spark = archive.find("/system/bin/spark").unwrap();
    assert_eq!(spark.kind, Kind::File);
    assert_eq!(spark.mode, 0o755);
    assert_eq!(spark.mtime, 2);
    assert_eq!(spark.data, b"\x7fELF spark");
    assert_eq!(spark.path().to_string(), "system/bin/spark");
    assert_eq!(spark.path().file_name(), "spark");
    assert_eq!(archive.find("system/tests/hello").unwrap().data, &[0x5a; 700][..]);
    assert_eq!(archive.find("system/tests/empty").unwrap().data, b"");
    assert_eq!(archive.find("system").unwrap().kind, Kind::Directory);
    assert!(archive.find("system/bin/nothing").is_none());
    assert!(archive.find("system/bi").is_none());
    let names = |dir| archive.list(dir).map(|e| e.path().file_name()).collect::<Vec<_>>();
    assert_eq!(names("/"), ["system"]);
    assert_eq!(names("system"), ["bin", "tests"]);
    assert_eq!(names("/system/tests/"), ["empty", "hello"]);
    assert!(names("system/bin/spark").is_empty());
}

#[test]
fn long_paths_use_the_prefix() {
    let deep = format!("{}/{}", "d".repeat(120), "f".repeat(90));
    let mut w = Writer::new();
    w.file(&deep, 0o644, 0, b"x").unwrap();
    let bytes = w.finish();
    let archive = open(&bytes).unwrap();
    assert_eq!(archive.find(&deep).unwrap().data, b"x");
    assert_eq!(archive.entries().next().unwrap().path().to_string(), deep);
    let mut w = Writer::new();
    assert_eq!(w.file(&"x".repeat(MAX_PATH + 1), 0o644, 0, b""), Err(Error::BadPath));
    assert_eq!(w.file(&"y".repeat(150), 0o644, 0, b""), Err(Error::BadPath));
}

#[test]
fn writes_in_order_only() {
    let mut w = Writer::new();
    w.file("b", 0o644, 0, b"").unwrap();
    assert_eq!(w.file("a", 0o644, 0, b""), Err(Error::Order));
    assert_eq!(w.file("b", 0o644, 0, b""), Err(Error::Order));
    for bad in ["", "/", "c/../d", "c//d", "./c"] {
        assert_eq!(w.file(bad, 0o644, 0, b""), Err(Error::BadPath), "{bad:?}");
    }
}

/// Changes the header at `at` with `change` and puts its checksum right.
fn patched(at: usize, change: impl FnOnce(&mut [u8])) -> Vec<u8> {
    let mut bytes = sample();
    change(&mut bytes[at..at + BLOCK]);
    let header = &mut bytes[at..at + BLOCK];
    header[CHECKSUM.0..CHECKSUM.0 + CHECKSUM.1].fill(b' ');
    let sum: u64 = header.iter().map(|&b| u64::from(b)).sum();
    header[CHECKSUM.0..CHECKSUM.0 + 8].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    bytes
}

fn set(header: &mut [u8], (offset, len): (usize, usize), text: &[u8]) {
    header[offset..offset + len].fill(0);
    header[offset..offset + text.len()].copy_from_slice(text);
}

#[test]
fn refuses_what_it_must() {
    // The third entry, system/bin/spark, starts at block 2.
    let at = 2 * BLOCK;
    let cases: Vec<(Vec<u8>, Error)> = vec![
        (patched(at, |h| h[TYPE] = b'2'), Error::Unsupported),
        (patched(at, |h| h[TYPE] = b'3'), Error::Unsupported),
        (patched(at, |h| set(h, NAME, b"/system/bin/spark")), Error::BadPath),
        (patched(at, |h| set(h, NAME, b"system/../bin/spark")), Error::BadPath),
        (patched(at, |h| set(h, NAME, b"system/./spark")), Error::BadPath),
        (patched(at, |h| set(h, NAME, b"")), Error::BadPath),
        (patched(at, |h| set(h, NAME, b"system/bin\xff")), Error::BadPath),
        (patched(at, |h| set(h, NAME, b"system")), Error::Order),
        (patched(at, |h| set(h, NAME, b"a")), Error::Order),
        (patched(at, |h| set(h, SIZE, b"9999")), Error::BadNumber),
        (patched(at, |h| set(h, SIZE, b"")), Error::BadNumber),
        (patched(at, |h| set(h, SIZE, b"77777777777")), Error::Truncated),
        (patched(at, |h| set(h, MODE, b"12a")), Error::BadNumber),
        (patched(at, |h| set(h, (257, 6), b"tar\0\0\0")), Error::NotUstar),
        (patched(0, |h| set(h, SIZE, b"1")), Error::BadNumber),
    ];
    for (i, (bytes, want)) in cases.iter().enumerate() {
        assert_eq!(open(bytes).err(), Some(*want), "case {i}");
    }
    let mut bytes = sample();
    bytes[at + 10] ^= 1;
    assert_eq!(open(&bytes).err(), Some(Error::Checksum));
    assert_eq!(open(&sample()[..BLOCK + 100]).err(), Some(Error::Truncated));
    // A leading `./`, as tar writes it, is fine.
    let dotted = patched(at, |h| set(h, NAME, b"./system/bin/spark"));
    assert!(open(&dotted).unwrap().find("system/bin/spark").is_some());
    // Without the end blocks, the end of the file ends it.
    let bytes = sample();
    assert_eq!(open(&bytes[..bytes.len() - 2 * BLOCK]).unwrap().len(), 6);
    assert!(open(&[]).unwrap().is_empty());
}

proptest! {
    /// Garbage gives an error, never a panic.
    #[test]
    fn garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        if let Ok(archive) = open(&bytes) {
            let _ = archive.entries().count();
        }
    }

    /// Whatever bytes of a good archive are changed, what is accepted
    /// keeps every promise: data inside the file, paths named, relative
    /// and in order.
    #[test]
    fn accepted_archives_keep_their_promises(
        changes in proptest::collection::vec((0usize..7 * BLOCK, any::<u8>()), 0..6)
    ) {
        let mut bytes = sample();
        for (at, value) in changes {
            bytes[at] = value;
        }
        if let Ok(archive) = open(&bytes) {
            let mut last: Option<Vec<String>> = None;
            for entry in archive.entries() {
                let parts: Vec<String> = entry.path().components().map(str::to_owned).collect();
                prop_assert!(!parts.is_empty());
                prop_assert!(parts.iter().all(|p| p != "." && p != ".." && !p.is_empty()));
                prop_assert!(last.as_ref().is_none_or(|last| *last < parts));
                if !entry.data.is_empty() {
                    let data = entry.data.as_ptr() as usize - bytes.as_ptr() as usize;
                    prop_assert!(data + entry.data.len() <= bytes.len());
                }
                last = Some(parts);
            }
            prop_assert_eq!(archive.entries().count(), archive.len());
        }
    }
}
