// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! eject(1): unmounts a volume at `/volumes`, writing out what it holds
//! first, so that its disk can be pulled out (ADR 0163), as util-linux's
//! eject does with a mount point or a device. A disk's name unmounts each
//! of its partitions'.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::args::{Arg, Args};
use crate::df;
use crate::probe;
use crate::proc::Mount;
use crate::util::{self, Fail, usage};
use crate::volumed;

/// The volumes `what` names: a label or name under `/volumes`, a path
/// there, or a disk or partition by its name or its path in `/devices`.
fn named<'a>(mounts: &'a [Mount], what: &str) -> Vec<&'a Mount> {
    let device = what.strip_prefix("/devices/").unwrap_or(what);
    let path =
        if what.starts_with('/') { PathBuf::from(what) } else { Path::new("/volumes").join(what) };
    let by_path: Vec<&Mount> = mounts.iter().filter(|m| df::mount_path(m) == path).collect();
    if !by_path.is_empty() {
        return by_path;
    }
    mounts.iter().filter(|m| m.source == device || probe::disk_of(&m.source) == device).collect()
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut verbose, mut what) = (false, None::<String>);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('v') => verbose = true,
            Arg::Long(long) if long == "verbose" => verbose = true,
            Arg::Operand(operand) if what.is_none() => {
                what = Some(operand.to_string_lossy().into_owned());
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let Some(what) = what else { return Err(usage("no volume given".into())) };
    let mounts = volumed::volume_mounts().map_err(|e| util::failed(&"/processes/mounts", e))?;
    let chosen = named(&mounts, &what);
    if chosen.is_empty() {
        return Err(util::fail(format!("{what}: no volume at /volumes")));
    }
    let mut out = io::stdout().lock();
    for mount in chosen {
        let path = df::mount_path(mount);
        if verbose {
            writeln!(out, "eject: unmounting {} (/devices/{})", path.display(), mount.source)?;
        }
        volumed::unmount(&path)
            .map_err(|why| util::fail(format!("{}: cannot unmount: {why}", path.display())))?;
    }
    Ok(0)
}
