// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Building the Radix kernel.

use std::path::{Path, PathBuf};

use crate::config::Profile;
use crate::process::{self, step};
use crate::{Context, Ctx, Result, bail, doctor, frames, krelocs, ksyms};

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
        "-Zbuild-std=core,compiler_builtins,alloc".into(),
        "-Zbuild-std-features=compiler-builtins-mem".into(),
        // `alloc` without its infallible APIs (`Box::new`, `Vec::push`):
        // every kernel allocation states its context and handles failure
        // (ADR 0017),
        "--config".into(),
        // and LLVM records each function's stack frame in `.stack_sizes`,
        // which `ob` holds to the kernel's frame budget after linking
        // (ADR 0058).
        format!(
            r#"target.{TARGET_NAME}.rustflags=["--cfg","no_global_oom_handling","-Zemit-stack-sizes"]"#
        ),
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

/// Builds the kernel, writes its symbol and relocation tables into it, and
/// installs it as `out/sysroot/boot/radix`.
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
    install(ctx, &built, &installed, profile)?;
    Ok(installed)
}

fn install(ctx: &Ctx, built: &Path, installed: &Path, profile: Profile) -> Result<()> {
    let mut bytes = std::fs::read(built).context(|| format!("reading {}", built.display()))?;
    // Only the optimised kernel, which CI tests and images carry, is held
    // to the frame budget: without optimisation, frames are several times
    // larger.
    let frames = match profile {
        Profile::Release => {
            format!("; {} stack frames held to {} bytes", frames::check(&bytes)?, frames::BUDGET)
        }
        Profile::Debug => String::new(),
    };
    let functions = ksyms::embed(&mut bytes)?;
    let relocations = krelocs::embed(&mut bytes)?;
    if let Some(parent) = installed.parent() {
        process::create_dir(parent)?;
    }
    std::fs::write(installed, &bytes).context(|| format!("writing {}", installed.display()))?;
    // GRUB refuses to load an ELF file with relocation sections, and the
    // kernel needs only the table in .krelocs, so they go, with the frame
    // sizes, which only the check above reads.
    process::run(
        ctx,
        std::process::Command::new(objcopy(ctx)?)
            .args(["--wildcard", "--remove-section=.rela*", "--remove-section=.stack_sizes"])
            .arg(installed),
    )?;
    step(&format!(
        "Kernel installed at {} (symbol table of {functions} functions, {relocations} \
         relocations{frames})",
        ctx.display(installed)
    ));
    Ok(())
}

/// The pinned toolchain's `llvm-objcopy`, from its `llvm-tools` component.
fn objcopy(ctx: &Ctx) -> Result<PathBuf> {
    let Some(sysroot) = doctor::sysroot(ctx) else {
        bail!("cannot find the pinned toolchain; run `./ob setup`");
    };
    let rustlib = sysroot.join("lib/rustlib");
    let found = std::fs::read_dir(&rustlib)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("bin/llvm-objcopy"))
        .find(|path| path.is_file());
    match found {
        Some(path) => Ok(path),
        None => bail!("the toolchain has no llvm-objcopy; run `./ob setup` to add llvm-tools"),
    }
}
