// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Linux kernel `ob bench` holds Radix to (ADR 0290).
//!
//! The baseline used to be the CI runner's own kernel, so each Ubuntu
//! kernel update moved the bar with no change of ours: tmpfs open+close
//! went from 0.87 to 1.04 of Linux that way. Now it is one release of
//! Ubuntu's Azure kernel, the flavour CI's runners boot, pinned by the
//! SHA-256 of its packages as Ubuntu's archive index lists them. `ob
//! bench` fetches them once into `.cache/linux-baseline`, unpacks them
//! without installing them and runs `depmod` on the copy, which Ubuntu's
//! package leaves to its installer, so that the Btrfs module and those it
//! needs can be found. Moving to a newer release is a change of its own
//! (all four constants below), whose pull request shows the new bar.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::process::{self, step};
use crate::{Ctx, Error, Result};

/// The pinned kernel's release, as `uname -r` gives it.
pub const RELEASE: &str = "7.0.0-1017-azure";

/// Its packages' version.
const VERSION: &str = "7.0.0-1017.17~24.04.1";

/// Where in the archive its packages are.
const POOL: &str = "pool/main/l/linux-azure-7.0";

/// Its packages, each with its SHA-256: the kernel, and its modules
/// (Btrfs is one).
const PACKAGES: &[(&str, &str)] = &[
    ("linux-image-unsigned", "da9c219855948b8d697634d279bbe66064e004c6ad79113071354fbb67fe4bdc"),
    ("linux-modules", "19ad1d32c0138b6ef90257f211e80490d3aaf322ba5c3c2910fdfbfb1f20da6d"),
];

/// Ubuntu's archive, then a mirror of it. Superseded kernels stay in the
/// archive's pool, so a pinned one can still be fetched after newer ones
/// come out.
const ARCHIVES: &[&str] =
    &["https://archive.ubuntu.com/ubuntu", "https://mirrors.edge.kernel.org/ubuntu"];

/// The pinned kernel, unpacked.
pub struct Baseline {
    /// Its image, `boot/vmlinuz-<release>`.
    pub kernel: PathBuf,
    /// The directory of its modules, with the `modules.dep` `depmod` wrote.
    pub modules: PathBuf,
}

/// The file name of the package `name`.
fn deb_name(name: &str) -> String {
    format!("{name}-{RELEASE}_{VERSION}_amd64.deb")
}

/// The pinned kernel, fetched and unpacked once.
pub fn fetch(ctx: &Ctx) -> Result<Baseline> {
    let cache = ctx.root.join(".cache/linux-baseline");
    let dir = cache.join(RELEASE);
    let done = dir.join(".unpacked");
    let baseline = Baseline {
        kernel: dir.join(format!("boot/vmlinuz-{RELEASE}")),
        modules: dir.join("lib/modules").join(RELEASE),
    };
    if done.exists() && baseline.kernel.exists() && baseline.modules.join("modules.dep").exists() {
        return Ok(baseline);
    }
    let Some(dpkg_deb) = process::find_program("dpkg-deb") else {
        return Err(Error::new(format!(
            "the Linux baseline is Ubuntu's kernel {RELEASE}, and unpacking it takes dpkg-deb"
        ))
        .hint("install dpkg, or give a kernel with --linux-kernel"));
    };
    let depmod = ["/usr/sbin/depmod", "/sbin/depmod"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .or_else(|| process::find_program("depmod"))
        .ok_or_else(|| {
            Error::new(format!(
                "the Linux baseline is Ubuntu's kernel {RELEASE}, and finding its Btrfs \
                 module takes depmod"
            ))
            .hint("install kmod, or give a kernel with --linux-kernel")
        })?;
    process::create_dir(&cache)?;
    let partial = cache.join(format!("{RELEASE}.partial"));
    for stale in [&partial, &dir] {
        if stale.exists() {
            std::fs::remove_dir_all(stale)
                .map_err(|err| Error::new(format!("removing {}: {err}", stale.display())))?;
        }
    }
    for (name, sha256) in PACKAGES {
        let deb = cache.join(deb_name(name));
        if !deb.exists() || process::sha256(ctx, &deb)? != *sha256 {
            download(ctx, name, sha256, &deb)?;
        }
        step(&format!("Unpacking {name} {RELEASE}, the Linux baseline"));
        process::run(ctx, Command::new(&dpkg_deb).arg("-x").arg(&deb).arg(&partial))?;
    }
    process::run(ctx, Command::new(depmod).arg("-b").arg(&partial).arg(RELEASE))?;
    std::fs::rename(&partial, &dir)
        .map_err(|err| Error::new(format!("moving it to {}: {err}", dir.display())))?;
    std::fs::write(&done, VERSION)
        .map_err(|err| Error::new(format!("writing {}: {err}", done.display())))?;
    Ok(baseline)
}

/// Downloads the package `name` to `deb`, from the first of [`ARCHIVES`]
/// that serves it whole.
fn download(ctx: &Ctx, name: &str, sha256: &str, deb: &Path) -> Result<()> {
    let partial = deb.with_extension("partial");
    let mut failed = Vec::new();
    for archive in ARCHIVES {
        let url = format!("{archive}/{POOL}/{}", deb_name(name));
        step(&format!("Fetching {name} {RELEASE}, the Linux baseline, from {url}"));
        let mut cmd = Command::new("curl");
        cmd.args(["--proto", "=https", "--tlsv1.2", "-fsSL", "--retry", "3", "-o"])
            .arg(&partial)
            .arg(&url);
        if let Err(err) = process::run(ctx, &mut cmd) {
            failed.push(format!("{url}: {err}"));
            continue;
        }
        let sum = process::sha256(ctx, &partial)?;
        if sum == sha256 {
            return std::fs::rename(&partial, deb)
                .map_err(|err| Error::new(format!("moving it to {}: {err}", deb.display())));
        }
        failed.push(format!("{url} has SHA-256 {sum}, not the pinned {sha256}"));
    }
    let _ = std::fs::remove_file(&partial);
    Err(Error::new(format!("no copy of {name} {RELEASE} would do: {}", failed.join("; ")))
        .hint("give a kernel with --linux-kernel"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_packages_are_of_the_pinned_release() {
        let release = RELEASE.strip_suffix("-azure").unwrap();
        assert!(VERSION.starts_with(&format!("{release}.")));
        assert_eq!(
            deb_name("linux-modules"),
            "linux-modules-7.0.0-1017-azure_7.0.0-1017.17~24.04.1_amd64.deb"
        );
        for (_, sha256) in PACKAGES {
            assert_eq!(sha256.len(), 64);
            assert!(sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
    }
}
