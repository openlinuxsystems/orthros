// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Service files (service.toml(5), ADR 0216): what Spark starts at boot
//! and keeps running.
//!
//! Each `NAME.toml` file in `/system/settings/spark/services` describes
//! one service, in TOML, read by the manifest crate as `service check`
//! reads it. The system's own come from
//! `/system/resources/default-settings/spark/services`, which Spark copies
//! in when they are missing, so an administrator turns one off with
//! `enabled = false` rather than by removing it.
//!
//! Spark reads a service's file again each time it starts the service.
//! A file that does not read never takes a service down: Spark says which
//! line is wrong and why, and runs the last copy that read, which it keeps
//! in `/system/state/spark/good`. An old file of Phases 9 to 12,
//! `NAME.service` with lines of `key = value` (ADR 0101), is converted to
//! `NAME.toml` with its comments, and kept as `NAME.service.old`.
//!
//! A service reaches the network only as its file says (ADR 0183):
//! `network.outbound = true` lets it connect out, and `network.listen`
//! names the ports it may listen on. `admin.hardware = true` gives it the
//! platform capability, which sets the fans, the LEDs and the ARGB header
//! (ADR 0208). It reaches the tree only through the view Spark builds of
//! what its file declares (`views`, ADR 0217).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libradix::abi::capability::Rights;
use libradix::abi::errors::File as FileError;
use libradix::abi::resource_domain::OomClass;
use manifest::service::{self, Oom, Place};
use radix_rt::fs::Dir;
use radix_rt::println;

pub use manifest::service::{Restart, check_name};

/// Where Spark keeps the last copy of each service file that read.
pub const GOOD: &str = "system/state/spark/good";

/// One service file, as Spark runs it.
#[derive(Clone, Debug)]
pub struct Spec {
    /// The file's name without `.toml`: the service's and its domain's.
    pub name: String,
    pub description: String,
    /// The program's absolute path.
    pub program: String,
    pub arguments: Vec<String>,
    /// Lower starts first; shutdown stops the highest first.
    pub order: u32,
    pub restart: Restart,
    /// The CPU weight of its domain.
    pub weight: u32,
    /// The memory limit of its domain, in pages.
    pub memory_pages: Option<u64>,
    /// Its domain's out-of-memory class: service or app.
    pub oom: OomClass,
    /// More of the environment, as `NAME=value`.
    pub environment: Vec<String>,
    pub enabled: bool,
    /// Whether it may connect out, to any address and port.
    pub outbound: bool,
    /// The ranges of ports it may listen on, as (first, count).
    pub listen: Vec<(u32, u32)>,
    /// Whether it gets the platform capability: admin.hardware.
    pub hardware: bool,
    /// The places its view holds beyond what every service's does.
    pub places: Vec<Place>,
    /// The places it mounts filesystems beneath, which it gets as
    /// descriptors too.
    pub mounts: Vec<String>,
    /// Whether it runs with Spark's root, the whole tree, in place of a
    /// view.
    pub whole_tree: bool,
    /// Whether it gets the names capability (ADR 0218).
    pub names: bool,
    /// The names of its secrets.
    pub secrets: Vec<String>,
}

impl Spec {
    /// How many network capabilities it gets: one to connect out, and one
    /// for each range of ports it listens on.
    pub fn network_grants(&self) -> usize {
        usize::from(self.outbound) + self.listen.len()
    }
}

/// Reads the service file `name`.toml with `text`.
pub fn parse(name: &str, text: &str) -> Result<Spec, String> {
    check_name(name)?;
    let file = service::from_toml(text).map_err(|err| err.to_string())?;
    let network = &file.permissions.network;
    Ok(Spec {
        name: name.to_string(),
        description: file.description,
        program: file.program,
        arguments: file.arguments,
        order: file.order,
        restart: file.restart,
        weight: file.limits.cpu_weight,
        memory_pages: file.limits.memory.map(|bytes| bytes.div_ceil(4096)),
        oom: match file.limits.oom {
            Oom::Service => OomClass::Service,
            Oom::App => OomClass::App,
        },
        environment: file
            .environment
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect(),
        enabled: file.enabled,
        outbound: network.outbound,
        listen: network.listen.iter().map(|p| (u32::from(p.first), p.count())).collect(),
        hardware: file.permissions.admin.hardware,
        places: file.permissions.places(),
        mounts: file.permissions.files.mount.clone(),
        whole_tree: file.permissions.runs_with_your_access,
        names: file.permissions.names,
        secrets: file.secrets,
    })
}

/// The read and write rights.
const READ_WRITE: Rights = Rights::READ.union(Rights::WRITE);

/// Converts each old `NAME.service` in `dir` that has no `NAME.toml` to
/// one, keeping the old file as `NAME.service.old`; returns how many.
pub fn convert_old(root: &Dir, dir: &str) -> usize {
    let Ok(at) = root.open_dir(dir, READ_WRITE) else { return 0 };
    let Ok(entries) = at.entries() else { return 0 };
    let mut converted = 0;
    for entry in entries {
        let Some(name) = entry.name.strip_suffix(".service") else { continue };
        if at.stat(&format!("{name}.toml")).is_ok() {
            continue;
        }
        match convert(&at, name) {
            Ok(()) => {
                println!(
                    "spark: converted /{dir}/{name}.service to {name}.toml; the old file is \
                     {name}.service.old"
                );
                converted += 1;
            }
            Err(err) => println!("spark: cannot convert /{dir}/{name}.service: {err}"),
        }
    }
    converted
}

/// Converts `name`.service in `dir` to `name`.toml.
fn convert(dir: &Dir, name: &str) -> Result<(), String> {
    let old = format!("{name}.service");
    let text = dir.read_file(&old).map_err(|err| format!("{err}"))?;
    let text = String::from_utf8(text).map_err(|_| String::from("it is not UTF-8 text"))?;
    let converted = manifest::legacy::convert(&text).map_err(|err| err.to_string())?;
    write_new(dir, &format!("{name}.toml"), &converted)?;
    dir.rename(&old, dir, &format!("{old}.old")).map_err(|err| format!("{err}"))
}

/// Writes `text` to `path` in `dir` whole or not at all: to a new file
/// first, then renamed over it.
fn write_new(dir: &Dir, path: &str, text: &str) -> Result<(), String> {
    let new = format!("{path}.new");
    dir.write_file(&new, text.as_bytes()).and_then(|()| dir.rename(&new, dir, path)).map_err(
        |err| {
            let _ = dir.remove(&new);
            format!("cannot write {path}: {err}")
        },
    )
}

/// Reads service `name`'s file in `dir`: what it says and its text.
pub fn read(root: &Dir, dir: &str, name: &str) -> Result<(Spec, String), String> {
    // A name is never a path.
    check_name(name)?;
    let path = format!("{dir}/{name}.toml");
    let text = root.read_file(&path).map_err(|err| format!("/{path}: {err}"))?;
    let text = String::from_utf8(text).map_err(|_| format!("/{path}: it is not UTF-8 text"))?;
    let spec = parse(name, &text).map_err(|err| format!("/{path}: {err}"))?;
    Ok((spec, text))
}

/// The last copy of service `name`'s file that read, if Spark kept one.
pub fn good(root: &Dir, name: &str) -> Option<(Spec, String)> {
    read(root, GOOD, name).ok()
}

/// Keeps `text` as the last good copy of service `name`'s file.
pub fn keep_good(root: &Dir, name: &str, text: &str) {
    let kept = make_dirs(root, GOOD)
        .and_then(|()| root.open_dir(GOOD, READ_WRITE).map_err(|err| format!("{err}")))
        .and_then(|dir| write_new(&dir, &format!("{name}.toml"), text));
    if let Err(err) = kept {
        println!("spark: cannot keep a good copy of {name}'s file in /{GOOD}: {err}");
    }
}

/// Makes `path` beneath `root` and each directory on the way.
pub fn make_dirs(root: &Dir, path: &str) -> Result<(), String> {
    let mut at = String::new();
    for part in path.split('/') {
        if !at.is_empty() {
            at.push('/');
        }
        at.push_str(part);
        // One that is there may be on a read-only filesystem, as
        // /system is, where making it fails as read-only.
        if root.stat(&at).is_ok() {
            continue;
        }
        match root.make_dir(&at) {
            Err(err) if !err.is(FileError::Exists) => return Err(format!("/{at}: {err}")),
            _ => {}
        }
    }
    Ok(())
}
