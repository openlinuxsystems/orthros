//! `ob lint` and `ob fmt`.

use crate::process::{self, step};
use crate::{Ctx, Result, kernel};

/// The Cargo workspaces in the repository.
const MANIFESTS: &[&str] = &["Cargo.toml", "kernel/Cargo.toml"];

/// `ob fmt`.
pub fn fmt(ctx: &Ctx) -> Result<()> {
    step("Formatting Rust code");
    for manifest in MANIFESTS {
        let mut cmd = process::cargo(ctx);
        cmd.args(["fmt", "--all", "--manifest-path", manifest]);
        process::run(ctx, &mut cmd)?;
    }
    Ok(())
}

/// `ob lint`.
pub fn lint(ctx: &Ctx) -> Result<()> {
    step("Checking formatting");
    for manifest in MANIFESTS {
        let mut cmd = process::cargo(ctx);
        cmd.args(["fmt", "--all", "--check", "--manifest-path", manifest]);
        process::run(ctx, &mut cmd).map_err(|err| err.hint("run `./ob fmt` to fix formatting"))?;
    }

    let kernel_target_dir = ctx.target.join("kernel");
    step("Running Clippy on the build tool");
    let mut cmd = process::cargo(ctx);
    cmd.args(["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)?;

    step("Running Clippy on the kernel");
    let mut cmd = process::cargo(ctx);
    cmd.args([
        "clippy",
        "--manifest-path",
        "kernel/Cargo.toml",
        "-p",
        "radix",
        "-p",
        "radix-arch-x86_64",
        "--target",
        kernel::TARGET,
    ])
    .arg("--target-dir")
    .arg(&kernel_target_dir)
    .args(["--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)?;

    step("Running Clippy on the kernel libraries and their host tests");
    let mut cmd = process::cargo(ctx);
    cmd.args([
        "clippy",
        "--manifest-path",
        "kernel/Cargo.toml",
        "-p",
        "radix-bootinfo",
        "--all-targets",
    ])
    .arg("--target-dir")
    .arg(&kernel_target_dir)
    .args(["--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)
}
