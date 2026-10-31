// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! bundle(1): making and checking a bundle's manifest.json
//! (manifest.json(5), ADR 0221), with the reader the system installs
//! bundles with.
//!
//! - `bundle new ID [FOLDER]` writes the manifest of a new bundle.
//! - `bundle check [FOLDER | FILE]` says what is wrong with one, by line,
//!   and that the programs it names are in the folder.
//! - `bundle schema` prints the JSON Schema of the manifest, for editors.
//! - `bundle upgrade-manifest [FOLDER | FILE]` brings a manifest up to the
//!   newest version this Dukovo reads.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use manifest::bundle::{self, Manifest};

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail, usage};

const USAGE: &str =
    "Usage: bundle new ID [FOLDER] [--name NAME] [--publisher NAME] [--license SPDX]
       bundle check [FOLDER | FILE]
       bundle schema
       bundle upgrade-manifest [FOLDER | FILE]";

/// The manifest's name in a bundle's folder.
const MANIFEST: &str = "manifest.json";

/// The license `bundle new` writes when none is given: the publisher's
/// own terms, until they choose a license.
const LICENSE: &str = "LicenseRef-Proprietary";

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let subcommand = args.subcommand().map(|s| s.to_string_lossy().into_owned());
    let mut operands = Vec::new();
    let mut name = None;
    let mut publisher = None;
    let mut license = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Long(l) if l == "name" => name = Some(args.text()?),
            Arg::Long(l) if l == "publisher" => publisher = Some(args.text()?),
            Arg::Long(l) if l == "license" => license = Some(args.text()?),
            Arg::Long(l) if l == "help" => {
                println!("{USAGE}");
                return Ok(0);
            }
            Arg::Operand(operand) => operands.push(operand.to_string_lossy().into_owned()),
            _ => return Err(usage(USAGE.into())),
        }
    }
    let new_only = name.is_some() || publisher.is_some() || license.is_some();
    match (subcommand.as_deref(), operands.as_slice()) {
        (Some("new"), [id, rest @ ..]) if rest.len() <= 1 => {
            new(id, rest.first().map(String::as_str), name, publisher, license)
        }
        _ if new_only => Err(usage(USAGE.into())),
        (Some("check"), [] | [_]) => check(operands.first().map_or(".", String::as_str)),
        (Some("schema"), []) => {
            println!("{}", bundle::schema());
            Ok(0)
        }
        (Some("upgrade-manifest"), [] | [_]) => {
            upgrade(operands.first().map_or(".", String::as_str))
        }
        _ => Err(usage(USAGE.into())),
    }
}

/// The manifest `what` names: a folder's manifest.json, or a file.
fn manifest_path(what: &str) -> (PathBuf, Option<PathBuf>) {
    let path = Path::new(what);
    if path.is_dir() {
        (path.join(MANIFEST), Some(path.to_path_buf()))
    } else {
        (path.into(), None)
    }
}

/// Reads the manifest at `path`: its text, or why it cannot be read.
fn read(path: &Path) -> Result<String, Fail> {
    if path.extension().is_some_and(|e| e == "dkv") {
        return Err(fail(format!(
            "{}: a packed bundle; check the folder it was made from",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => fail(format!(
            "{}: not there; `bundle new ID` writes a manifest to start from",
            path.display()
        )),
        _ => util::failed(&path.display(), e),
    })?;
    String::from_utf8(bytes).map_err(|_| fail(format!("{}: it is not UTF-8 text", path.display())))
}

/// `bundle new ID [FOLDER]`: a folder for a new bundle, FOLDER or the
/// last part of its ID, with its manifest and a folder for its programs.
fn new(
    id: &str,
    folder: Option<&str>,
    name: Option<String>,
    publisher: Option<String>,
    license: Option<String>,
) -> Result<i32, Fail> {
    bundle::check_id(id).map_err(usage)?;
    let short = id.rsplit('.').next().unwrap_or(id);
    let folder = PathBuf::from(folder.unwrap_or(short));
    let name = name.unwrap_or_else(|| short.to_string());
    let publisher = publisher.unwrap_or_else(|| util::user_name(util::own_user()));
    let license = license.unwrap_or_else(|| LICENSE.into());
    bundle::spdx::check(&license).map_err(|why| usage(format!("--license: {why}")))?;
    let text = bundle::template(id, &name, &publisher, &license);
    // What it writes must read: a name or publisher it cannot hold is the
    // command line's mistake.
    if let Err(errors) = bundle::from_json(&text) {
        let why: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
        return Err(usage(why.join("; ")));
    }
    fs::create_dir_all(folder.join("bin")).map_err(|e| util::failed(&folder.display(), e))?;
    let path = folder.join(MANIFEST);
    let mut file =
        OpenOptions::new().write(true).create_new(true).mode(0o644).open(&path).map_err(
            |e| match e.kind() {
                io::ErrorKind::AlreadyExists => fail(format!(
                    "{} is there already; check it with `bundle check`",
                    path.display()
                )),
                _ => util::failed(&path.display(), e),
            },
        )?;
    file.write_all(text.as_bytes()).map_err(|e| util::failed(&path.display(), e))?;
    println!("wrote {}", path.display());
    println!(
        "put the program in {}, then check the bundle with `bundle check {}`",
        folder.join("bin").join(short).display(),
        folder.display()
    );
    if license == LICENSE {
        println!(
            "its license is {LICENSE}, your own terms: write an SPDX expression such as MIT in \
             \"license\" to choose another"
        );
    }
    Ok(0)
}

/// `bundle check [FOLDER | FILE]`: what is wrong with a manifest, each on
/// a line of its own as `FILE:LINE: why`; in a folder, also each program
/// or icon it names that the folder lacks.
fn check(what: &str) -> Result<i32, Fail> {
    let (path, folder) = manifest_path(what);
    let text = read(&path)?;
    let manifest = match bundle::from_json(&text) {
        Ok(manifest) => manifest,
        Err(errors) => {
            for error in &errors {
                match error.line {
                    Some(line) => eprintln!("{}:{line}: {}", path.display(), error.message),
                    None => eprintln!("{}: {}", path.display(), error.message),
                }
            }
            return Ok(1);
        }
    };
    let missing = folder.as_deref().map_or_default(|folder| missing_files(&manifest, folder));
    for why in &missing {
        eprintln!("{}: {why}", path.display());
    }
    for note in manifest.notes() {
        println!("{}: note: {note}", path.display());
    }
    if !missing.is_empty() {
        return Ok(1);
    }
    println!(
        "{}: ok: {} {}, manifest version {}",
        path.display(),
        manifest.id,
        manifest.version,
        bundle::VERSION
    );
    Ok(0)
}

/// What of the files `manifest` names `folder` lacks: each program, as
/// an executable file, and the icon.
fn missing_files(manifest: &Manifest, folder: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for (name, program) in manifest.programs.iter() {
        for (arch, path) in program.path.iter() {
            match fs::metadata(folder.join(&path.0)) {
                Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => {}
                // A WebAssembly module is not run as a program is.
                Ok(meta) if meta.is_file() && *arch == bundle::Arch::Wasm => {}
                Ok(meta) if meta.is_file() => missing.push(format!(
                    "program {name} for {arch}, {}, may not be run: `chmod +x` it",
                    path.0
                )),
                _ => missing.push(format!("program {name} for {arch} is not at {}", path.0)),
            }
        }
    }
    if let Some(icon) = &manifest.icon
        && !folder.join(&icon.0).is_file()
    {
        missing.push(format!("the icon is not at {}", icon.0));
    }
    missing
}

/// `bundle upgrade-manifest [FOLDER | FILE]`: brings a manifest of an
/// earlier version to this one. Version 1 is the first, so there is
/// nothing earlier to bring up yet: it says so, once the manifest reads.
fn upgrade(what: &str) -> Result<i32, Fail> {
    let (path, _) = manifest_path(what);
    let text = read(&path)?;
    match bundle::from_json(&text) {
        Ok(_) => {
            println!(
                "{}: manifest version {}, the newest this Dukovo reads: nothing to upgrade",
                path.display(),
                bundle::VERSION
            );
            Ok(0)
        }
        Err(errors) => {
            for error in &errors {
                eprintln!("{}: {error}", path.display());
            }
            Ok(1)
        }
    }
}
