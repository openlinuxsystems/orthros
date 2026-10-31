// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob fixture ntfs-3g`: an NTFS volume ntfs-3g filled.
//!
//! `mkntfs` formats an 8 MiB volume with clusters of 512 bytes, and
//! ntfs-3g, mounted through FUSE, fills it with what Radix's NTFS reader
//! must read as ntfs-3g wrote it: directories deep and wide, names of
//! every kind, hard links enough to need an attribute list, links of
//! ntfs-3g's own (Interix), Windows' and WSL's and a junction, made with
//! ntfs-3g's `system.ntfs_reparse_data`, compressed and sparse files, a
//! file in pieces, an alternate data stream, a short name, and files
//! marked read-only and offline. The NTFS host tests check that Radix
//! reads back each of them (ADR 0236). Mounting through FUSE takes root,
//! or a FUSE that lets this user mount; `setfattr` comes with the `attr`
//! package.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Command;

use crate::process::{self, step};
use crate::{Context, Ctx, Result};

/// Where the NTFS host tests keep the fixture.
pub const FIXTURE: &str = "kernel/fs/ntfs/src/fixtures/ntfs-3g.sectors";

/// The volume's size.
const BYTES: u64 = 8 << 20;

/// `ob fixture ntfs-3g`.
pub fn ntfs_3g(ctx: &Ctx) -> Result<()> {
    for tool in ["mkntfs", "ntfs-3g", "fusermount", "setfattr"] {
        if process::find_program(tool).is_none() && !Path::new("/usr/sbin").join(tool).exists() {
            crate::bail!(
                "{tool} is missing: install ntfs-3g, fuse and attr (apt install ntfs-3g attr)"
            );
        }
    }
    let dir = ctx.out.join("fixture");
    process::create_dir(&dir)?;
    let image = dir.join("ntfs-3g.img");
    let file = File::create(&image).context(|| format!("making {}", image.display()))?;
    file.set_len(BYTES).context(|| format!("sizing {}", image.display()))?;
    drop(file);
    let mut mkntfs = Command::new("mkntfs");
    mkntfs.args(["-q", "-F", "-Q", "-c", "512", "-L", "NTFS3G"]).arg(&image);
    process::run(ctx, &mut mkntfs)?;
    let mount = dir.join("ntfs-3g.mnt");
    process::create_dir(&mount)?;
    step("Filling the volume through ntfs-3g");
    let mut ntfs_3g = Command::new("ntfs-3g");
    ntfs_3g.args(["-o", "compression,streams_interface=xattr"]).arg(&image).arg(&mount);
    process::run(ctx, &mut ntfs_3g)?;
    let filled = fill(&mount);
    let mut unmount = Command::new("fusermount");
    unmount.arg("-u").arg(&mount);
    let unmounted = process::run(ctx, &mut unmount);
    filled.context(|| format!("filling the volume mounted at {}", mount.display()))?;
    unmounted?;
    let bytes = fs::read(&image).context(|| format!("reading {}", image.display()))?;
    let at = ctx.root.join(FIXTURE);
    if let Some(parent) = at.parent() {
        process::create_dir(parent)?;
    }
    fs::write(&at, sectors(&bytes)).context(|| format!("writing {}", at.display()))?;
    println!("\nFixture ready in {}", ctx.display(&at));
    Ok(())
}

/// `image` as the fixture keeps it: its count of 512-byte sectors, then
/// each sector not all zeros, as its number and its bytes; a number with
/// its top bit set stands for a sector all 0xff, without its bytes, as
/// `mkntfs` fills `$LogFile` with. The numbers are little-endian `u32`s.
pub fn sectors(image: &[u8]) -> Vec<u8> {
    let mut out = ((image.len() / 512) as u32).to_le_bytes().to_vec();
    for (n, sector) in image.as_chunks::<512>().0.iter().enumerate() {
        if sector.iter().all(|&b| b == 0) {
            continue;
        }
        if sector.iter().all(|&b| b == 0xff) {
            out.extend_from_slice(&(n as u32 | 1 << 31).to_le_bytes());
            continue;
        }
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.extend_from_slice(sector);
    }
    out
}

/// The volume the fixture `kept` keeps ([`sectors`]): the boot test
/// `ntfs` reads it from a disk.
pub fn image(kept: &[u8]) -> Vec<u8> {
    let word =
        |at: usize| kept.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let mut image = vec![0u8; word(0).unwrap_or(0) as usize * 512];
    let mut at = 4;
    while let Some(n) = word(at) {
        at += 4;
        let start = (n & !(1 << 31)) as usize * 512;
        let Some(sector) = image.get_mut(start..start + 512) else { break };
        if n & 1 << 31 != 0 {
            sector.fill(0xff);
        } else if let Some(bytes) = kept.get(at..at + 512) {
            sector.copy_from_slice(bytes);
            at += 512;
        }
    }
    image
}

/// Bytes that differ from offset to offset and file to file, as the NTFS
/// host tests make them.
pub fn pattern(seed: u32, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// A text that compresses well: 4000 lines of 50 kinds.
pub fn text() -> Vec<u8> {
    (0..4000)
        .flat_map(|i| {
            format!("line {} of a text that compresses well, again and again\n", i % 50)
                .into_bytes()
        })
        .collect()
}

/// 8 KiB of text, 16 KiB of zeros, and the 8 KiB again.
pub fn mixed() -> Vec<u8> {
    let text: Vec<u8> =
        (0..2000).flat_map(|i| format!("mixed {i}\n").into_bytes()).take(8192).collect();
    [&text[..], &[0u8; 16384][..], &text[..]].concat()
}

/// Sets the extended attribute `name` of `path` to the bytes `value`,
/// through ntfs-3g.
fn set_attr(path: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    let hex = value.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    let status = Command::new("setfattr")
        .args(["-n", name, "-v"])
        .arg(format!("0x{hex}"))
        .arg(path)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("setfattr -n {name} {} failed", path.display())))
    }
}

/// UTF-16, little-endian.
fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// A reparse point of `tag` holding `data`.
fn reparse(tag: u32, data: &[u8]) -> Vec<u8> {
    let mut out = tag.to_le_bytes().to_vec();
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(data);
    out
}

/// A Windows symbolic link's reparse point, to `target` (substitute name)
/// shown as `print`; relative if `target` is.
fn symlink(target: &str, print: &str) -> Vec<u8> {
    let (s, p) = (utf16(target), utf16(print));
    let relative = u32::from(!target.starts_with(r"\??\"));
    let mut data = Vec::new();
    for field in [0, s.len() as u16, s.len() as u16, p.len() as u16] {
        data.extend_from_slice(&field.to_le_bytes());
    }
    data.extend_from_slice(&relative.to_le_bytes());
    data.extend_from_slice(&s);
    data.extend_from_slice(&p);
    reparse(0xa000_000c, &data)
}

/// A junction's reparse point, to `target` shown as `print`, each name
/// ending in a NUL as Windows writes them.
fn junction(target: &str, print: &str) -> Vec<u8> {
    let (s, p) = (utf16(target), utf16(print));
    let mut data = Vec::new();
    for field in [0, s.len() as u16, s.len() as u16 + 2, p.len() as u16] {
        data.extend_from_slice(&field.to_le_bytes());
    }
    data.extend_from_slice(&s);
    data.extend_from_slice(&[0, 0]);
    data.extend_from_slice(&p);
    data.extend_from_slice(&[0, 0]);
    reparse(0xa000_0003, &data)
}

/// Fills the volume mounted at `m`.
fn fill(m: &Path) -> std::io::Result<()> {
    let put = |path: &str, bytes: &[u8]| fs::write(m.join(path), bytes);
    fs::create_dir_all(m.join("Documents/Deep/er/still"))?;
    put("Documents/report.txt", b"Written by ntfs-3g.\n")?;
    put("Documents/big.bin", &pattern(1, 40_000))?;
    put("Documents/\u{dc}n\u{ef}c\u{f6}d\u{e9} \u{2713}.txt", &pattern(3, 1000))?;
    put("Documents/emoji \u{1f600}\u{1f680}.bin", b"emoji\n")?;
    put("Documents/Deep/er/still/file.txt", b"deep\n")?;
    put("empty", b"")?;
    fs::create_dir(m.join("empty dir"))?;
    // More names than the directory's root holds: index blocks.
    fs::create_dir(m.join("many"))?;
    for i in 0..40 {
        put(&format!("many/file {i:02}.txt"), format!("file {i:02}\n").as_bytes())?;
    }
    // Hard links; enough names for one file that its record overflows
    // into more, which its attribute list names.
    fs::hard_link(m.join("Documents/report.txt"), m.join("report link.txt"))?;
    put("linked.txt", b"linked\n")?;
    fs::create_dir(m.join("links"))?;
    for i in 0..30 {
        fs::hard_link(
            m.join("linked.txt"),
            m.join(format!("links/name number {i:02} of the linked file.txt")),
        )?;
    }
    // Links: ntfs-3g's own, Windows' relative and absolute ones, a
    // junction, and WSL's.
    std::os::unix::fs::symlink("Documents/report.txt", m.join("interix link"))?;
    put("windows link", b"")?;
    set_attr(
        &m.join("windows link"),
        "system.ntfs_reparse_data",
        &symlink(r"Documents\report.txt", r"Documents\report.txt"),
    )?;
    put("Documents/Deep/er/absolute link", b"")?;
    let absolute = symlink(r"\??\C:\Documents\Deep", r"C:\Documents\Deep");
    set_attr(&m.join("Documents/Deep/er/absolute link"), "system.ntfs_reparse_data", &absolute)?;
    fs::create_dir(m.join("junction"))?;
    let to = junction(r"\??\C:\Documents", r"C:\Documents");
    set_attr(&m.join("junction"), "system.ntfs_reparse_data", &to)?;
    put("wsl link", b"")?;
    let wsl = [&2u32.to_le_bytes()[..], b"Documents/report.txt"].concat();
    set_attr(&m.join("wsl link"), "system.ntfs_reparse_data", &reparse(0xa000_001d, &wsl))?;
    // A directory marked compressed: what is written in it is.
    fs::create_dir(m.join("compressed"))?;
    set_attr(&m.join("compressed"), "system.ntfs_attrib_be", &0x0810u32.to_be_bytes())?;
    put("compressed/text.txt", &text())?;
    put("compressed/random.bin", &pattern(7, 12_000))?;
    put("compressed/mixed.bin", &mixed())?;
    // Data, a hole, data.
    {
        let mut f = File::create(m.join("sparse.bin"))?;
        f.write_all(&b"start".repeat(100))?;
        f.seek(SeekFrom::Start(3 << 20))?;
        f.write_all(&b"middle".repeat(100))?;
        f.seek(SeekFrom::Start((6 << 20) - 600))?;
        f.write_all(&b"end.".repeat(150))?;
    }
    put("streams.txt", b"main\n")?;
    set_attr(&m.join("streams.txt"), "user.extra", b"in a stream")?;
    put("read only.txt", b"ro\n")?;
    set_attr(&m.join("read only.txt"), "system.ntfs_attrib_be", &0x21u32.to_be_bytes())?;
    put("cloud.txt", b"in the cloud\n")?;
    set_attr(&m.join("cloud.txt"), "system.ntfs_attrib_be", &0x1020u32.to_be_bytes())?;
    put("a long file name.txt", b"long\n")?;
    set_attr(&m.join("a long file name.txt"), "system.ntfs_dos_name", b"ALONGF~1.TXT")?;
    // Names the same but for case, as POSIX names may be.
    put("Case.txt", b"upper\n")?;
    put("case.txt", b"lower\n")?;
    // A file in pieces: after each piece, a file of zeros the next piece
    // must go past.
    fs::create_dir(m.join("fill"))?;
    {
        let mut f = File::create(m.join("pieces.bin"))?;
        for i in 0..40u8 {
            f.write_all(&[0x40 + i; 512])?;
            f.sync_all()?;
            let mut spacer = File::create(m.join(format!("fill/{i:02}")))?;
            spacer.write_all(&[0; 1024])?;
            spacer.sync_all()?;
        }
    }
    for entry in fs::read_dir(m.join("fill"))? {
        fs::remove_file(entry?.path())?;
    }
    fs::remove_dir(m.join("fill"))?;
    Ok(())
}
