// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob test tla`: checks the TLA+ models with TLC (ADR 0024).
//!
//! TLC comes from the TLA+ tools release pinned here, which `ob` downloads
//! once into `.cache/tla/` and checks against its SHA-256. It runs on Java
//! 11 or later. The tools are MIT-licensed and only run on the build host.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::process::{self, step};
use crate::{Context, Ctx, Error, Result};

/// TLA+ tools release the models are checked with (ADR 0024).
pub const TLA_VERSION: &str = "1.7.4";

/// SHA-256 of that release's `tla2tools.jar`.
const TLA_SHA256: &str = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88";

/// The models: a specification and a TLC configuration beside it.
pub const MODELS: &[(&str, &str)] = &[
    ("kernel/cap/tla/Capabilities.tla", "Capabilities.cfg"),
    ("kernel/cap/tla/Capabilities.tla", "CapabilitiesRetype.cfg"),
    ("kernel/cap/tla/Capabilities.tla", "CapabilitiesLiveness.cfg"),
    ("kernel/sched/tla/Scheduler.tla", "Scheduler.cfg"),
    ("kernel/sched/tla/Scheduler.tla", "SchedulerLiveness.cfg"),
    ("kernel/sched/tla/IdleWake.tla", "IdleWake.cfg"),
    ("kernel/ipc/tla/Endpoint.tla", "Endpoint.cfg"),
    ("kernel/ipc/tla/Endpoint.tla", "EndpointLiveness.cfg"),
    ("userspace/libradix/tla/Channel.tla", "Channel.cfg"),
    ("kernel/fs/fat/tla/Clusters.tla", "Clusters.cfg"),
    ("kernel/fs/btrfs/tla/Commit.tla", "Commit.cfg"),
    ("kernel/fs/btrfs/tla/Commit.tla", "CommitValues.cfg"),
    ("kernel/radix/tla/Futex.tla", "Futex.cfg"),
    ("kernel/radix/tla/Futex.tla", "FutexBatch.cfg"),
    ("kernel/radix/tla/Poll.tla", "Poll.cfg"),
    ("kernel/radix/tla/Timers.tla", "Timers.cfg"),
    ("kernel/radix/tla/FileLocks.tla", "FileLocks.cfg"),
    ("kernel/radix/tla/Join.tla", "Join.cfg"),
];

/// How to install Java.
pub const JAVA_INSTALL: &str =
    "install Java 11 or later (Debian, Ubuntu, Mint: `sudo apt install default-jre-headless`)";

/// Checks every model.
pub fn check(ctx: &Ctx) -> Result<()> {
    if process::find_program("java").is_none() {
        return Err(Error::new("Java is not installed; TLC needs it").hint(JAVA_INSTALL));
    }
    let jar = tools(ctx)?;
    for (spec, config) in MODELS {
        let spec = ctx.root.join(spec);
        let dir = spec.parent().unwrap_or(Path::new("."));
        let name = config.trim_end_matches(".cfg");
        step(&format!("Checking the TLA+ model {name} with TLC"));
        let metadir = ctx.out.join("tla").join(name);
        let mut cmd = Command::new("java");
        cmd.current_dir(dir)
            .arg("-XX:+UseParallelGC")
            .arg("-cp")
            .arg(&jar)
            .args(["tlc2.TLC", "-workers", "auto", "-cleanup", "-config", config])
            .arg("-metadir")
            .arg(&metadir)
            .arg(spec.file_name().unwrap_or_default());
        process::run(ctx, &mut cmd)?;
    }
    Ok(())
}

/// The pinned `tla2tools.jar`, downloaded if it is not cached yet.
fn tools(ctx: &Ctx) -> Result<PathBuf> {
    let dir = ctx.root.join(".cache/tla");
    let jar = dir.join(format!("tla2tools-{TLA_VERSION}.jar"));
    if jar.exists() && process::sha256(ctx, &jar)? == TLA_SHA256 {
        return Ok(jar);
    }
    let url = format!(
        "https://github.com/tlaplus/tlaplus/releases/download/v{TLA_VERSION}/tla2tools.jar"
    );
    step(&format!("Downloading the TLA+ tools {TLA_VERSION}"));
    fs::create_dir_all(&dir).context(|| format!("cannot create {}", dir.display()))?;
    let partial = dir.join("tla2tools.jar.partial");
    let mut cmd = Command::new("curl");
    cmd.args(["--proto", "=https", "--tlsv1.2", "-fsSL", "-o"]).arg(&partial).arg(&url);
    process::run(ctx, &mut cmd)?;
    let sum = process::sha256(ctx, &partial)?;
    if sum != TLA_SHA256 {
        let _ = fs::remove_file(&partial);
        return Err(Error::new(format!(
            "{url} has SHA-256 {sum}, not the pinned {TLA_SHA256}; not using it"
        )));
    }
    fs::rename(&partial, &jar).context(|| format!("cannot move it to {}", jar.display()))?;
    Ok(jar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for (spec, config) in MODELS {
            let spec = root.join(spec);
            assert!(spec.exists(), "{} is missing", spec.display());
            assert!(spec.with_file_name(config).exists(), "{config} is missing");
        }
    }

    #[test]
    fn ci_pins_the_same_tla_tools() {
        let ci = include_str!("../../../.github/workflows/ci.yml").replace(": ", "=");
        let pin = format!("TLA_VERSION={TLA_VERSION}");
        assert!(ci.contains(&pin), ".github/workflows/ci.yml pins other TLA+ tools");
    }
}
