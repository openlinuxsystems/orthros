// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Building the user programs in userspace/, and the boot archive.
//!
//! The test and benchmark programs are linked into one image that the
//! boot loader loads as a module (`usertests`) and the kernel's `user`,
//! `ipc`, `fuzz` and `bench` self-tests start (ADR 0036). Spark and the
//! programs it starts go into the boot archive (`bootfs`), a ustar file
//! the boot loader loads as another module and the kernel starts Spark
//! from (spark(8)), with the C test programs of userspace/posix and the
//! files the ports put there (ADR 0069).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Profile;
use crate::ports::{self, Ports};
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
pub const ARCHIVE: &[(&str, &str)] = &[
    ("system/bin/spark", "spark"),
    ("system/tests/hello", "hello"),
    ("system/tests/storage", "storage"),
];

/// Empty directories of the boot archive: mount points under `/system`
/// for what Spark mounts there (spark(8)).
pub const ARCHIVE_DIRS: &[&str] = &["system/boot", "system/etc", "system/var"];

/// The C test programs' sources, and where in the boot archive they go.
pub const POSIX_SOURCES: &str = "userspace/posix";
pub const POSIX_DIR: &str = "system/tests/posix";

/// The user programs, built: the test image and the boot archive.
pub struct Programs {
    /// The test image.
    pub usertests: PathBuf,
    /// The boot archive.
    pub bootfs: PathBuf,
    /// The ports, and the toolchain they and the C programs were built
    /// with: what the license manifest lists.
    pub ports: Ports,
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
/// `out/sysroot/system/include/radix/`, where the system's headers are.
pub fn install_headers(ctx: &Ctx) -> Result<PathBuf> {
    let include = ctx.out.join("sysroot/system/include");
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

/// Builds the ports, the C test programs and the user programs, installs
/// the test image as `out/sysroot/boot/usertests`, and writes the boot
/// archive to `out/sysroot/boot/bootfs`.
pub fn build(ctx: &Ctx, profile: Profile) -> Result<Programs> {
    install_headers(ctx)?;
    let ports = ports::build_all(ctx, &[])?;
    let mut files = build_posix(ctx, &ports.toolchain)?;
    for port in &ports.ports {
        for file in &port.recipe.image {
            files.push((file.clone(), ctx.out.join("sysroot").join(file)));
        }
    }
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
    for (path, package) in ARCHIVE {
        files.push((path.to_string(), built.join(package)));
    }
    let archive = write_archive(&files, crate::image::source_date_epoch(ctx))?;
    std::fs::write(&bootfs, &archive).context(|| format!("writing {}", bootfs.display()))?;
    step(&format!(
        "Boot archive of {} programs written to {} ({} KiB)",
        files.len(),
        ctx.display(&bootfs),
        archive.len() / 1024
    ));
    Ok(Programs { usertests: installed, bootfs, ports })
}

/// Builds each C test program of [`POSIX_SOURCES`] (one per `.c` file,
/// with the headers there) into `out/sysroot/system/tests/posix`, and
/// returns each one's place in the boot archive and file.
fn build_posix(ctx: &Ctx, toolchain: &ports::Toolchain) -> Result<Vec<(String, PathBuf)>> {
    let sources = ctx.root.join(POSIX_SOURCES);
    let out = ctx.out.join("sysroot").join(POSIX_DIR);
    process::create_dir(&out)?;
    let mut programs: Vec<PathBuf> = std::fs::read_dir(&sources)
        .context(|| format!("reading {}", sources.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "c"))
        .collect();
    programs.sort();
    step(&format!("Building {} C test programs", programs.len()));
    let mut files = Vec::new();
    for source in programs {
        let name = source.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let program = out.join(&name);
        let mut cmd = Command::new(toolchain.cc());
        cmd.args(["-std=c11", "-D_GNU_SOURCE"])
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-I"])
            .arg(&sources)
            .arg("-o")
            .arg(&program)
            .arg(&source);
        process::run(ctx, &mut cmd)?;
        files.push((format!("{POSIX_DIR}/{name}"), program));
    }
    Ok(files)
}

/// The boot archive: `files`, each a path in the archive and the file
/// it holds, the directories above them and those of [`ARCHIVE_DIRS`],
/// owned by root and dated `epoch` (the commit's time), so the same
/// programs make the same bytes.
fn write_archive(files: &[(String, PathBuf)], epoch: Option<String>) -> Result<Vec<u8>> {
    let mtime = epoch.and_then(|epoch| epoch.parse().ok()).unwrap_or(0);
    let mut entries: Vec<(String, Option<&Path>)> = Vec::new();
    for dir in ARCHIVE_DIRS {
        entries.push((dir.to_string(), None));
    }
    for (path, file) in files {
        // Each directory above the file, once.
        for (end, _) in path.match_indices('/') {
            let dir = &path[..end];
            if !entries.iter().any(|(seen, _)| seen == dir) {
                entries.push((dir.to_string(), None));
            }
        }
        entries.push((path.to_string(), Some(file)));
    }
    // A directory before what is in it, as the archive's reader wants.
    entries.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
    let mut writer = radix_bootfs::write::Writer::new();
    for (path, file) in &entries {
        let written = match file {
            None => writer.directory(path, 0o755, mtime),
            Some(file) => {
                let data = std::fs::read(file).context(|| format!("reading {}", file.display()))?;
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
        let mut files = Vec::new();
        for (path, package) in ARCHIVE {
            std::fs::write(dir.join(package), format!("the {package} program")).unwrap();
            files.push((path.to_string(), dir.join(package)));
        }
        let bytes = write_archive(&files, Some("1700000000".into())).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let archive = radix_bootfs::open(&bytes).unwrap();
        let paths: Vec<String> = archive.entries().map(|e| e.path().to_string()).collect();
        assert_eq!(
            paths,
            [
                "system",
                "system/bin",
                "system/bin/spark",
                "system/boot",
                "system/etc",
                "system/tests",
                "system/tests/hello",
                "system/tests/storage",
                "system/var"
            ]
        );
        let spark = archive.find("/system/bin/spark").unwrap();
        assert_eq!(spark.data, b"the spark program");
        assert_eq!((spark.mode, spark.mtime), (0o755, 1_700_000_000));
        assert_eq!(write_archive(&files, None).ok(), None, "the programs are gone");
    }
}
