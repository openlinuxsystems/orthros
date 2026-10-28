// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the decompressors and of compressed files, against what
//! the reference tools make: zlib streams from Python's `zlib` module,
//! zstd frames that the zstd tool made (`zstd/`), and filesystems that
//! `mkfs.btrfs --compress` made with each algorithm.
//!
//! The frames in `zstd/` hold [`sample`]s, written by the ignored test
//! `write_samples` into the directory `RADIX_BTRFS_SAMPLES` names and
//! compressed by zstd 1.5.5 as each `ZSTD` entry says.

use std::collections::BTreeMap;
use std::format;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::string::ToString;
use std::vec;
use std::vec::Vec;

use radix_vfs::{Error, FileSystem, SliceMut};

use super::files::{contents, mount, noise, same_tree};
use super::{XorShift, mkfs, open, scratch};
use crate::extent::{Compression, Extent, Regular};
use crate::format::{Key, item, tree};
use crate::inflate::Inflate;
use crate::lzo;
use crate::node::BtrfsNode;
use crate::tree::Cursor;
use crate::unpack::Workspace;
use crate::zstd::{Zstd, xxh64};

/// Data compressors find matches in, from `seed`: words from a short
/// list, numbers, runs of one byte and bits of noise, as logs and source
/// files have them.
fn sample(seed: u64, len: usize) -> Vec<u8> {
    const WORDS: [&str; 16] = [
        "the", "disk", "Dukovo", "reads", "Btrfs", "extent", "page", "cache", "zstd", "of", "a",
        "file", "and", "sector", "0x", "checksum",
    ];
    let mut random = XorShift(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut out = Vec::with_capacity(len + 512);
    while out.len() < len {
        match random.below(12) {
            0 => out.extend(std::iter::repeat_n(random.next() as u8, 1 + random.below(300))),
            1 => out.extend((0..=random.below(40)).map(|_| random.next() as u8)),
            2 | 3 => out.extend(format!("{} ", random.next() % 100_000).bytes()),
            _ => {
                out.extend(WORDS[random.below(WORDS.len())].bytes());
                out.push(if random.below(8) == 0 { b'\n' } else { b' ' });
            }
        }
    }
    out.truncate(len);
    out
}

/// Blocks of 4 KiB, by turns one byte repeated, noise and [`sample`]
/// text: zstd keeps the first as RLE blocks and the second raw.
fn blocks(seed: u64, count: usize) -> Vec<u8> {
    let mut random = XorShift(seed | 1);
    let mut out = Vec::new();
    for n in 0..count {
        match n % 3 {
            0 => out.extend([random.next() as u8; 4096]),
            1 => out.extend((0..4096).map(|_| random.next() as u8)),
            _ => out.extend(sample(seed + n as u64, 4096)),
        }
    }
    out
}

/// Bytes below 16, at random, from `seed`: literals with no matches, of
/// a short alphabet.
fn narrow(seed: u64, len: usize) -> Vec<u8> {
    let mut random = XorShift(seed | 1);
    (0..len).map(|_| (random.next() % 16) as u8).collect()
}

/// Noise, and then pieces of it again, each after one `x`: matches whose
/// literals are all one byte.
fn echoes(seed: u64, len: usize) -> Vec<u8> {
    let mut random = XorShift(seed | 1);
    let mut out: Vec<u8> = (0..600).map(|_| random.next() as u8).collect();
    while out.len() < len {
        out.push(b'x');
        let from = out.len() - 60 - random.below(500);
        let piece = out[from..from + 20 + random.below(40)].to_vec();
        out.extend(piece);
    }
    out.truncate(len);
    out
}

/// A zstd frame of `zstd/`: its name, its bytes, what it holds, and the
/// zstd options that made it (from a file, unless the input was a pipe,
/// which leaves the content size out).
type Frame = (&'static str, &'static [u8], fn() -> Vec<u8>, &'static str);

/// The zstd frames of `zstd/`.
const ZSTD: [Frame; 7] = [
    ("level-19", include_bytes!("zstd/level-19.zst"), || sample(1, 200_000), "-19"),
    ("piped", include_bytes!("zstd/piped.zst"), || sample(2, 100_000), "-3 < sample"),
    ("window-1k", include_bytes!("zstd/window-1k.zst"), || sample(3, 60_000), "-9 --zstd=wlog=10"),
    ("fast", include_bytes!("zstd/fast.zst"), || sample(4, 100_000), "--fast=4 --no-check"),
    ("blocks", include_bytes!("zstd/blocks.zst"), || blocks(5, 15), "-3 --no-check --zstd=wlog=12"),
    ("narrow", include_bytes!("zstd/narrow.zst"), || narrow(6, 40_000), "-1 --zstd=wlog=10"),
    ("echoes", include_bytes!("zstd/echoes.zst"), || echoes(7, 40_000), "-3 --zstd=wlog=10"),
];

#[test]
#[ignore = "writes the samples that zstd/ was made from"]
fn write_samples() {
    let dir = std::env::var_os("RADIX_BTRFS_SAMPLES").expect("RADIX_BTRFS_SAMPLES");
    for (name, _, make, _) in ZSTD {
        std::fs::write(Path::new(&dir).join(name), make()).unwrap();
    }
}

#[test]
fn xxh64_matches_its_specification() {
    for (data, sum) in [
        (&b""[..], 0xef46_db37_51d8_e999),
        (b"a", 0xd24e_c4f1_a98c_6e5b),
        (b"abc", 0x44bc_2cf5_ad77_0999),
        (b"Nobody inspects the spammish repetition", 0xfbce_a83c_8a37_8bf1),
    ] {
        assert_eq!(xxh64(data, 0), sum, "{:?}", std::str::from_utf8(data));
    }
}

/// `data` compressed by Python's `zlib`, in `pieces` with full flushes
/// between them (each an empty stored block).
fn zlib(
    data: &[u8],
    level: i32,
    wbits: i32,
    mem_level: i32,
    strategy: i32,
    pieces: usize,
) -> Vec<u8> {
    const SCRIPT: &str = "import sys, zlib\n\
        d = open(sys.argv[1], 'rb').read()\n\
        level, wbits, mem, strategy, pieces = map(int, sys.argv[2:7])\n\
        c = zlib.compressobj(level, zlib.DEFLATED, wbits, mem, strategy)\n\
        step = len(d) // pieces + 1\n\
        out = b''.join(c.compress(d[i:i + step]) + c.flush(zlib.Z_FULL_FLUSH) for i in range(0, len(d), step))\n\
        sys.stdout.buffer.write(out + c.flush())\n";
    let path = scratch("zlib");
    std::fs::write(&path, data).unwrap();
    let out = Command::new("python3")
        .args(["-I", "-c", SCRIPT])
        .arg(&path)
        .args([level, wbits, mem_level, strategy, pieces as i32].map(|n| n.to_string()))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "python3: {}", std::string::String::from_utf8_lossy(&out.stderr));
    out.stdout
}

#[test]
fn inflates_what_zlib_makes() {
    let mut inflate = Inflate::new().unwrap();
    let inputs = [
        vec![],
        vec![7],
        sample(10, 100_000),
        noise(11, 20_000),
        blocks(12, 6),
        b"ab".repeat(5000),
    ];
    // Level, window bits, memory level, strategy (default, filtered,
    // Huffman only, RLE, fixed) and pieces.
    let params = [
        (0, 15, 8, 0, 1),
        (1, 15, 8, 0, 1),
        (6, 15, 8, 0, 1),
        (9, 15, 9, 0, 1),
        (9, 9, 1, 0, 1),
        (6, 12, 8, 1, 1),
        (6, 15, 8, 2, 1),
        (6, 15, 8, 3, 1),
        (6, 15, 8, 4, 1),
        (6, 15, 8, 0, 4),
    ];
    for data in &inputs {
        for (level, wbits, mem, strategy, pieces) in params {
            let what = format!("{} bytes, {level} {wbits} {mem} {strategy} {pieces}", data.len());
            let stream = zlib(data, level, wbits, mem, strategy, pieces);
            let mut out = vec![0xaa; data.len() + 100];
            assert_eq!(inflate.zlib(&stream, &mut out), Ok(data.len()), "{what}");
            assert!(out[..data.len()] == data[..], "{what}");
            // A buffer the data fills ends the inflation there.
            let mut half = vec![0; data.len() / 2];
            assert_eq!(inflate.zlib(&stream, &mut half), Ok(half.len()), "{what}");
            assert!(half[..] == data[..half.len()], "{what}");
            // A stream cut short fails.
            if data.len() > 100 {
                let cut = &stream[..stream.len() / 2];
                assert_eq!(inflate.zlib(cut, &mut out), Err(Error::Io), "{what}");
            }
        }
    }
    // Headers Linux refuses: another method, a window over 32 KiB, a
    // check that fails, and a preset dictionary.
    let stream = zlib(b"header", 6, 15, 8, 0, 1);
    for (byte, value) in [(0, 0x79), (0, 0x88), (1, stream[1] ^ 1)] {
        let mut damaged = stream.clone();
        damaged[byte] = value;
        assert_eq!(inflate.zlib(&damaged, &mut [0; 16]), Err(Error::Io));
    }
    let mut dictionary = stream.clone();
    dictionary[1] = 0xbb;
    assert_eq!(inflate.zlib(&dictionary, &mut [0; 16]), Err(Error::Io));
    assert_eq!(inflate.zlib(&stream, &mut [0; 16]), Ok(6));
}

#[test]
fn decodes_what_zstd_makes() {
    // With BMI2's shifts where the host has them, and without.
    for mut zstd in [Zstd::new().unwrap(), Zstd::new().unwrap().without_bmi2()] {
        for (name, frame, make, _) in ZSTD {
            let data = make();
            let mut out = vec![0xaa; data.len() + 100];
            assert_eq!(zstd.decompress(frame, &mut out), Ok(data.len()), "{name}");
            assert!(out[..data.len()] == data[..], "{name}");
            // A buffer the data fills ends the frame there.
            let mut part = vec![0; data.len() / 3];
            assert_eq!(zstd.decompress(frame, &mut part), Ok(part.len()), "{name}");
            assert!(part[..] == data[..part.len()], "{name}");
            // A frame cut short fails, and so does a checksum that does
            // not match.
            let cut = &frame[..frame.len() - 5];
            assert_eq!(zstd.decompress(cut, &mut out), Err(Error::Io), "{name}");
            if frame[4] & 0x04 != 0 {
                let mut damaged = frame.to_vec();
                *damaged.last_mut().unwrap() ^= 1;
                assert_eq!(zstd.decompress(&damaged, &mut out), Err(Error::Io), "{name}");
            }
        }
    }
    let mut zstd = Zstd::new().unwrap();
    // Frames of one raw or RLE block, by hand: single segment, a content
    // size of 1 byte, and a checksum when asked.
    let frame = |flags: u8, size: u8, block: &[u8]| {
        let mut f = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20 | flags, size];
        f.extend(block);
        f
    };
    let mut out = [0; 64];
    assert_eq!(zstd.decompress(&frame(0, 0, &[1, 0, 0]), &mut out), Ok(0));
    assert_eq!(zstd.decompress(&frame(0, 5, &[0x2b, 0, 0, b'z']), &mut out), Ok(5));
    assert_eq!(&out[..5], b"zzzzz");
    assert_eq!(zstd.decompress(&frame(0, 3, &[0x19, 0, 0, b'a', b'b', b'c']), &mut out), Ok(3));
    assert_eq!(&out[..3], b"abc");
    // A compressed block of literals and no sequences: RLE, then raw.
    assert_eq!(zstd.decompress(&frame(0, 10, &[0x1d, 0, 0, 0x51, b'q', 0]), &mut out), Ok(10));
    assert_eq!(&out[..10], b"qqqqqqqqqq");
    assert_eq!(
        zstd.decompress(&frame(0, 3, &[0x2d, 0, 0, 0x18, b'a', b'b', b'c', 0]), &mut out),
        Ok(3)
    );
    assert_eq!(&out[..3], b"abc");
    let mut checked = frame(0x04, 3, &[0x19, 0, 0, b'a', b'b', b'c']);
    checked.extend((xxh64(b"abc", 0) as u32).to_le_bytes());
    assert_eq!(zstd.decompress(&checked, &mut out), Ok(3));
    *checked.last_mut().unwrap() ^= 0x80;
    assert_eq!(zstd.decompress(&checked, &mut out), Err(Error::Io));
    // What it refuses: a content size that is not what the frame makes,
    // a reserved bit, a dictionary, another magic number, and a reserved
    // block type.
    for bad in [
        frame(0, 6, &[0x2b, 0, 0, b'z']),
        frame(0x08, 0, &[1, 0, 0]),
        vec![0x28, 0xb5, 0x2f, 0xfd, 0x01, 0, 7, 1, 0, 0],
        vec![0x29, 0xb5, 0x2f, 0xfd, 0x20, 0, 1, 0, 0],
        frame(0, 0, &[7, 0, 0]),
    ] {
        assert_eq!(zstd.decompress(&bad, &mut out), Err(Error::Io), "{bad:x?}");
    }
}

/// An LZO1X stream of `data` as literals only: one run, then the end.
fn lzo_literals(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match data.len() {
        0 => {}
        n @ 1..=238 => out.push(17 + n as u8),
        n => {
            // A long run: 3 + 15 + the extended length.
            let extended = n - 18;
            out.push(0);
            out.extend(vec![0; (extended - 1) / 255]);
            out.push((extended - (extended - 1) / 255 * 255) as u8);
        }
    }
    out.extend(data);
    out.extend([0x11, 0, 0]);
    out
}

#[test]
fn lzo_instructions_decode_as_documented() {
    let run = |stream: &[u8]| {
        let mut out = vec![0; 20_000];
        lzo::decompress(stream, &mut out).map(|n| out[..n].to_vec())
    };
    assert_eq!(run(&[0x11, 0, 0]), Ok(vec![]));
    assert_eq!(run(&lzo_literals(b"hello")), Ok(b"hello".to_vec()));
    let long = sample(20, 3000);
    assert_eq!(run(&lzo_literals(&long)), Ok(long.clone()));
    // A match of 4 bytes from 4 back (64 to 255: 3 to 8 bytes, up to
    // 2 KiB back), then one literal, then a match of 2 bytes from 2 back
    // (below 16 after 1 to 3 literals: up to 1 KiB back).
    let stream = [21, b'a', b'b', b'c', b'd', 0x6d, 0, b'x', 0x04, 0, 0x11, 0, 0];
    assert_eq!(run(&stream), Ok(b"abcdabcdxdx".to_vec()));
    // After a run of 4 or more literals, below 16 is 3 bytes from 2 KiB
    // to 3 KiB back: 2100 here.
    let mut stream = lzo_literals(&long[..2100]);
    stream.truncate(stream.len() - 3);
    stream.extend([0x0c, 12, 0x11, 0, 0]);
    let mut want = long[..2100].to_vec();
    want.extend_from_slice(&long[..3]);
    assert_eq!(run(&stream), Ok(want));
    // 32 to 63, with a length past its field: 31 + 5 + 2 bytes from 2 back.
    let stream = [19, b'a', b'b', 0x20, 5, 0x04, 0, 0x11, 0, 0];
    assert_eq!(run(&stream), Ok(b"ab".repeat(20)));
    // 16 to 31: from 16 KiB and more back.
    let far = sample(21, 16_400);
    let mut stream = lzo_literals(&far);
    stream.truncate(stream.len() - 3);
    stream.extend([0x11, 10 << 2, 0, 0x11, 0, 0]);
    let mut want = far.clone();
    want.extend_from_slice(&far[16_400 - 16_394..][..3]);
    assert_eq!(run(&stream), Ok(want));
    // What fails: a match before the start, a stream cut short, bytes
    // after the end, an end of the wrong length, and more output than
    // there is room for.
    let mut out = [0; 8];
    for bad in [
        &[0x7d, 0, 0x11, 0, 0][..],
        &[21, b'a', b'b', b'c', b'd', 0x11, 0][..],
        &[0x11, 0, 0, 0][..],
        &[0x12, 0, 0][..],
        &lzo_literals(b"nine byte")[..],
    ] {
        assert_eq!(lzo::decompress(bad, &mut out), Err(Error::Io), "{bad:x?}");
    }
}

/// A Btrfs LZO extent of `segments`, with sectors of `sector` bytes.
fn btrfs_lzo(segments: &[Vec<u8>], sector: usize) -> Vec<u8> {
    let mut out = vec![0; 4];
    for segment in segments {
        if sector - out.len() % sector < 4 {
            out.resize(out.len().next_multiple_of(sector), 0);
        }
        out.extend((segment.len() as u32).to_le_bytes());
        out.extend(segment);
    }
    let total = out.len() as u32;
    out[..4].copy_from_slice(&total.to_le_bytes());
    out
}

#[test]
fn lzo_segments_are_read_as_btrfs_lays_them_out() {
    let data = sample(30, 3 * 4096 + 100);
    let segments: Vec<Vec<u8>> = data.chunks(4096).map(lzo_literals).collect();
    let extent = btrfs_lzo(&segments, 4096);
    let mut out = vec![0; data.len() + lzo::SEGMENT_MAX];
    assert_eq!(lzo::btrfs(&extent, &mut out, data.len(), 4096), Ok(data.len()));
    assert!(out[..data.len()] == data[..]);
    // Fewer bytes wanted: the segments that hold them.
    assert_eq!(lzo::btrfs(&extent, &mut out, 5000, 4096), Ok(5000));
    assert!(out[..5000] == data[..5000]);
    // A segment that ends 3 bytes before a sector's end: the next one's
    // length starts the next sector.
    let n = (3900..4096).find(|&n| lzo_literals(&data[..n]).len() == 4096 - 8 - 3).unwrap();
    let segments = [lzo_literals(&data[..n]), lzo_literals(&data[4096..8000])];
    let extent = btrfs_lzo(&segments, 4096);
    assert_eq!(&extent[4093..4096], &[0, 0, 0]);
    assert_eq!(extent[4096..4100], (segments[1].len() as u32).to_le_bytes());
    let want: Vec<u8> = [&data[..n], &data[4096..8000]].concat();
    assert_eq!(lzo::btrfs(&extent, &mut out, want.len(), 4096), Ok(want.len()));
    assert!(out[..want.len()] == want[..]);
    // A length past the extent, or a segment longer than one can be.
    let mut damaged = extent.clone();
    damaged[..4].copy_from_slice(&(extent.len() as u32 + 1).to_le_bytes());
    assert_eq!(lzo::btrfs(&damaged, &mut out, want.len(), 4096), Err(Error::Io));
    let mut damaged = extent.clone();
    damaged[4..8].copy_from_slice(&(lzo::SEGMENT_MAX as u32 + 1).to_le_bytes());
    assert_eq!(lzo::btrfs(&damaged, &mut out, want.len(), 4096), Err(Error::Io));
}

/// An extent of a file, as its `EXTENT_DATA` item says.
struct Found {
    compression: Compression,
    /// Its compressed bytes, inline or read from the image's first copy.
    bytes: Vec<u8>,
    /// Its data's size.
    ram_bytes: u64,
    regular: Option<Regular>,
}

/// The extents of the files of the top subvolume of `image`.
fn extents(image: &[u8]) -> Vec<Found> {
    let volume = open(image).unwrap();
    let fs = volume.tree(tree::FS).unwrap().unwrap();
    let mut cursor = Cursor::seek(&volume, &fs, &Key::new(0, 0, 0)).unwrap();
    let mut found = Vec::new();
    while let Some((key, data)) = cursor.get() {
        if key.kind == item::EXTENT_DATA {
            match Extent::read(data).unwrap() {
                Extent::Inline { ram_bytes, compression, data } => found.push(Found {
                    compression,
                    bytes: data.to_vec(),
                    ram_bytes,
                    regular: None,
                }),
                Extent::Regular(regular) if regular.disk_bytenr != 0 => {
                    let at = volume.place(regular.disk_bytenr).unwrap().copies()[0] as usize;
                    found.push(Found {
                        compression: regular.compression,
                        bytes: image[at..at + regular.disk_num_bytes as usize].to_vec(),
                        ram_bytes: regular.ram_bytes,
                        regular: Some(regular),
                    });
                }
                _ => {}
            }
        }
        cursor.advance().unwrap();
    }
    found
}

/// A tree of files that compress: text of each size around the inline
/// and sector limits and of many extents, files of noise and of noise
/// and text by turns, a sparse file and many small ones.
fn compressible_tree() -> PathBuf {
    let dir = scratch("compressible");
    let write = |path: &str, bytes: &[u8]| {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    for (n, len) in
        [1, 100, 2000, 3000, 4095, 4096, 4097, 10_000, 131_072, 131_073, 400_000, 2_000_000]
            .into_iter()
            .enumerate()
    {
        write(&format!("text-{len}"), &sample(n as u64, len));
    }
    write("noise", &noise(40, 300_000));
    write("by-turns", &blocks(41, 60));
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut sparse = std::fs::File::create(dir.join("sparse")).unwrap();
        sparse.seek(SeekFrom::Start(1 << 20)).unwrap();
        sparse.write_all(&sample(42, 300_000)).unwrap();
        sparse.set_len(3 << 20).unwrap();
    }
    for n in 0..50 {
        write(&format!("small/file-{n}"), &sample(100 + n, 50 * n as usize + 1));
    }
    dir
}

#[test]
fn reads_compressed_files_as_linux_made_them() {
    let dir = compressible_tree();
    let text = std::fs::read(dir.join("text-400000")).unwrap();
    for (option, compression) in [
        ("zlib", Compression::Zlib),
        ("zlib:1", Compression::Zlib),
        ("zlib:9", Compression::Zlib),
        ("lzo", Compression::Lzo),
        ("zstd", Compression::Zstd),
        ("zstd:1", Compression::Zstd),
        ("zstd:15", Compression::Zstd),
    ] {
        let image = mkfs(&["--compress", option], &dir);
        // Both inline and regular extents are compressed.
        let found = extents(&image);
        let regular = found.iter().filter(|f| f.regular.is_some() && f.compression == compression);
        let inline = found.iter().filter(|f| f.regular.is_none() && f.compression == compression);
        assert!(regular.count() > 20, "{option}");
        assert!(inline.count() > 5, "{option}");
        let fs = mount(&image).unwrap();
        same_tree(&fs.root(), &dir, &mut BTreeMap::new());
        // A page read uncompresses its extent, and the file's cache keeps
        // the extent's other pages.
        let fs = mount(&image).unwrap();
        let file = fs.root().lookup("text-400000").unwrap();
        let mut page = vec![0; 4096];
        assert_eq!(file.read(40 * 4096, &mut SliceMut(&mut page)), Ok(4096));
        assert!(page[..] == text[40 * 4096..41 * 4096], "{option}");
        let node = file.as_any().downcast_ref::<BtrfsNode>().unwrap();
        assert_eq!(node.cached_pages(), 32, "{option}");
        // Pages read in any order are the file's.
        let mut random = XorShift(77);
        for _ in 0..200 {
            let at = random.below(text.len());
            let n = file.read(at as u64, &mut SliceMut(&mut page)).unwrap();
            assert!(page[..n] == text[at..at + n], "{option} at {at}");
        }
        // Memory pressure drops the workspace, and the next read makes it
        // again.
        assert!(fs.shrink(usize::MAX) > 0);
        assert!(contents(&*file, 4096).unwrap() == text, "{option}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn damaged_compressed_data_is_read_from_its_other_copy_or_fails() {
    let dir = scratch("compressed-damage");
    std::fs::create_dir_all(&dir).unwrap();
    let text = sample(50, 300_000);
    std::fs::write(dir.join("text"), &text).unwrap();
    for (profile, copies) in [("single", 1), ("dup", 2)] {
        let mut image = mkfs(&["-d", profile, "--compress", "zstd"], &dir);
        let first = extents(&image).into_iter().find_map(|f| f.regular).unwrap();
        assert_eq!(first.compression, Compression::Zstd);
        assert!(first.disk_num_bytes >= 8192, "{profile}");
        let volume = open(&image).unwrap();
        let at = volume.place(first.disk_bytenr).unwrap().copies().to_vec();
        assert_eq!(at.len(), copies, "{profile}");
        // The second sector of the first copy.
        image[at[0] as usize + 4096 + 100] ^= 1;
        let fs = mount(&image).unwrap();
        let file = fs.root().lookup("text").unwrap();
        if copies == 2 {
            assert!(contents(&*file, 4096).unwrap() == text);
            image[at[1] as usize + 4096 + 100] ^= 1;
            let fs = mount(&image).unwrap();
            let file = fs.root().lookup("text").unwrap();
            assert_eq!(contents(&*file, 4096), Err(Error::Io));
        } else {
            assert_eq!(contents(&*file, 4096), Err(Error::Io));
            // The extents after it still read.
            let mut page = [0; 4096];
            assert_eq!(file.read(200_000, &mut SliceMut(&mut page)), Ok(4096));
            assert!(page[..] == text[200_000..204_096]);
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// A run of compressed extents, halves of it uncompressed on two threads,
/// ends before an extent that does not read, in either half: the reads
/// before it and after it are the file's, and one of it fails.
#[test]
fn a_run_of_compressed_extents_ends_before_one_that_fails() {
    let dir = scratch("compressed-run-damage");
    std::fs::create_dir_all(&dir).unwrap();
    let text = sample(51, 1 << 20);
    std::fs::write(dir.join("text"), &text).unwrap();
    let image = mkfs(&["--compress", "zstd"], &dir);
    let found: Vec<Regular> = extents(&image).into_iter().filter_map(|f| f.regular).collect();
    assert_eq!(found.len(), 8);
    let piece = 128 << 10;
    // The first run is the first four extents: two in each half.
    for (damaged, extent) in found.iter().enumerate().take(4) {
        let mut image = image.clone();
        let at = open(&image).unwrap().place(extent.disk_bytenr).unwrap().copies()[0];
        image[at as usize + 100] ^= 1;
        let fs = mount(&image).unwrap();
        let file = fs.root().lookup("text").unwrap();
        let mut buf = vec![0; piece];
        for n in 0..8 {
            let read = file.read((n * piece) as u64, &mut SliceMut(&mut buf));
            if n == damaged {
                assert_eq!(read, Err(Error::Io), "extent {damaged}");
            } else {
                assert_eq!(read, Ok(piece), "extent {damaged}, piece {n}");
                assert!(buf == text[n * piece..(n + 1) * piece], "extent {damaged}, piece {n}");
            }
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// Compressed extents with damage the checksums do not see (as though
/// they were damaged before they were written): every one decodes or
/// fails, and none panics or makes more than its size.
#[test]
fn damaged_compressed_extents_never_panic() {
    let dir = scratch("compressed-fuzz");
    for (n, len) in [700, 3000, 20_000, 300_000].into_iter().enumerate() {
        let path = dir.join(format!("file-{len}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(path, sample(60 + n as u64, len)).unwrap();
    }
    let mut corpus: Vec<(Compression, Vec<u8>, usize)> = Vec::new();
    for option in ["zlib", "lzo", "zstd"] {
        let image = mkfs(&["--compress", option], &dir);
        for f in extents(&image) {
            if f.compression != Compression::None {
                corpus.push((f.compression, f.bytes, f.ram_bytes as usize));
            }
        }
    }
    for (_, frame, make, _) in ZSTD {
        corpus.push((Compression::Zstd, frame.to_vec(), make().len().min(128 * 1024)));
    }
    assert!(corpus.len() > 20, "{} extents", corpus.len());
    let mut workspace = Workspace::new().unwrap();
    let mut random = XorShift(0x9e37_79b9);
    let mut failed = 0;
    let rounds = 2500;
    for _ in 0..rounds {
        let (compression, bytes, len) = &corpus[random.below(corpus.len())];
        let mut damaged = bytes.clone();
        for _ in 0..=random.below(4) {
            let at = random.below(damaged.len().max(1));
            match random.below(5) {
                0 if !damaged.is_empty() => damaged[at] ^= 1 << random.below(8),
                1 if !damaged.is_empty() => damaged[at] = random.next() as u8,
                2 if !damaged.is_empty() => damaged[at] = [0, 0xff][random.below(2)],
                3 => damaged.truncate(at),
                _ => {
                    let from = random.below(damaged.len().max(1));
                    let piece: Vec<u8> = damaged.iter().skip(from).take(64).copied().collect();
                    let at = at.min(damaged.len());
                    damaged.splice(at..at, piece);
                }
            }
        }
        match workspace.unpack(*compression, &damaged, *len, 4096) {
            Ok(data) => assert_eq!(data.len(), *len),
            Err(error) => {
                assert!(matches!(error, Error::Io | Error::Corrupt), "{error:?}");
                failed += 1;
            }
        }
    }
    // Damage that decodes is possible; most is not.
    assert!(failed > rounds / 2, "{failed} of {rounds} failed");
    std::fs::remove_dir_all(dir).unwrap();
}
