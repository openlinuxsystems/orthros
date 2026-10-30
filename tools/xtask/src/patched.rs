// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The crates from crates.io that the kernel builds with changes of its
//! own (ADR 0182), as the ports patch theirs (ADR 0080).
//!
//! `kernel/patches/crates.toml` lists each crate with the SHA-256 of its
//! `.crate` file, and `kernel/patches/<name>-<version>.patch` holds the
//! change. [`ensure`] fetches each crate once, as the ports' crates are
//! fetched, checks its hash, applies the patch and puts the result in
//! `kernel/patched/<name>-<version>`, where `[patch.crates-io]` in
//! kernel/Cargo.toml takes it from in place of crates.io's. Every Cargo
//! command `ob` runs on the kernel calls it first; `ob crates` runs it
//! alone, for Cargo commands run by hand, such as CI's cargo-deny.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ports::{self, Crate};
use crate::process::{self, step};
use crate::{Context, Ctx, Result};

/// The list of patched crates, relative to the repository root.
pub const LIST: &str = "kernel/patches/crates.toml";
/// Where they are put, relative to the repository root.
pub const DIR: &str = "kernel/patched";
/// The file in each patched crate that says what it was made from.
const STAMP: &str = ".patched";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    #[serde(rename = "crate", default)]
    crates: Vec<Crate>,
}

/// The crates `kernel/patches/crates.toml` lists.
pub fn crates(ctx: &Ctx) -> Result<Vec<Crate>> {
    let path = ctx.root.join(LIST);
    let text = std::fs::read_to_string(&path).context(|| format!("reading {}", path.display()))?;
    parse(&text).map_err(|err| crate::Error::new(format!("{}: {err}", path.display())))
}

fn parse(text: &str) -> std::result::Result<Vec<Crate>, String> {
    let list: List = toml::from_str(text).map_err(|err| err.to_string())?;
    for krate in &list.crates {
        if krate.path.is_some() {
            return Err(format!("{} has a path; a patched crate goes in {DIR}", krate.name));
        }
    }
    Ok(list.crates)
}

/// Where the patched copy of `krate` is.
pub fn dir(ctx: &Ctx, krate: &Crate) -> PathBuf {
    ctx.root.join(DIR).join(krate.path())
}

/// Puts every listed crate, patched, in `kernel/patched`, unless it is
/// there already from the same `.crate` file and patch; and removes the
/// patched crates the list no longer has.
pub fn ensure(ctx: &Ctx) -> Result<()> {
    let crates = crates(ctx)?;
    let patches = ctx.root.join("kernel/patches");
    for krate in &crates {
        let patch = patches.join(format!("{}.patch", krate.path()));
        let text =
            std::fs::read_to_string(&patch).context(|| format!("reading {}", patch.display()))?;
        // The stamp is the crate's hash and the whole patch: a change to
        // either makes the crate again.
        let stamp = format!("{}\n{text}", krate.sha256);
        let dir = dir(ctx, krate);
        if std::fs::read_to_string(dir.join(STAMP)).ok().as_deref() == Some(stamp.as_str()) {
            continue;
        }
        let source = ports::fetch_crate(ctx, krate)?;
        step(&format!("Patching crate {} {} for the kernel", krate.name, krate.version));
        make(ctx, &source, &patch, &dir, &stamp)?;
    }
    remove_unlisted(ctx, &crates)
}

/// Makes `dir` a copy of `source` with `patch` applied: in a directory
/// beside it first, so that `dir` is only ever a whole crate.
fn make(ctx: &Ctx, source: &Path, patch: &Path, dir: &Path, stamp: &str) -> Result<()> {
    let partial = dir.with_extension("partial");
    for old in [dir, partial.as_path()] {
        if old.exists() {
            std::fs::remove_dir_all(old).context(|| format!("removing {}", old.display()))?;
        }
    }
    ports::copy_tree(ctx, source, &partial)?;
    // Cargo checks a vendored crate's files against this; a path crate
    // has none.
    let checksum = partial.join(".cargo-checksum.json");
    if checksum.exists() {
        std::fs::remove_file(&checksum).context(|| format!("removing {}", checksum.display()))?;
    }
    if process::find_program("patch").is_none() {
        return Err(crate::Error::new("patch is not installed")
            .hint("the kernel's patched crates need it; run ./ob doctor for the command"));
    }
    let mut cmd = std::process::Command::new("patch");
    cmd.args(["-p1", "--batch", "--forward", "--quiet", "-d"]).arg(&partial).arg("-i").arg(patch);
    process::run(ctx, &mut cmd)?;
    ports::write_file(&partial.join(STAMP), stamp)?;
    std::fs::rename(&partial, dir).context(|| format!("moving it to {}", dir.display()))
}

/// Removes the crates in `kernel/patched` that the list has no more, as
/// after a version changes: only those `ensure` made, which have a stamp.
fn remove_unlisted(ctx: &Ctx, crates: &[Crate]) -> Result<()> {
    let root = ctx.root.join(DIR);
    let Ok(entries) = std::fs::read_dir(&root) else { return Ok(()) };
    for entry in entries.flatten() {
        let path = entry.path();
        let listed = crates.iter().any(|krate| dir(ctx, krate) == path);
        if !listed && path.join(STAMP).is_file() {
            step(&format!("Removing {}, which {LIST} no longer lists", ctx.display(&path)));
            std::fs::remove_dir_all(&path).context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_parses() {
        let text = include_str!("../../../kernel/patches/crates.toml");
        let crates = parse(text).unwrap();
        assert!(crates.iter().any(|krate| krate.name == "smoltcp"));
        for krate in &crates {
            assert_eq!(krate.sha256.len(), 64, "{}", krate.name);
            assert!(krate.sha256.bytes().all(|b| b.is_ascii_hexdigit()), "{}", krate.name);
        }
    }

    #[test]
    fn a_path_is_refused() {
        let text = "[[crate]]\nname = \"a\"\nversion = \"1.0.0\"\nsha256 = \"00\"\n\
                    license = \"MIT\"\npath = \"elsewhere\"\n";
        assert!(parse(text).unwrap_err().contains("has a path"));
        assert!(parse("[[crate]]\nname = \"a\"\n").is_err());
    }

    /// Each listed crate has its patch, and kernel/Cargo.toml takes each
    /// from where `ensure` puts it, and no other crate from there.
    #[test]
    fn every_crate_has_its_patch_and_its_entry() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let crates = parse(include_str!("../../../kernel/patches/crates.toml")).unwrap();
        let manifest = include_str!("../../../kernel/Cargo.toml");
        for krate in &crates {
            let patch = root.join("kernel/patches").join(format!("{}.patch", krate.path()));
            assert!(patch.is_file(), "{} has no patch", krate.name);
            let entry = format!(
                "{} = {{ path = \"patched/{}-{}\" }}",
                krate.name, krate.name, krate.version
            );
            assert!(manifest.contains(&entry), "kernel/Cargo.toml lacks `{entry}`");
        }
        let entries = manifest.matches("path = \"patched/").count();
        assert_eq!(entries, crates.len(), "kernel/Cargo.toml patches crates the list lacks");
    }
}
