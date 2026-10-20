//! Building the Radix kernel.

use std::path::{Path, PathBuf};

use crate::config::Profile;
use crate::process::{self, step};
use crate::{Ctx, Result, ksyms};

/// The kernel's target specification, relative to the repository root.
/// docs/adr/0011-kernel-target-and-build-std.md explains why the kernel has
/// its own target instead of `x86_64-unknown-none`.
pub const TARGET_SPEC: &str = "kernel/targets/x86_64-radix-kernel.json";

/// Name Cargo gives the target's output directory.
pub const TARGET_NAME: &str = "x86_64-radix-kernel";

/// The built-in target the AArch64 HAL stubs are checked against.
pub const AARCH64_TARGET: &str = "aarch64-unknown-none";

/// Cargo arguments that build `core` for the kernel's own target. The
/// precompiled `core` of the built-in targets omits frame pointers, which
/// the kernel's backtraces follow.
pub fn target_args(ctx: &Ctx) -> Vec<String> {
    vec![
        "--target".into(),
        ctx.root.join(TARGET_SPEC).display().to_string(),
        "-Zjson-target-spec".into(),
        "-Zbuild-std=core,compiler_builtins".into(),
        "-Zbuild-std-features=compiler-builtins-mem".into(),
    ]
}

/// A Cargo command for the kernel workspace, writing to `target/kernel`.
pub fn cargo(ctx: &Ctx, subcommand: &str) -> std::process::Command {
    let mut cmd = process::cargo(ctx);
    cmd.args([subcommand, "--manifest-path", "kernel/Cargo.toml"])
        .arg("--target-dir")
        .arg(target_dir(ctx))
        // Building `core` for a soft-float target makes Cargo print a
        // future-incompatibility notice about `core` itself on every build.
        .env("CARGO_FUTURE_INCOMPAT_REPORT_FREQUENCY", "never");
    cmd
}

/// Cargo output directory of the kernel workspace.
pub fn target_dir(ctx: &Ctx) -> PathBuf {
    ctx.target.join("kernel")
}

/// Where the kernel is installed; `ob run --gdb` loads its symbols from here.
pub fn installed(ctx: &Ctx) -> PathBuf {
    ctx.out.join("sysroot/boot/radix")
}

/// Builds the kernel, writes its symbol table into it, and installs it as
/// `out/sysroot/boot/radix`.
pub fn build(ctx: &Ctx, profile: Profile) -> Result<PathBuf> {
    step(&format!("Building the Radix kernel ({profile})"));
    let mut cmd = cargo(ctx, "build");
    cmd.args(["-p", "radix"]).args(target_args(ctx));
    if profile == Profile::Release {
        cmd.arg("--release");
    }
    process::run(ctx, &mut cmd)?;

    let built = target_dir(ctx).join(TARGET_NAME).join(profile.dir()).join("radix");
    let installed = installed(ctx);
    install(ctx, &built, &installed)?;
    Ok(installed)
}

fn install(ctx: &Ctx, built: &Path, installed: &Path) -> Result<()> {
    let functions = ksyms::embed(built, installed)?;
    step(&format!(
        "Kernel installed at {} (symbol table of {functions} functions)",
        ctx.display(installed)
    ));
    Ok(())
}
