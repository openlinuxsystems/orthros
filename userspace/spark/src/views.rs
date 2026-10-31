// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a service holds (ADR 0217): its root is a view (ADR 0215) that
//! Spark builds as it starts the service, of exactly what its file
//! declares and what every service gets without asking.
//!
//! The view holds, each at its real path: the places [`ALWAYS`] lists,
//! read-only but for the devices; the places the file names under
//! `files`, read-only or not as it says, each of the second kind made if
//! it is not there yet; the service's program; the folder of its
//! secrets, read-only; a `/tmp` of its own, which is
//! `/system/live/spark/tmp/NAME`; and the links of the root and of
//! `/system`, so that `/bin/sh` and `/usr/bin/env` lead where they do. The
//! service acts for root without the bypass right, so the owners and modes
//! of its files narrow it too, and it reaches nothing past its view. A
//! place `files.mount` names is handed to it as a descriptor as well, from
//! 3 on, with the control right that mounting needs and a view never
//! hands on; `MOUNT_PLACES` names them, in that order.
//!
//! A file with `runs-with-your-access` gets Spark's own root instead, the
//! whole tree with the bypass right, as consoled needs to start root's
//! shells until sign-in (13b.5).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libradix::abi::capability::Rights;
use libradix::abi::directory::FileKind;
use libradix::abi::errors::File as FileError;
use manifest::service::{self, ALWAYS, Place};
use radix_rt::fs::{Dir, File};

use crate::services::{self, Spec};

/// Where the services' own `/tmp` folders are.
pub const TMP: &str = "system/live/spark/tmp";

/// The folder whose links a view copies with the root's (`/bin`,
/// `/etc`): `/system`'s are `/system/bin` and the like, where `/usr/bin`
/// leads.
const SYSTEM: &str = "system";

/// The rights of what a service gets: to read and change, and to copy
/// and pass on, as `fork` and `exec` need.
const GIVEN: Rights = Rights::READ.union(Rights::WRITE).union(Rights::COPY).union(Rights::TRANSFER);

/// A service's root and what it mounts beneath.
pub struct View {
    /// Its root: the view, or Spark's root for a service that runs with
    /// Spark's access.
    pub root: Root,
    /// The places `files.mount` names, with the control right, in order.
    pub mounts: Vec<Dir>,
}

/// A service's root.
pub enum Root {
    /// A view of its own.
    View(Dir),
    /// Spark's root.
    Spark,
}

/// Builds service `spec`'s view beneath Spark's `root`, or says why not.
pub fn build(root: &Dir, spec: &Spec) -> Result<View, String> {
    if spec.whole_tree {
        return Ok(View { root: Root::Spark, mounts: Vec::new() });
    }
    // Without the bypass right: the view, and each place attached to it,
    // acts for root as the owners and modes of its files let it.
    let base = root
        .derive(GIVEN.union(Rights::CONTROL))
        .map_err(|err| format!("cannot narrow the root: {err}"))?;
    let view = base.make_view().map_err(|err| format!("cannot make its view: {err}"))?;
    let mut attached: Vec<String> = Vec::new();

    let always = ALWAYS
        .iter()
        .filter(|(path, _)| !spec.places.iter().any(|p| within(path, &p.path)))
        .map(|&(path, write)| (Place { path: path.into(), write }, true));
    let declared = spec.places.iter().map(|place| (place.clone(), false));
    for (place, optional) in always.chain(declared) {
        let path = &place.path[1..];
        // A place the file lets it change is made if it is not there, as
        // systemd's StateDirectory= is; so is one of /system/live, Spark's
        // folder for what goes at shutdown, as notifyd's is for those that
        // start before notifyd.
        if (place.write && !optional) || within(path, "system/live") {
            services::make_dirs(root, path)?;
        }
        match root.stat(path) {
            Ok(_) => {}
            // A place every service gets that this system lacks: /kernel
            // without its files, say.
            Err(_) if optional => continue,
            Err(err) => return Err(format!("{}: {err}", place.path)),
        }
        attach(&base, &view, path, place.write)?;
        attached.push(place.path);
    }

    // Its program, unless a place holds it already.
    if !attached.iter().any(|path| within(&spec.program, path)) {
        attach(&base, &view, &spec.program[1..], false)?;
    }

    if !spec.secrets.is_empty() {
        let dir = service::secrets_dir(&spec.name);
        for secret in &spec.secrets {
            if root.stat(&format!("{}/{secret}", &dir[1..])).is_err() {
                return Err(format!(
                    "its secret {secret} is not set: set it with `service secret set {} {secret}`",
                    spec.name
                ));
            }
        }
        attach(&base, &view, &dir[1..], false)?;
    }

    let tmp = format!("{TMP}/{}", spec.name);
    services::make_dirs(root, &tmp)?;
    let opened = base.open_dir(&tmp, GIVEN).map_err(|err| format!("/{tmp}: {err}"))?;
    view.attach("tmp", opened.slot()).map_err(|err| format!("cannot attach /tmp: {err}"))?;

    copy_links(root, &view, "");
    if let Ok(system) = root.open_dir(SYSTEM, Rights::READ) {
        copy_links(&system, &view, SYSTEM);
    }

    let mut mounts = Vec::new();
    for path in &spec.mounts {
        let place = base
            .open_dir(&path[1..], GIVEN.union(Rights::CONTROL))
            .map_err(|err| format!("{path}: {err}"))?;
        mounts.push(place);
    }
    // Without the control right: nothing more can be attached to it.
    let given = view.derive(GIVEN).map_err(|err| format!("cannot narrow its view: {err}"))?;
    Ok(View { root: Root::View(given), mounts })
}

/// Whether `inner` is `outer` or beneath it.
fn within(inner: &str, outer: &str) -> bool {
    inner.strip_prefix(outer).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Attaches the directory or file at `path` beneath `base` at the same
/// path in `view`, read-only unless `write`.
fn attach(base: &Dir, view: &Dir, path: &str, write: bool) -> Result<(), String> {
    let rights =
        if write { GIVEN } else { Rights::READ.union(Rights::COPY).union(Rights::TRANSFER) };
    let slot = match base.open_dir(path, rights) {
        Ok(dir) => Held::Dir(dir),
        Err(err) if err.is(FileError::NotDirectory) => {
            let file = base
                .open(path, libradix::abi::directory::OpenFlags::NONE, rights)
                .map_err(|err| format!("/{path}: {err}"))?;
            Held::File(file)
        }
        Err(err) => return Err(format!("/{path}: {err}")),
    };
    view.attach(path, slot.slot()).map_err(|err| format!("cannot attach /{path}: {err}"))
}

/// A directory or a file, held while it is attached.
enum Held {
    Dir(Dir),
    File(File),
}

impl Held {
    fn slot(&self) -> u32 {
        match self {
            Held::Dir(dir) => dir.slot(),
            Held::File(file) => file.slot(),
        }
    }
}

/// Copies the symbolic links of the directory `at`, which is `dir` of the
/// root, into the view, where nothing has their names yet.
fn copy_links(at: &Dir, view: &Dir, dir: &str) {
    let Ok(entries) = at.entries() else { return };
    for entry in entries.iter().filter(|e| e.kind == FileKind::Symlink) {
        let Ok(target) = at.read_link(&entry.name) else { continue };
        let Ok(target) = String::from_utf8(target) else { continue };
        let path =
            if dir.is_empty() { entry.name.clone() } else { format!("{dir}/{}", entry.name) };
        // A name the view has already keeps what it has, as /system/bin
        // where a file names /system whole.
        let _ = view.attach_link(&path, &target);
    }
}
