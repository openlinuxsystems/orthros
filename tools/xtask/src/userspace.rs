// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Building the user programs in userspace/, and the boot archive.
//!
//! The test and benchmark programs are linked into one image that the
//! boot loader loads as a module (`usertests`) and the kernel's `user`,
//! `ipc`, `fuzz` and `bench` self-tests start (ADR 0036). Spark and the
//! programs it starts go into the boot archive (`bootfs`), a ustar file
//! the boot loader loads as another module and the kernel starts Spark
//! from (spark(8)).

use std::path::{Path, PathBuf};

use crate::config::Profile;
use crate::process::{self, step};
use crate::{Context, Ctx, Result};

/// The user programs' target specification, relative to the repository
/// root.
pub const TARGET_SPEC: &str = "userspace/targets/x86_64-radix-user.json";

/// Name Cargo gives the target's output directory.
pub const TARGET_NAME: &str = "x86_64-radix-user";

/// Where the test image goes in a boot image, and the word on its module
/// line that the kernel finds it by.
pub const USERTESTS_PATH: &str = "/boot/usertests";
pub const USERTESTS_WORD: &str = "usertests";

/// Where the boot archive goes in a boot image, and the word on its module
/// line.
pub const BOOTFS_PATH: &str = "/boot/bootfs";
pub const BOOTFS_WORD: &str = "bootfs";

/// What the boot archive holds: each program's path in it, and its
/// package.
pub const ARCHIVE: &[(&str, &str)] =
    &[("system/bin/spark", "spark"), ("system/tests/hello", "hello")];

/// The user programs, built: the test image and the boot archive.
pub struct Programs {
    /// The test image.
    pub usertests: PathBuf,
    /// The boot archive.
    pub bootfs: PathBuf,
}

/// Cargo arguments that build `core` for the user target.
pub fn target_args(ctx: &Ctx) -> Vec<String> {
    vec![
        "--target".into(),
        ctx.root.join(TARGET_SPEC).display().to_string(),
        "-Zjson-target-spec".into(),
        "-Zbuild-std=core,compiler_builtins,alloc".into(),
        "-Zbuild-std-features=compiler-builtins-mem".into(),
    ]
}

/// A Cargo command for the userspace workspace, writing to
/// `target/userspace`.
pub fn cargo(ctx: &Ctx, subcommand: &str) -> std::process::Command {
    let mut cmd = process::cargo(ctx);
    cmd.args([subcommand, "--manifest-path", "userspace/Cargo.toml"])
        .arg("--target-dir")
        .arg(ctx.target.join("userspace"))
        .env("CARGO_FUTURE_INCOMPAT_REPORT_FREQUENCY", "never");
    cmd
}

/// Where the test image is installed.
pub fn installed(ctx: &Ctx) -> PathBuf {
    ctx.out.join("sysroot").join(USERTESTS_PATH.trim_start_matches('/'))
}

/// The system call interface, compiled from `abi/*.ridl` by ridlc.
pub fn abi(ctx: &Ctx) -> Result<ridlc::Abi> {
    let dir = ctx.root.join("abi");
    ridlc::compile_dir(&dir).map_err(|errors| {
        let lines: Vec<String> = errors.iter().map(|error| format!("    {error}")).collect();
        crate::Error::new(format!(
            "abi/*.ridl has {} problems:\n{}",
            errors.len(),
            lines.join("\n")
        ))
    })
}

/// Writes the C headers of the system call interface into
/// `out/sysroot/include/radix/`.
pub fn install_headers(ctx: &Ctx) -> Result<PathBuf> {
    let include = ctx.out.join("sysroot/include");
    for (name, text) in ridlc::c_headers(&abi(ctx)?) {
        let path = include.join(name);
        if let Some(parent) = path.parent() {
            process::create_dir(parent)?;
        }
        std::fs::write(&path, text).context(|| format!("writing {}", path.display()))?;
    }
    step(&format!("C headers of the system call interface in {}", ctx.display(&include)));
    Ok(include)
}

/// Builds the user programs, installs the test image as
/// `out/sysroot/boot/usertests`, and writes the boot archive to
/// `out/sysroot/boot/bootfs`.
pub fn build(ctx: &Ctx, profile: Profile) -> Result<Programs> {
    step(&format!("Building the user programs ({profile})"));
    let mut cmd = cargo(ctx, "build");
    cmd.args(["-p", "usertests"]);
    for (_, package) in ARCHIVE {
        cmd.args(["-p", package]);
    }
    cmd.args(target_args(ctx));
    if profile == Profile::Release {
        cmd.arg("--release");
    }
    process::run(ctx, &mut cmd)?;
    let built = ctx.target.join("userspace").join(TARGET_NAME).join(profile.dir());
    let installed = installed(ctx);
    if let Some(parent) = installed.parent() {
        process::create_dir(parent)?;
    }
    let usertests = built.join("usertests");
    std::fs::copy(&usertests, &installed)
        .context(|| format!("copying {} to {}", usertests.display(), installed.display()))?;
    let size = std::fs::metadata(&installed).map(|m| m.len()).unwrap_or(0);
    step(&format!(
        "User test programs installed at {} ({} KiB)",
        ctx.display(&installed),
        size / 1024
    ));
    let bootfs = ctx.out.join("sysroot").join(BOOTFS_PATH.trim_start_matches('/'));
    let archive = write_archive(&built, crate::image::source_date_epoch(ctx))?;
    std::fs::write(&bootfs, &archive).context(|| format!("writing {}", bootfs.display()))?;
    step(&format!(
        "Boot archive of {} programs written to {} ({} KiB)",
        ARCHIVE.len(),
        ctx.display(&bootfs),
        archive.len() / 1024
    ));
    Ok(Programs { usertests: installed, bootfs })
}

/// The boot archive: the programs of [`ARCHIVE`] from `built`, and the
/// directories above them, owned by root and dated `epoch` (the commit's
/// time), so the same programs make the same bytes.
fn write_archive(built: &Path, epoch: Option<String>) -> Result<Vec<u8>> {
    let mtime = epoch.and_then(|epoch| epoch.parse().ok()).unwrap_or(0);
    let mut entries: Vec<(String, Option<&str>)> = Vec::new();
    for (path, package) in ARCHIVE {
        // Each directory above the program, once.
        for (end, _) in path.match_indices('/') {
            let dir = &path[..end];
            if !entries.iter().any(|(seen, _)| seen == dir) {
                entries.push((dir.to_string(), None));
            }
        }
        entries.push((path.to_string(), Some(package)));
    }
    // A directory before what is in it, as the archive's reader wants.
    entries.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
    let mut writer = radix_bootfs::write::Writer::new();
    for (path, package) in &entries {
        let written = match package {
            None => writer.directory(path, 0o755, mtime),
            Some(package) => {
                let file = built.join(package);
                let data =
                    std::fs::read(&file).context(|| format!("reading {}", file.display()))?;
                writer.file(path, 0o755, mtime, &data)
            }
        };
        written.map_err(|err| crate::Error::new(format!("the boot archive: {path}: {err}")))?;
    }
    Ok(writer.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_archive_holds_the_programs_under_their_directories() {
        let dir = std::env::temp_dir().join(format!("ob-archive-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (_, package) in ARCHIVE {
            std::fs::write(dir.join(package), format!("the {package} program")).unwrap();
        }
        let bytes = write_archive(&dir, Some("1700000000".into())).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let archive = radix_bootfs::open(&bytes).unwrap();
        let paths: Vec<String> = archive.entries().map(|e| e.path().to_string()).collect();
        assert_eq!(
            paths,
            ["system", "system/bin", "system/bin/spark", "system/tests", "system/tests/hello"]
        );
        let spark = archive.find("/system/bin/spark").unwrap();
        assert_eq!(spark.data, b"the spark program");
        assert_eq!((spark.mode, spark.mtime), (0o755, 1_700_000_000));
        assert_eq!(write_archive(&dir, None).ok(), None, "the programs are gone");
    }
}
