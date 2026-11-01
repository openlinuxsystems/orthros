// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! volumed(8): mounts each volume plugged in after boot at
//! `/volumes/<label>`, as udisks does for a desktop's removable media,
//! and unmounts the volumes of a disk that goes (ADR 0163); eject(1)
//! unmounts one before its disk is pulled out.
//!
//! Spark mounts the disks there at boot, each at `/volumes/<name>`;
//! volumed takes those it finds at its start as done, and looks at
//! `/kernel/block` every second for disks and partitions that come or go.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

use radix_abi::directory::{self, MountFlags};
use radix_abi::errors::{File as FileError, Kernel};
use radix_abi::{Status, startup};

use crate::args::{Arg, Args};
use crate::df;
use crate::probe::{self, BLOCKS};
use crate::proc::{self, Mount};
use crate::sys::{self, SYS};
use crate::util::{self, Fail};

/// How often `/kernel/block` is looked at.
const PERIOD: Duration = Duration::from_secs(1);

/// The type of an EFI system partition, as `/kernel/block` gives it: in
/// a GPT, and in an MBR. Spark leaves these alone, and so does volumed.
const EFI_SYSTEM: [&str; 2] = ["c12a7328-f81f-11d2-ba4b-00a0c93ec93b", "0xef"];

/// The filesystems tried, in Spark's order: FAT, exFAT and NTFS before
/// the others, whose superblocks their `mkfs` can leave behind, and discs
/// last, UDF before the ISO 9660 a DVD has as a bridge to it.
const KINDS: [(&[u8], &str); 7] = [
    (b"fat", "FAT"),
    (b"exfat", "exFAT"),
    (b"ntfs", "NTFS"),
    (b"btrfs", "Btrfs"),
    (b"ext4", "ext4"),
    (b"udf", "UDF"),
    (b"iso9660", "ISO 9660"),
];

/// The disks and partitions there are now.
fn present() -> BTreeSet<String> {
    std::fs::read_dir(BLOCKS).map_or_else(
        |_| BTreeSet::new(),
        |entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
    )
}

/// Whether `name` is a volume: a partition, or a disk with none, that is
/// no EFI system partition.
fn is_volume(name: &str, present: &BTreeSet<String>) -> bool {
    let partitioned = probe::block_text(name, "partition").is_none()
        && present.iter().any(|other| other != name && probe::disk_of(other) == name);
    let efi = probe::block_text(name, "parttype").is_some_and(|kind| EFI_SYSTEM.contains(&&*kind));
    !partitioned && !efi
}

/// The name a volume is mounted at: its filesystem's label, made a single
/// file name, or the device's name for a volume with none; with the
/// device's name after a dash when another volume has the label.
pub fn place_for(label: Option<&str>, name: &str, taken: impl Fn(&str) -> bool) -> String {
    let label: String = label
        .unwrap_or_default()
        .trim()
        .chars()
        .map(|c| if c == '/' || c.is_control() { '_' } else { c })
        .collect();
    let label = match label.as_str() {
        "" | "." | ".." => return name.to_string(),
        _ => label,
    };
    if taken(&label) { format!("{label}-{name}") } else { label }
}

/// Opens `/devices/<name>` to write, or to read when it refuses writes.
fn open_device(name: &str) -> io::Result<(File, MountFlags)> {
    let path = format!("/devices/{name}");
    match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => Ok((file, MountFlags::NONE)),
        Err(err) if err.raw_os_error() == Some(libc::EROFS) => {
            Ok((File::open(&path)?, MountFlags::READ_ONLY))
        }
        Err(err) => Err(err),
    }
}

/// The directory to mount beneath at the absolute `path`, as a slot, and
/// `path` beneath it: a place Spark handed this program as a descriptor
/// from 3 on, as its service file's `files.mount` asks, which
/// `MOUNT_PLACES` names in order (ADR 0217), since nothing is mounted
/// through a view; or else the root, as for eject(1) in root's shell.
fn mount_base(path: &str) -> (u32, String) {
    let places = std::env::var("MOUNT_PLACES").unwrap_or_default();
    for (fd, place) in (3..).zip(places.split(':').filter(|p| !p.is_empty())) {
        if let Some(rest) = path.strip_prefix(place).and_then(|r| r.strip_prefix('/')) {
            return (sys::slot_of(fd), rest.to_string());
        }
    }
    (startup::SLOT_ROOT, path.trim_start_matches('/').to_string())
}

/// Mounts `device` at `/volumes/<at>` as a filesystem of `kind`, read-only
/// if it refuses writes or Radix does not write it, as Spark does.
fn mount_as(at: &str, kind: &[u8], device: &File, flags: MountFlags) -> Result<MountFlags, Status> {
    let (base, path) = mount_base(&format!("/volumes/{at}"));
    let source = sys::slot_of(device.as_raw_fd());
    match directory::mount(&SYS, base, path.as_bytes(), kind, source, flags) {
        Err(status)
            if !flags.contains(MountFlags::READ_ONLY)
                && (FileError::from_status(status) == Some(FileError::ReadOnly)
                    || Kernel::from_status(status) == Some(Kernel::InvalidArgument)) =>
        {
            let flags = MountFlags::READ_ONLY;
            directory::mount(&SYS, base, path.as_bytes(), kind, source, flags).map(|()| flags)
        }
        done => done.map(|()| flags),
    }
}

/// Mounts the volume `name`, which just came: a line of what it did.
fn mount(name: &str) -> String {
    let label = probe::reader(name).and_then(|mut read| probe::filesystem(&mut read)?.label);
    let at = place_for(label.as_deref(), name, |at| Path::new("/volumes").join(at).exists());
    let (device, flags) = match open_device(name) {
        Ok(opened) => opened,
        Err(err) => return format!("cannot open /devices/{name}: {}", util::strip_os_error(&err)),
    };
    if let Err(err) = std::fs::create_dir(format!("/volumes/{at}")) {
        return format!("cannot make /volumes/{at}: {}", util::strip_os_error(&err));
    }
    let mut last = None;
    for (kind, said) in KINDS {
        match mount_as(&at, kind, &device, flags) {
            Ok(flags) => {
                let ro = if flags.contains(MountFlags::READ_ONLY) { ", read-only" } else { "" };
                return format!("mounted the {said} filesystem of {name} at /volumes/{at}{ro}");
            }
            Err(status) if FileError::from_status(status) == Some(FileError::UnknownFilesystem) => {
            }
            Err(status) => {
                last = Some(status);
                break;
            }
        }
    }
    let _ = std::fs::remove_dir(format!("/volumes/{at}"));
    match last {
        None => format!(
            "{name} holds no FAT, exFAT, NTFS, Btrfs, ext4, UDF or ISO 9660 filesystem; left alone"
        ),
        Some(status) if FileError::from_status(status) == Some(FileError::InUse) => {
            format!("{name} is in use already; left alone")
        }
        Some(status) => {
            format!("cannot mount {name}: {}", util::strip_os_error(&sys::io_error(status)))
        }
    }
}

/// The mounts at `/volumes/<one name>`.
pub fn volume_mounts() -> io::Result<Vec<Mount>> {
    Ok(proc::mounts()?
        .into_iter()
        .filter(|m| {
            let path = df::mount_path(m);
            path.parent() == Some(Path::new("/volumes"))
        })
        .collect())
}

/// Unmounts the volume at `path` and removes its directory.
pub fn unmount(path: &Path) -> Result<(), String> {
    let (base, relative) = mount_base(&path.to_string_lossy());
    directory::unmount(&SYS, base, relative.as_bytes())
        .map_err(|status| util::strip_os_error(&sys::io_error(status)))?;
    std::fs::remove_dir(path).map_err(|err| util::strip_os_error(&err))
}

/// Mounts the volume `mount` at `/volumes` again, as `flags` ask (ADR
/// 0238): unmounted, then mounted from its device with its filesystem; one
/// that refuses writes is mounted read-only, as at its first mount. A
/// mount that fails is tried again as it was.
pub fn remount(mount: &Mount, flags: MountFlags) -> Result<MountFlags, String> {
    let path = df::mount_path(mount);
    let at = path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or("not a volume")?;
    let (device, device_flags) =
        open_device(&mount.source).map_err(|err| util::strip_os_error(&err))?;
    let flags = if device_flags.contains(MountFlags::READ_ONLY) {
        flags.union(MountFlags::READ_ONLY)
    } else {
        flags
    };
    unmount(&path)?;
    std::fs::create_dir(&path).map_err(|err| util::strip_os_error(&err))?;
    let kind = mount.kind.as_bytes();
    match mount_as(&at, kind, &device, flags) {
        Ok(flags) => Ok(flags),
        Err(status) => {
            let was = if mount.read_only { MountFlags::READ_ONLY } else { MountFlags::NONE };
            let _ = mount_as(&at, kind, &device, was);
            Err(util::strip_os_error(&sys::io_error(status)))
        }
    }
}

/// Unmounts what is mounted at `/volumes` from the disks and partitions
/// in `gone`: lines of what it did.
fn unmount_gone(gone: &BTreeSet<String>) -> Vec<String> {
    let Ok(mounts) = volume_mounts() else { return Vec::new() };
    let mut said = Vec::new();
    for mount in mounts.iter().filter(|m| gone.contains(&m.source)) {
        let path = df::mount_path(mount);
        said.push(match unmount(&path) {
            Ok(()) => format!("{} is gone; unmounted {}", mount.source, path.display()),
            Err(why) => {
                format!("{} is gone; cannot unmount {}: {why}", mount.source, path.display())
            }
        });
    }
    said
}

/// Watches `/kernel/block` for good.
fn watch() -> Result<i32, Fail> {
    let mut known = present();
    let mut out = io::stdout().lock();
    writeln!(out, "watching for volumes; {} disks and partitions now", known.len())?;
    loop {
        std::thread::sleep(PERIOD);
        let now = present();
        let gone: BTreeSet<String> = known.difference(&now).cloned().collect();
        for line in unmount_gone(&gone) {
            writeln!(out, "{line}")?;
        }
        for name in now.difference(&known).filter(|name| is_volume(name, &now)) {
            writeln!(out, "{}", mount(name))?;
        }
        known = now;
    }
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut once = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Long(long) if long == "list" => once = true,
            other => return Err(args.unknown(&other)),
        }
    }
    if !once {
        return watch();
    }
    let mounts = volume_mounts().map_err(|e| util::failed(&"/processes/mounts", e))?;
    let mut out = io::stdout().lock();
    for mount in &mounts {
        let ro = if mount.read_only { " (read-only)" } else { "" };
        let path = df::mount_path(mount);
        writeln!(out, "{}  /devices/{}  {}{ro}", path.display(), mount.source, mount.kind)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_come_from_labels() {
        let free = |_: &str| false;
        assert_eq!(place_for(Some("DUKOVO"), "sdb1", free), "DUKOVO");
        assert_eq!(place_for(Some("  My Stick "), "sdb1", free), "My Stick");
        assert_eq!(place_for(Some("a/b"), "sdb1", free), "a_b");
        assert_eq!(place_for(None, "sdb1", free), "sdb1");
        assert_eq!(place_for(Some(".."), "sdb1", free), "sdb1");
        assert_eq!(place_for(Some("DUKOVO"), "sdc", |at| at == "DUKOVO"), "DUKOVO-sdc");
    }
}
