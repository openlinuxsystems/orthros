//! Building the Radix kernel.

use std::path::PathBuf;

use crate::config::Profile;
use crate::process::{self, step};
use crate::{Ctx, Result};

/// Rust target the kernel is compiled for.
pub const TARGET: &str = "x86_64-unknown-none";

/// Builds the kernel and installs it as `out/sysroot/boot/radix`.
pub fn build(ctx: &Ctx, profile: Profile) -> Result<PathBuf> {
    step(&format!("Building the Radix kernel ({profile})"));
    let target_dir = ctx.target.join("kernel");
    let mut cmd = process::cargo(ctx);
    cmd.args(["build", "--manifest-path", "kernel/Cargo.toml", "-p", "radix", "--target", TARGET])
        .arg("--target-dir")
        .arg(&target_dir);
    if profile == Profile::Release {
        cmd.arg("--release");
    }
    process::run(ctx, &mut cmd)?;

    let built = target_dir.join(TARGET).join(profile.dir()).join("radix");
    let installed = ctx.out.join("sysroot/boot/radix");
    process::copy(&built, &installed)?;
    step(&format!("Kernel installed at {}", ctx.display(&installed)));
    Ok(installed)
}
