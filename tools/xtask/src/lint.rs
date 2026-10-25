// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob lint` and `ob fmt`.

use crate::process::{self, step};
use crate::{Ctx, Result, kernel, ports, test, userspace};

/// The Cargo workspaces in the repository.
const MANIFESTS: &[&str] =
    &["Cargo.toml", "kernel/Cargo.toml", "userspace/Cargo.toml", "userspace/programs/Cargo.toml"];

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
    process::run(ctx, &mut cmd)?;

    step("Running Clippy on the host tests of the programs on Rust's standard library");
    let mut cmd = userspace::host_cargo(ctx, "clippy");
    cmd.args(["--all-targets", "--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)?;

    step("Running Clippy on the user programs");
    let mut cmd = userspace::cargo(ctx, "clippy");
    cmd.arg("--workspace").args(userspace::target_args(ctx)).args(["--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)?;

    // These need Rust's standard library for Radix, which the rust-std
    // port builds.
    userspace::install_headers(ctx)?;
    let ports = ports::build_all(ctx, &[])?;
    step("Running Clippy on the programs on Rust's standard library");
    let mut cmd = userspace::std_cargo(ctx, "clippy", &ports.toolchain);
    cmd.arg("--workspace").args(["--", "-D", "warnings"]);
    process::run(ctx, &mut cmd)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::MANIFESTS;

    /// The manifest of the bench program, which lints as the workspaces do
    /// but with lints of its own.
    const BENCH: &str = "tools/linux-bench/Cargo.toml";

    /// The comment that opens each manifest's set of Clippy's pedantic lints.
    const OPENS: &str = "# Clippy's pedantic lints that find bugs";

    /// The lines of the set in `manifest`, after its comment, up to a blank
    /// line or the next table.
    fn pedantic(manifest: &str) -> Vec<&str> {
        manifest
            .lines()
            .skip_while(|line| !line.starts_with(OPENS))
            .skip_while(|line| line.starts_with('#'))
            .take_while(|line| !line.is_empty() && !line.starts_with('['))
            .collect()
    }

    #[test]
    fn every_workspace_turns_on_the_same_pedantic_lints() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap();
        let first = read(MANIFESTS[0]);
        let set = pedantic(&first);
        assert!(set.len() > 30, "{} has no set of pedantic lints", MANIFESTS[0]);
        assert!(set.iter().all(|line| line.ends_with(" = \"warn\"")), "{set:?}");
        for manifest in MANIFESTS[1..].iter().chain([&BENCH]) {
            let text = read(manifest);
            assert_eq!(pedantic(&text), set, "{manifest}'s set differs from {}'s", MANIFESTS[0]);
        }
    }
}
