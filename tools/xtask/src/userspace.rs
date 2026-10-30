// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Building the user programs in userspace/, and the boot archive.
//!
//! The test and benchmark programs are linked into one image that the
//! boot loader loads as a module (`usertests`) and the kernel's `user`,
//! `ipc`, `fuzz` and `bench` self-tests start (ADR 0036). Spark and the
//! programs it starts go into the boot archive (`bootfs`), a ustar file
//! the boot loader loads as another module and the kernel starts Spark
//! from (spark(8)), with the C test programs of userspace/posix, the
//! files the ports put there (ADR 0069) and the programs on Rust's
//! standard library of userspace/programs (ADR 0070).

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

/// The target of programs on Rust's standard library, relative to the
/// repository root (ADR 0070), and the name of its output directory.
pub const STD_TARGET_SPEC: &str = "userspace/targets/x86_64-unknown-radix.json";
pub const STD_TARGET_NAME: &str = "x86_64-unknown-radix";

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
    ("system/commands/spark", "spark"),
    ("system/tests/hello", "hello"),
    ("system/tests/storage", "storage"),
];

/// Empty directories of the boot archive: mount points under `/system`
/// for what Spark mounts there (spark(8)).
pub const ARCHIVE_DIRS: &[&str] =
    &["system/boot", "system/live", "system/settings", "system/state"];

/// Symbolic links of the boot archive: Unix's names for folders of
/// `/system`, each leading to the folder that holds what it stands for
/// (ADR 0114), so that `/usr/bin` and `/usr/lib` still lead somewhere.
pub const ARCHIVE_LINKS: &[(&str, &str)] =
    &[("system/bin", "commands"), ("system/lib", "libraries"), ("system/share", "resources")];

/// The `.hidden` file of `/system`: the names of its links, which
/// listings leave out (ADR 0113).
pub const ARCHIVE_HIDDEN: (&str, &str) = ("system/.hidden", "bin\nlib\nshare\n");

/// The programs on Rust's standard library that the boot archive holds:
/// each one's path in it, and its package in userspace/programs.
pub const STD_ARCHIVE: &[(&str, &str)] = &[
    ("system/commands/consoled", "consoled"),
    ("system/commands/nukesh", "nukesh"),
    ("system/commands/systools", "systools"),
    ("system/tests/posix/std-test", "std-test"),
];

/// Symbolic links to those programs in the boot archive: each link's path,
/// and its target. Nukesh is the POSIX shell when it is started as `sh`,
/// a restricted shell as `rnukesh`, and runs bash's scripts as `bash`
/// (nukesh(1), ADR 0103); the native tools are one program, which each
/// tool's name runs (ADR 0096).
pub const STD_LINKS: &[(&str, &str)] = &[
    ("system/commands/bash", "nukesh"),
    ("system/commands/sh", "nukesh"),
    ("system/commands/rnukesh", "nukesh"),
    ("system/commands/balance", "systools"),
    ("system/commands/blkid", "systools"),
    ("system/commands/chgrp", "systools"),
    ("system/commands/chmod", "systools"),
    ("system/commands/chown", "systools"),
    ("system/commands/column", "systools"),
    ("system/commands/df", "systools"),
    ("system/commands/device", "systools"),
    ("system/commands/diskhealth", "systools"),
    ("system/commands/dmesg", "systools"),
    ("system/commands/eject", "systools"),
    ("system/commands/findmnt", "systools"),
    ("system/commands/fstrim", "systools"),
    ("system/commands/free", "systools"),
    ("system/commands/hd", "systools"),
    ("system/commands/halt", "systools"),
    ("system/commands/hexdump", "systools"),
    ("system/commands/ionice", "systools"),
    ("system/commands/iostat", "systools"),
    ("system/commands/keymap", "systools"),
    ("system/commands/kill", "systools"),
    ("system/commands/lsblk", "systools"),
    ("system/commands/lscpu", "systools"),
    ("system/commands/lsmem", "systools"),
    ("system/commands/lsof", "systools"),
    ("system/commands/lspci", "systools"),
    ("system/commands/mkswap", "systools"),
    ("system/commands/nvme", "systools"),
    ("system/commands/pgrep", "systools"),
    ("system/commands/pidstat", "systools"),
    ("system/commands/pkill", "systools"),
    ("system/commands/poweroff", "systools"),
    ("system/commands/ps", "systools"),
    ("system/commands/quota", "systools"),
    ("system/commands/reboot", "systools"),
    ("system/commands/renice", "systools"),
    ("system/commands/replace", "systools"),
    ("system/commands/rtrace", "systools"),
    ("system/commands/script", "systools"),
    ("system/commands/scrub", "systools"),
    ("system/commands/service", "systools"),
    ("system/commands/shutdown", "systools"),
    ("system/commands/subvolume", "systools"),
    ("system/commands/swapoff", "systools"),
    ("system/commands/swapon", "systools"),
    ("system/commands/taskset", "systools"),
    ("system/commands/top", "systools"),
    ("system/commands/tree", "systools"),
    ("system/commands/uptime", "systools"),
    ("system/commands/users", "systools"),
    ("system/commands/vmstat", "systools"),
    ("system/commands/volumed", "systools"),
    ("system/commands/w", "systools"),
    ("system/commands/watch", "systools"),
    ("system/commands/watchdog", "systools"),
    ("system/commands/which", "systools"),
    ("system/commands/who", "systools"),
    ("system/commands/xxd", "systools"),
];

/// The shell's test scripts, with their expected output, and where in
/// the boot archive they go, for the `shell` POSIX test.
pub const SHELL_CASES: &str = "userspace/programs/nukesh/tests/cases";
pub const SHELL_CASES_DIR: &str = "system/tests/shell";

/// The system's own default settings (ADR 0090), and where in the boot
/// archive they go: among the ports' defaults, which Spark copies into
/// `/system/settings` (spark(8)).
pub const DEFAULTS: &str = "userspace/defaults";
pub const DEFAULTS_DIR: &str = "system/resources/default-settings";

/// The service files of the boot tests that Spark is pointed at with
/// `--service-dir` (spark(8)), and where in the boot archive they go.
pub const TEST_SERVICES: &str = "userspace/spark/tests/services";
pub const TEST_SERVICES_DIR: &str = "system/tests/services";

/// The C and C++ test programs' sources, and where in the boot archive
/// they go.
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

/// A Cargo command for the workspace of programs on Rust's standard
/// library, writing to `target/programs`: the toolchain's Cargo for
/// Radix, which builds for their own target against the sysroot the
/// rust-std port builds, with the crates the ports patch for Radix in
/// place of crates.io's, linked by the Radix C compiler (ADR 0070).
pub fn std_cargo(ctx: &Ctx, subcommand: &str, toolchain: &ports::Toolchain) -> Command {
    let mut cmd = Command::new(toolchain.cargo());
    cmd.current_dir(&ctx.root)
        .args([subcommand, "--manifest-path", STD_MANIFEST])
        .env("CARGO_TARGET_DIR", ctx.target.join("programs"));
    cmd
}

/// The workspace of programs on Rust's standard library.
pub const STD_MANIFEST: &str = "userspace/programs/Cargo.toml";

/// The programs on Rust's standard library whose unit tests run on the
/// host: what they test does not need Radix.
pub const HOST_TESTED: &[&str] = &["consoled", "keymap", "nukesh", "systools"];

/// The Cargo.lock of the host builds of the programs on Rust's standard
/// library. They take the crates the ports patch for Radix from
/// crates.io, so they keep a lock of their own: with one lock for both,
/// each build locked the other's crates afresh, to crates.io's newest.
pub const HOST_LOCK: &str = "userspace/programs/host/Cargo.lock";

/// A Cargo command for the workspace of programs on Rust's standard
/// library, building for the host, writing to `target/programs-host`:
/// for their unit tests.
pub fn host_cargo(ctx: &Ctx, subcommand: &str) -> Command {
    let mut cmd = process::cargo(ctx);
    cmd.args([subcommand, "--manifest-path", STD_MANIFEST])
        .arg("--config")
        .arg(format!(r#"resolver.lockfile-path="{}""#, ctx.root.join(HOST_LOCK).display()))
        .arg("--target-dir")
        .arg(ctx.target.join("programs-host"));
    for package in HOST_TESTED {
        cmd.args(["-p", package]);
    }
    cmd
}

/// Checks that the programs on Rust's standard library take each crate
/// the ports patch for Radix from the port, not from crates.io: Cargo
/// keeps a version Cargo.lock names that the port has no patch for.
fn check_patched(ctx: &Ctx, toolchain: &ports::Toolchain) -> Result<()> {
    let patches = toolchain.crate_patches();
    let mut patched: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir(&patches).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some((name, version)) = name.strip_suffix(".toml").and_then(|n| n.rsplit_once('-')) {
            patched.push((name.to_string(), version.to_string()));
        }
    }
    let path = ctx.root.join("userspace/programs/Cargo.lock");
    let text = std::fs::read_to_string(&path).context(|| format!("reading {}", path.display()))?;
    let lock: toml::Value = toml::from_str(&text)
        .map_err(|err| crate::Error::new(format!("{}: {err}", path.display())))?;
    for package in lock.get("package").and_then(|p| p.as_array()).into_iter().flatten() {
        let field = |key: &str| package.get(key).and_then(|v| v.as_str()).unwrap_or_default();
        let (name, version) = (field("name"), field("version"));
        if field("source").is_empty() {
            continue;
        }
        let versions: Vec<&str> =
            patched.iter().filter(|(n, _)| n == name).map(|(_, v)| v.as_str()).collect();
        if !versions.is_empty() {
            return Err(crate::Error::new(format!(
                "userspace/programs uses {name} {version} from crates.io, which does not \
                 build for Radix"
            ))
            .hint(format!(
                "the ports patch {name} {}: run `cargo update --manifest-path {STD_MANIFEST} \
                 -p {name}@{version} --precise {}`",
                versions.join(" and "),
                versions[0]
            )));
        }
    }
    Ok(())
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
    install_tree(ctx, DEFAULTS, DEFAULTS_DIR, &mut files)?;
    install_tree(ctx, TEST_SERVICES, TEST_SERVICES_DIR, &mut files)?;
    for port in &ports.ports {
        files.extend(ports::image_files(ctx, &port.recipe, &ctx.out.join("sysroot"))?);
    }
    files.extend(build_std(ctx, profile, &ports.toolchain)?);
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
    let size = std::fs::metadata(&installed).map_or(0, |m| m.len());
    step(&format!(
        "User test programs installed at {} ({} KiB)",
        ctx.display(&installed),
        size / 1024
    ));
    let bootfs = ctx.out.join("sysroot").join(BOOTFS_PATH.trim_start_matches('/'));
    for (path, package) in ARCHIVE {
        files.push((path.to_string(), built.join(package)));
    }
    crate::manuals::install(ctx, &mut files)?;
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

/// Copies the files beneath `source`, a directory of the repository,
/// into `out/sysroot` at `dir`, in name order, and adds each one's place
/// in the boot archive and file to `files`.
fn install_tree(
    ctx: &Ctx,
    source: &str,
    dir: &str,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let from = ctx.root.join(source);
    let mut entries: Vec<(String, bool)> = std::fs::read_dir(&from)
        .context(|| format!("reading {}", from.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let is_dir = entry.file_type().ok()?.is_dir();
            Some((entry.file_name().to_string_lossy().into_owned(), is_dir))
        })
        .collect();
    entries.sort();
    for (name, is_dir) in entries {
        if is_dir {
            install_tree(ctx, &format!("{source}/{name}"), &format!("{dir}/{name}"), files)?;
            continue;
        }
        let installed = ctx.out.join("sysroot").join(dir).join(&name);
        process::copy(&from.join(&name), &installed)?;
        files.push((format!("{dir}/{name}"), installed));
    }
    Ok(())
}

/// Builds the programs of [`STD_ARCHIVE`] into `out/sysroot`, with their
/// links and the shell's test scripts, and returns each one's place in the
/// boot archive and file.
fn build_std(
    ctx: &Ctx,
    profile: Profile,
    toolchain: &ports::Toolchain,
) -> Result<Vec<(String, PathBuf)>> {
    step(&format!("Building the programs on Rust's standard library ({profile})"));
    let mut cmd = std_cargo(ctx, "build", toolchain);
    for (_, package) in STD_ARCHIVE {
        cmd.args(["-p", package]);
    }
    if profile == Profile::Release {
        cmd.arg("--release");
    }
    process::run(ctx, &mut cmd)?;
    check_patched(ctx, toolchain)?;
    let built = ctx.target.join("programs").join(STD_TARGET_NAME).join(profile.dir());
    let mut files = Vec::new();
    for (path, package) in STD_ARCHIVE {
        let installed = ctx.out.join("sysroot").join(path);
        process::copy(&built.join(package), &installed)?;
        files.push((path.to_string(), installed));
    }
    for (path, target) in STD_LINKS {
        let link = ctx.out.join("sysroot").join(path);
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context(|| format!("removing {}", link.display()))?;
        }
        std::os::unix::fs::symlink(target, &link)
            .context(|| format!("linking {} to {target}", link.display()))?;
        files.push((path.to_string(), link));
    }
    let cases = ctx.root.join(SHELL_CASES);
    let mut names: Vec<String> = std::fs::read_dir(&cases)
        .context(|| format!("reading {}", cases.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|name| matches!(Path::new(name).extension(), Some(e) if e == "sh" || e == "out"))
        .collect();
    names.sort();
    for name in names {
        let installed = ctx.out.join("sysroot").join(SHELL_CASES_DIR).join(&name);
        process::copy(&cases.join(&name), &installed)?;
        files.push((format!("{SHELL_CASES_DIR}/{name}"), installed));
    }
    Ok(files)
}

/// Builds each C test program of [`POSIX_SOURCES`] (one per `.c` file,
/// with the headers there), and each C++ one (one per `.cpp` file, on
/// libc++, ADR 0090), into `out/sysroot/system/tests/posix`, and
/// the dynamically linked one of its `dynamic` directory (ADR 0071), and
/// returns each one's place in the boot archive and file.
fn build_posix(ctx: &Ctx, toolchain: &ports::Toolchain) -> Result<Vec<(String, PathBuf)>> {
    let sources = ctx.root.join(POSIX_SOURCES);
    let out = ctx.out.join("sysroot").join(POSIX_DIR);
    process::create_dir(&out)?;
    let mut programs: Vec<PathBuf> = std::fs::read_dir(&sources)
        .context(|| format!("reading {}", sources.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "c" || e == "cpp"))
        .collect();
    programs.sort();
    step(&format!("Building {} C and C++ test programs", programs.len()));
    let mut files = Vec::new();
    for source in programs {
        let name = source.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let program = out.join(&name);
        let (compiler, standard) = if source.extension().is_some_and(|e| e == "cpp") {
            (toolchain.cxx(), "-std=c++23")
        } else {
            (toolchain.cc(), "-std=c11")
        };
        let mut cmd = Command::new(compiler);
        cmd.args([standard, "-D_GNU_SOURCE"])
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-I"])
            .arg(&sources)
            .arg("-o")
            .arg(&program)
            .arg(&source);
        process::run(ctx, &mut cmd)?;
        files.push((format!("{POSIX_DIR}/{name}"), program));
    }
    files.extend(build_dynamic(ctx, toolchain)?);
    Ok(files)
}

/// Builds the `dynamic` test as a bundle's program is built (ADR 0071):
/// the program, on the shared C library, with its private library in
/// `lib` beside it, found through its run path, and a library it opens
/// with dlopen there too.
fn build_dynamic(ctx: &Ctx, toolchain: &ports::Toolchain) -> Result<Vec<(String, PathBuf)>> {
    let posix = ctx.root.join(POSIX_SOURCES);
    let sources = posix.join("dynamic");
    let out = ctx.out.join("sysroot").join(POSIX_DIR);
    let lib = out.join("lib");
    process::create_dir(&lib)?;
    step("Building the dynamically linked C test program and its libraries");
    // Each one's file, mode, source and further arguments.
    let parts: [(PathBuf, &str, &str, &[&str]); 3] = [
        (lib.join("libgreet.so"), "-shared", "greet.c", &["-fPIC", "-Wl,-soname,libgreet.so"]),
        (lib.join("plugin.so"), "-shared", "plugin.c", &["-fPIC", "-lgreet", "-Wl,-rpath,$ORIGIN"]),
        (out.join("dynamic"), "-dynamic", "main.c", &["-lgreet", "-Wl,-rpath,$ORIGIN/lib"]),
    ];
    let mut files = Vec::new();
    for (file, mode, source, args) in parts {
        let mut cmd = Command::new(toolchain.cc());
        cmd.arg(mode)
            .args(["-std=c11", "-D_GNU_SOURCE"])
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-I"])
            .arg(&posix)
            .arg("-I")
            .arg(&sources)
            .arg("-o")
            .arg(&file)
            .arg(sources.join(source))
            .arg("-L")
            .arg(&lib)
            .args(args);
        process::run(ctx, &mut cmd)?;
        let inside = file.strip_prefix(ctx.out.join("sysroot")).unwrap_or(&file);
        files.push((inside.to_string_lossy().into_owned(), file));
    }
    Ok(files)
}

/// What a path of the boot archive is.
enum Entry<'a> {
    Directory,
    /// A file, or a symbolic link, on the host.
    File(&'a Path),
    /// A symbolic link with this target.
    Link(&'a str),
    /// A file with this text.
    Text(&'a str),
}

/// The boot archive: `files`, each a path in the archive and the file
/// it holds (a symbolic link stays one, with its target), the
/// directories above them, those of [`ARCHIVE_DIRS`], the links of
/// [`ARCHIVE_LINKS`] and [`ARCHIVE_HIDDEN`], owned by root and dated
/// `epoch` (the commit's time), so the same programs make the same
/// bytes. A file is 0755 if its source may be run by anyone (a program
/// or a script), and 0644 otherwise (settings, manual pages, a test's
/// expected output), as Linux installs them. No file may be under one of
/// the links: it belongs in the folder the link leads to.
fn write_archive(files: &[(String, PathBuf)], epoch: Option<String>) -> Result<Vec<u8>> {
    let mtime = epoch.and_then(|epoch| epoch.parse().ok()).unwrap_or(0);
    let mut entries: Vec<(String, Entry<'_>)> = Vec::new();
    for dir in ARCHIVE_DIRS {
        entries.push((dir.to_string(), Entry::Directory));
    }
    let mut leaves: Vec<(String, Entry<'_>)> = Vec::new();
    for (link, target) in ARCHIVE_LINKS {
        leaves.push((link.to_string(), Entry::Link(target)));
    }
    leaves.push((ARCHIVE_HIDDEN.0.to_string(), Entry::Text(ARCHIVE_HIDDEN.1)));
    for (path, file) in files {
        if let Some((link, _)) = ARCHIVE_LINKS
            .iter()
            .find(|(link, _)| path.strip_prefix(link).is_some_and(|rest| rest.starts_with('/')))
        {
            return Err(crate::Error::new(format!(
                "the boot archive: {path} is under the link /{link}"
            )));
        }
        leaves.push((path.clone(), Entry::File(file)));
    }
    for (path, entry) in leaves {
        // Each directory above it, once.
        for (end, _) in path.match_indices('/') {
            let dir = &path[..end];
            if !entries.iter().any(|(seen, _)| seen == dir) {
                entries.push((dir.to_string(), Entry::Directory));
            }
        }
        entries.push((path, entry));
    }
    // A directory before what is in it, as the archive's reader wants.
    entries.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
    let mut writer = radix_bootfs::write::Writer::new();
    for (path, entry) in &entries {
        let written = match entry {
            Entry::Directory => writer.directory(path, 0o755, mtime),
            Entry::Link(target) => writer.symlink(path, target, mtime),
            Entry::Text(text) => writer.file(path, 0o644, mtime, text.as_bytes()),
            Entry::File(file) if file.is_symlink() => {
                let target =
                    std::fs::read_link(file).context(|| format!("reading {}", file.display()))?;
                let target = target.to_str().ok_or_else(|| {
                    crate::Error::new(format!("the link {} is not UTF-8", file.display()))
                })?;
                writer.symlink(path, target, mtime)
            }
            Entry::File(file) => {
                let data = std::fs::read(file).context(|| format!("reading {}", file.display()))?;
                let mode = if is_executable(file)? { 0o755 } else { 0o644 };
                writer.file(path, mode, mtime, &data)
            }
        };
        written.map_err(|err| crate::Error::new(format!("the boot archive: {path}: {err}")))?;
    }
    Ok(writer.finish())
}

/// Whether the file at `path` has an execute bit set.
fn is_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::metadata(path).context(|| format!("reading {}", path.display()))?;
    Ok(meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn the_archive_holds_the_programs_under_their_directories() {
        let dir = std::env::temp_dir().join(format!("ob-archive-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut files = Vec::new();
        for (path, package) in ARCHIVE {
            std::fs::write(dir.join(package), format!("the {package} program")).unwrap();
            std::fs::set_permissions(dir.join(package), Permissions::from_mode(0o755)).unwrap();
            files.push((path.to_string(), dir.join(package)));
        }
        std::fs::write(dir.join("hosts"), "127.0.0.1 localhost\n").unwrap();
        std::fs::set_permissions(dir.join("hosts"), Permissions::from_mode(0o644)).unwrap();
        files.push(("system/settings/hosts".to_string(), dir.join("hosts")));
        std::os::unix::fs::symlink("spark", dir.join("init")).unwrap();
        files.push(("system/commands/init".to_string(), dir.join("init")));
        let bytes = write_archive(&files, Some("1700000000".into())).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let archive = radix_bootfs::open(&bytes).unwrap();
        let paths: Vec<String> = archive.entries().map(|e| e.path().to_string()).collect();
        assert_eq!(
            paths,
            [
                "system",
                "system/.hidden",
                "system/bin",
                "system/boot",
                "system/commands",
                "system/commands/init",
                "system/commands/spark",
                "system/lib",
                "system/live",
                "system/settings",
                "system/settings/hosts",
                "system/share",
                "system/state",
                "system/tests",
                "system/tests/hello",
                "system/tests/storage"
            ]
        );
        let spark = archive.find("/system/commands/spark").unwrap();
        assert_eq!(spark.data, b"the spark program");
        assert_eq!((spark.mode, spark.mtime), (0o755, 1_700_000_000));
        assert_eq!(archive.find("/system/settings/hosts").unwrap().mode, 0o644);
        let init = archive.find("/system/commands/init").unwrap();
        assert_eq!((init.kind, init.data), (radix_bootfs::Kind::Symlink, &b"spark"[..]));
        let bin = archive.find("/system/bin").unwrap();
        assert_eq!((bin.kind, bin.data), (radix_bootfs::Kind::Symlink, &b"commands"[..]));
        let hidden = archive.find("/system/.hidden").unwrap();
        assert_eq!((hidden.mode, hidden.data), (0o644, &b"bin\nlib\nshare\n"[..]));
        assert_eq!(write_archive(&files, None).ok(), None, "the programs are gone");
        let under_a_link = [("system/bin/ls".to_string(), dir.join("ls"))];
        assert!(write_archive(&under_a_link, None).is_err(), "a file under /system/bin");
    }
}
