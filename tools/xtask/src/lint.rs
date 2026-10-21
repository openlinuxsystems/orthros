// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob lint` and `ob fmt`.

use crate::process::{self, step};
use crate::{Ctx, Result, kernel, test};

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

    step("Running Clippy on the build tool");
    let mut cmd = process::cargo(ctx);
    cmd.args(["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)?;

    step("Running Clippy on the kernel (x86-64)");
    let mut cmd = kernel::cargo(ctx, "clippy");
    cmd.args(["-p", "radix", "-p", "radix-arch-x86_64"])
        .args(kernel::target_args(ctx))
        .args(["--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)?;

    // The common kernel must build against the HAL alone; the AArch64 stubs
    // implement every trait, so x86-64 assumptions fail here.
    step("Running Clippy on the kernel (AArch64, against the HAL stubs)");
    let mut cmd = kernel::cargo(ctx, "clippy");
    cmd.args(["-p", "radix", "-p", "radix-arch-aarch64", "--target", kernel::AARCH64_TARGET])
        .args(["--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)?;

    step("Running Clippy on the kernel libraries and their host tests");
    let mut cmd = kernel::cargo(ctx, "clippy");
    for library in test::KERNEL_LIBRARIES {
        cmd.args(["-p", library]);
    }
    cmd.args(["--all-targets", "--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)
}
