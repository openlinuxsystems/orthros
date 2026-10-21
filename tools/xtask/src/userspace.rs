// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Building the user programs in userspace/.
//!
//! Until Phase 6 brings init and a file system, the only user programs are
//! the test and benchmark programs, linked into one image that the boot
//! loader loads as a module (`usertests`) and the kernel's `user`, `ipc`
//! and `bench` self-tests start (ADR 0036).

use std::path::PathBuf;

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

/// Cargo arguments that build `core` for the user target.
pub fn target_args(ctx: &Ctx) -> Vec<String> {
    vec![
        "--target".into(),
        ctx.root.join(TARGET_SPEC).display().to_string(),
        "-Zjson-target-spec".into(),
        "-Zbuild-std=core,compiler_builtins".into(),
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

/// Builds the user programs and installs the test image as
/// `out/sysroot/boot/usertests`.
pub fn build(ctx: &Ctx, profile: Profile) -> Result<PathBuf> {
    step(&format!("Building the user programs ({profile})"));
    let mut cmd = cargo(ctx, "build");
    cmd.args(["-p", "usertests"]).args(target_args(ctx));
    if profile == Profile::Release {
        cmd.arg("--release");
    }
    process::run(ctx, &mut cmd)?;
    let built =
        ctx.target.join("userspace").join(TARGET_NAME).join(profile.dir()).join("usertests");
    let installed = installed(ctx);
    if let Some(parent) = installed.parent() {
        process::create_dir(parent)?;
    }
    std::fs::copy(&built, &installed)
        .context(|| format!("copying {} to {}", built.display(), installed.display()))?;
    let size = std::fs::metadata(&installed).map(|m| m.len()).unwrap_or(0);
    step(&format!(
        "User test programs installed at {} ({} KiB)",
        ctx.display(&installed),
        size / 1024
    ));
    Ok(installed)
}
