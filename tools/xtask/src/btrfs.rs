// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The btrfs-progs that Radix's Btrfs tests, and the image's Dukovo root
//! (ADR 0137), make filesystems with.
//!
//! They use `mkfs.btrfs` options that some distributions' copies lack
//! (Ubuntu 24.04 has btrfs-progs 6.6): subvolumes, compression and inode
//! flags. When the system's copy lacks one, `ob test` or `ob image` fetches
//! Ubuntu 26.04's package, pinned by its SHA-256, and unpacks it into
//! `.cache/btrfs-progs`, without installing it: the tests are pointed at
//! it by `RADIX_BTRFS_PROGS`. It runs on Ubuntu 24.04 and what is made
//! from it, as CI and Linux Mint 22 are, and needs the libraries the
//! system's btrfs-progs package already brought.
//!
//! [`give_to_root`] then makes the image's files root's (ADR 0213).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::process::{self, step};
use crate::{Context, Ctx, Error, Result};

/// The options of `mkfs.btrfs` the tests use that older copies lack.
const OPTIONS: &[&str] = &["--subvol", "--compress", "--inode-flags"];

/// The pinned package's version.
pub const VERSION: &str = "6.17.1-1build1";

/// Its SHA-256, as Ubuntu's archive index lists it.
const SHA256: &str = "89fb4fe8fce0d2ca42d2f26d626c7f46181ef6c7c8e2656ea1462579ec9b67e8";

/// Where it is: Ubuntu's archive, then a mirror of it.
const URLS: &[&str] = &[
    "https://archive.ubuntu.com/ubuntu/pool/main/b/btrfs-progs/btrfs-progs_6.17.1-1build1_amd64.deb",
    "https://mirrors.edge.kernel.org/ubuntu/pool/main/b/btrfs-progs/btrfs-progs_6.17.1-1build1_amd64.deb",
];

/// The system's `mkfs.btrfs`, which is in `/usr/sbin` or `/sbin`, or else
/// on `PATH`.
pub fn system_mkfs() -> Option<PathBuf> {
    ["/usr/sbin/mkfs.btrfs", "/sbin/mkfs.btrfs"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .or_else(|| process::find_program("mkfs.btrfs"))
}

/// Whether `mkfs` runs and has every option the tests use.
fn capable(mkfs: &Path) -> bool {
    Command::new(mkfs).arg("--help").stdin(Stdio::null()).output().is_ok_and(|out| {
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        OPTIONS.iter().all(|option| text.contains(option))
    })
}

/// The directory of the btrfs-progs tools the tests use: `None` for the
/// system's, when it has every option they use; else the pinned copy's,
/// fetched once.
pub fn for_tests(ctx: &Ctx) -> Result<Option<PathBuf>> {
    if system_mkfs().is_some_and(|mkfs| capable(&mkfs)) {
        return Ok(None);
    }
    let cache = ctx.root.join(".cache/btrfs-progs");
    let deb = cache.join(format!("btrfs-progs_{VERSION}_amd64.deb"));
    let dir = cache.join(VERSION);
    let sbin = dir.join("usr/sbin");
    if dir.is_dir() && deb.exists() && process::sha256(ctx, &deb)? == SHA256 {
        return Ok(Some(sbin));
    }
    let Some(dpkg_deb) = process::find_program("dpkg-deb") else {
        return Err(Error::new(
            "the Btrfs tests need a mkfs.btrfs with --subvol, --compress and --inode-flags",
        )
        .hint("install btrfs-progs 6.17 or later from your distribution"));
    };
    process::create_dir(&cache)?;
    if !deb.exists() || process::sha256(ctx, &deb)? != SHA256 {
        fetch(ctx, &deb)?;
    }
    step(&format!("Unpacking btrfs-progs {VERSION} for the Btrfs tests"));
    let partial = cache.join(format!("{VERSION}.partial"));
    for stale in [&partial, &dir] {
        if stale.exists() {
            std::fs::remove_dir_all(stale)
                .map_err(|err| Error::new(format!("removing {}: {err}", stale.display())))?;
        }
    }
    process::run(ctx, Command::new(dpkg_deb).arg("-x").arg(&deb).arg(&partial))?;
    std::fs::rename(&partial, &dir)
        .map_err(|err| Error::new(format!("moving it to {}: {err}", dir.display())))?;
    if !capable(&sbin.join("mkfs.btrfs")) {
        return Err(Error::new(format!("Ubuntu's btrfs-progs {VERSION} does not run here"))
            .hint("install btrfs-progs 6.17 or later from your distribution"));
    }
    Ok(Some(sbin))
}

/// The `mkfs.btrfs` the tests use: the system's, or else the pinned
/// copy's ([`for_tests`]).
pub fn mkfs(ctx: &Ctx) -> Result<PathBuf> {
    match for_tests(ctx)? {
        Some(dir) => Ok(dir.join("mkfs.btrfs")),
        None => system_mkfs().ok_or_else(|| Error::new("mkfs.btrfs is missing")),
    }
}

/// The `btrfs` tool the tests check and read filesystems with: beside
/// the `mkfs.btrfs` they use ([`mkfs`]), in the pinned copy's `usr/bin`,
/// or else the system's.
pub fn tool(ctx: &Ctx) -> Result<PathBuf> {
    match for_tests(ctx)? {
        Some(sbin) => Ok(sbin.with_file_name("bin").join("btrfs")),
        None => ["/usr/bin/btrfs", "/bin/btrfs", "/usr/sbin/btrfs", "/sbin/btrfs"]
            .iter()
            .map(PathBuf::from)
            .find(|path| path.exists())
            .or_else(|| process::find_program("btrfs"))
            .ok_or_else(|| Error::new("the btrfs tool of btrfs-progs is missing")),
    }
}

/// Downloads the pinned package to `deb`, from the first of [`URLS`]
/// that serves it whole.
fn fetch(ctx: &Ctx, deb: &Path) -> Result<()> {
    let partial = deb.with_extension("partial");
    let mut failed = Vec::new();
    for url in URLS {
        step(&format!("Fetching btrfs-progs {VERSION} for the Btrfs tests from {url}"));
        let mut cmd = Command::new("curl");
        cmd.args(["--proto", "=https", "--tlsv1.2", "-fsSL", "--retry", "3", "-o"])
            .arg(&partial)
            .arg(url);
        if let Err(err) = process::run(ctx, &mut cmd) {
            failed.push(format!("{url}: {err}"));
            continue;
        }
        let sum = process::sha256(ctx, &partial)?;
        if sum == SHA256 {
            return std::fs::rename(&partial, deb)
                .map_err(|err| Error::new(format!("moving it to {}: {err}", deb.display())));
        }
        failed.push(format!("{url} has SHA-256 {sum}, not the pinned {SHA256}"));
    }
    let _ = std::fs::remove_file(&partial);
    Err(Error::new(format!("no copy of btrfs-progs {VERSION} would do: {}", failed.join("; ")))
        .hint("install btrfs-progs 6.17 or later from your distribution"))
}

/// Where the superblock is, and what it starts with at 0x40.
const SUPER_AT: u64 = 0x1_0000;
const MAGIC: &[u8; 8] = b"_BHRfS_M";
/// The size of a tree block's header, of a leaf's item and of a node's
/// pointer.
const HEADER: usize = 101;
const ITEM: usize = 25;
const POINTER: usize = 33;
/// The key types of an inode item, a root item and a chunk item.
const INODE_ITEM: u8 = 1;
const ROOT_ITEM: u8 = 132;
const CHUNK_ITEM: u8 = 228;
/// The block group profiles that stripe rather than copy.
const STRIPED: u64 = (1 << 3) | (1 << 6) | (1 << 7) | (1 << 8);

/// Gives every file of the Btrfs filesystem in `image` the owner and
/// group 0, root's (ADR 0213). mkfs.btrfs gives each file the owner its
/// staging copy has, which is whoever runs `ob`, and reads it through
/// nftw(3), past fakeroot's reach, so `ob image` sets it here instead: in
/// each inode item of the filesystem's trees, with each leaf's checksum,
/// in every copy of the metadata. Returns how many it changed.
pub fn give_to_root(image: &Path) -> Result<usize> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .context(|| format!("opening {}", image.display()))?;
    let disk = Disk { file: &file, path: image };
    let mut sb = vec![0; 4096];
    disk.read(SUPER_AT, &mut sb)?;
    let bad = |what: &str| Error::new(format!("{}: {what}", image.display()));
    if &sb[0x40..0x48] != MAGIC {
        return Err(bad("no Btrfs superblock"));
    }
    // crc32c, on one device: what mkfs.btrfs makes by default.
    if u16_at(&sb, 0xc4) != 0 || u64_at(&sb, 0x88) != 1 {
        return Err(bad("not a Btrfs of one device with crc32c checksums"));
    }
    let nodesize = u32_at(&sb, 0x94) as usize;
    let mut chunks = Vec::new();
    // The system chunks the superblock holds, then the chunk tree's.
    let array = &sb[0x32b..0x32b + u32_at(&sb, 0xa0) as usize];
    let mut at = 0;
    while at + 17 <= array.len() {
        let chunk = &array[at + 17..];
        let len = 48 + 32 * usize::from(u16_at(chunk, 44));
        chunks.push(Chunk::read(u64_at(array, at + 9), &chunk[..len], image)?);
        at += 17 + len;
    }
    let mut tree = Tree { disk: &disk, chunks, nodesize, seen: Vec::new() };
    let mut found = Vec::new();
    tree.walk(u64_at(&sb, 0x58), &mut |_, leaf, item| {
        if leaf[item + 8] == CHUNK_ITEM {
            found.push((u64_at(leaf, item + 9), item_data(leaf, item).to_vec()));
        }
        false
    })?;
    for (start, data) in found {
        if !tree.chunks.iter().any(|c| c.start == start) {
            tree.chunks.push(Chunk::read(start, &data, image)?);
        }
    }
    // The filesystem trees: the top one, 5, and each subvolume's, from 256.
    let mut roots = Vec::new();
    tree.walk(u64_at(&sb, 0x50), &mut |_, leaf, item| {
        let id = u64_at(leaf, item);
        if leaf[item + 8] == ROOT_ITEM && (id == 5 || (256..=u64::MAX - 256).contains(&id)) {
            roots.push(u64_at(item_data(leaf, item), 176));
        }
        false
    })?;
    let mut changed = 0;
    for root in roots {
        tree.walk(root, &mut |_, leaf, item| {
            if leaf[item + 8] != INODE_ITEM {
                return false;
            }
            let at = HEADER + u32_at(leaf, item + 17) as usize;
            if leaf[at + 44..at + 52].iter().all(|&b| b == 0) {
                return false;
            }
            leaf[at + 44..at + 52].fill(0);
            changed += 1;
            true
        })?;
    }
    file.sync_all().context(|| format!("writing {}", image.display()))?;
    Ok(changed)
}

/// The image file, read and written at offsets.
struct Disk<'a> {
    file: &'a std::fs::File,
    path: &'a Path,
}

impl Disk<'_> {
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<()> {
        use std::os::unix::fs::FileExt as _;
        self.file.read_exact_at(buf, at).context(|| format!("reading {}", self.path.display()))
    }

    fn write(&self, at: u64, buf: &[u8]) -> Result<()> {
        use std::os::unix::fs::FileExt as _;
        self.file.write_all_at(buf, at).context(|| format!("writing {}", self.path.display()))
    }
}

/// A chunk: logical addresses from `start`, `len` long, at each of
/// `copies` on the one device.
struct Chunk {
    start: u64,
    len: u64,
    copies: Vec<u64>,
}

impl Chunk {
    /// The chunk at `start` that the chunk item `data` describes.
    fn read(start: u64, data: &[u8], image: &Path) -> Result<Chunk> {
        if u64_at(data, 24) & STRIPED != 0 {
            return Err(Error::new(format!("{}: a striped chunk", image.display())));
        }
        let stripes = usize::from(u16_at(data, 44));
        let copies = (0..stripes).map(|i| u64_at(data, 48 + 32 * i + 8)).collect();
        Ok(Chunk { start, len: u64_at(data, 0), copies })
    }
}

/// The trees of the filesystem, read and written by logical address.
struct Tree<'a> {
    disk: &'a Disk<'a>,
    chunks: Vec<Chunk>,
    nodesize: usize,
    /// The blocks walked already.
    seen: Vec<u64>,
}

impl Tree<'_> {
    /// Where the block at logical `at` is, in each copy.
    fn copies(&self, at: u64) -> Result<Vec<u64>> {
        self.chunks
            .iter()
            .find(|c| (c.start..c.start + c.len).contains(&at))
            .map(|c| c.copies.iter().map(|copy| copy + (at - c.start)).collect())
            .ok_or_else(|| {
                Error::new(format!("{}: no chunk holds {at:#x}", self.disk.path.display()))
            })
    }

    /// Calls `f` with each item of each leaf of the tree whose root is at
    /// `root`, with the leaf and the item's offset in it; a leaf for which
    /// it returns true is written back, with its checksum, to each copy.
    fn walk(&mut self, root: u64, f: &mut dyn FnMut(u64, &mut [u8], usize) -> bool) -> Result<()> {
        let mut todo = vec![root];
        while let Some(at) = todo.pop() {
            if self.seen.contains(&at) {
                continue;
            }
            self.seen.push(at);
            let copies = self.copies(at)?;
            let mut block = vec![0; self.nodesize];
            self.disk.read(copies[0], &mut block)?;
            let path = self.disk.path.display();
            if u64_at(&block, 48) != at || u32_at(&block, 0) != crc32c(&block[32..]) {
                return Err(Error::new(format!("{path}: the tree block at {at:#x} is not whole")));
            }
            let items = u32_at(&block, 96) as usize;
            if block[100] > 0 {
                todo.extend((0..items).map(|i| u64_at(&block, HEADER + POINTER * i + 17)));
                continue;
            }
            let mut dirty = false;
            for i in 0..items {
                dirty |= f(at, &mut block, HEADER + ITEM * i);
            }
            if dirty {
                let sum = crc32c(&block[32..]);
                block[..32].fill(0);
                block[..4].copy_from_slice(&sum.to_le_bytes());
                for copy in copies {
                    self.disk.write(copy, &block)?;
                }
            }
        }
        Ok(())
    }
}

/// The data of the leaf's item at `item`.
fn item_data(leaf: &[u8], item: usize) -> &[u8] {
    let at = HEADER + u32_at(leaf, item + 17) as usize;
    &leaf[at..at + u32_at(leaf, item + 21) as usize]
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or_default())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or_default())
}

/// The CRC-32C (Castagnoli, reflected) Btrfs checksums its blocks with.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32c_is_castagnolis() {
        assert_eq!(super::crc32c(b"123456789"), 0xe306_9283);
    }
}
