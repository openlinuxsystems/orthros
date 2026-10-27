// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The btrfs-progs that Radix's Btrfs tests make filesystems with.
//!
//! The tests use `mkfs.btrfs` options that some distributions' copies
//! lack (Ubuntu 24.04 has btrfs-progs 6.6): subvolumes, compression and
//! inode flags. When the system's copy lacks one, `ob test` fetches
//! Ubuntu 26.04's package, pinned by its SHA-256, and unpacks it into
//! `.cache/btrfs-progs`, without installing it: the tests are pointed at
//! it by `RADIX_BTRFS_PROGS`. It runs on Ubuntu 24.04 and what is made
//! from it, as CI and Linux Mint 22 are, and needs the libraries the
//! system's btrfs-progs package already brought.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::process::{self, step};
use crate::{Ctx, Error, Result};

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
