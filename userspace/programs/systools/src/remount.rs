// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! remount(8): mounts a volume at `/volumes` again, to write or only to
//! read, as `mount -o remount` does on Linux. An NTFS that Windows
//! hibernated with, or shut down with Fast Startup, is only read; with
//! `--discard-hibernation` it is written anyway, after a warning, as
//! ntfs-3g's `remove_hiberfile` does: Windows then starts afresh, without
//! what it left open, and checks the volume (ADR 0238).

use std::io::{self, Write};

use radix_abi::directory::MountFlags;

use crate::args::{Arg, Args};
use crate::df;
use crate::eject;
use crate::prompt;
use crate::util::{self, Fail, usage};
use crate::volumed;

/// What it is told before it throws a hibernation away.
const WARNING: &str = "Windows hibernated with this volume, or shut down with Fast Startup, \
which hibernates it too. Writing it throws the hibernation away: Windows then starts afresh, \
and what it had open, its programs and what it had not yet written to the volume, is lost. \
Windows checks the volume at its next start.";

/// The flags a mount is asked with: to write or only to read, and whether
/// a hibernation is thrown away.
pub fn flags_for(read_only: bool, discard: bool) -> MountFlags {
    let flags = if read_only { MountFlags::READ_ONLY } else { MountFlags::NONE };
    if discard { flags.union(MountFlags::DISCARD_HIBERNATION) } else { flags }
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut read_only, mut discard, mut yes, mut what) = (false, false, false, None::<String>);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('r') => read_only = true,
            Arg::Short('w') => read_only = false,
            Arg::Short('y') => yes = true,
            Arg::Long(long) if long == "read-only" => read_only = true,
            Arg::Long(long) if long == "write" => read_only = false,
            Arg::Long(long) if long == "yes" => yes = true,
            Arg::Long(long) if long == "discard-hibernation" => discard = true,
            Arg::Operand(operand) if what.is_none() => {
                what = Some(operand.to_string_lossy().into_owned());
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let Some(what) = what else { return Err(usage("no volume given".into())) };
    if discard && read_only {
        return Err(usage("--discard-hibernation writes the volume; --read-only does not".into()));
    }
    let mounts = volumed::volume_mounts().map_err(|e| util::failed(&"/processes/mounts", e))?;
    let chosen = eject::named(&mounts, &what);
    let mount = match chosen.as_slice() {
        [] => return Err(util::fail(format!("{what}: no volume at /volumes"))),
        [one] => *one,
        _ => return Err(util::fail(format!("{what}: more than one volume; name one"))),
    };
    let path = df::mount_path(mount);
    if discard && mount.kind != "ntfs" {
        return Err(util::fail(format!(
            "{}: only an NTFS holds a Windows hibernation",
            path.display()
        )));
    }
    if discard {
        let mut err = io::stderr().lock();
        writeln!(err, "remount: {}: {WARNING}", path.display())?;
        drop(err);
        if !yes && !prompt::confirm("Write it anyway?") {
            writeln!(io::stdout(), "remount: {} is left as it was", path.display())?;
            return Ok(1);
        }
    }
    let got = volumed::remount(mount, flags_for(read_only, discard))
        .map_err(|why| util::fail(format!("{}: cannot mount it again: {why}", path.display())))?;
    let mut out = io::stdout().lock();
    if got.contains(MountFlags::READ_ONLY) && !read_only {
        writeln!(
            out,
            "remount: {} is only read: its filesystem refuses writes; the kernel's log says why",
            path.display()
        )?;
        return Ok(1);
    }
    let how = if read_only { "only read" } else { "written" };
    writeln!(out, "remount: {} is {how} now", path.display())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_say_what_was_asked() {
        assert_eq!(flags_for(false, false), MountFlags::NONE);
        assert_eq!(flags_for(true, false), MountFlags::READ_ONLY);
        assert_eq!(flags_for(false, true), MountFlags::DISCARD_HIBERNATION);
    }
}
