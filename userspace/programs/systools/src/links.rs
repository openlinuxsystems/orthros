// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The links to disks and partitions by what is on them (ADR 0303), as
//! udev keeps Linux's in `/dev/disk`: `by-uuid` and `by-label` by their
//! filesystems', `by-partuuid` and `by-partlabel` by what their disks'
//! tables call them. They are in `/system/live/disk`, memory that goes at
//! shutdown, which the kernel's `/devices/disk` leads to; each leads to
//! its device, `/devices/vda1`. volumed(8) keeps them as disks come and
//! go, and rdisk(8), the mkfs tools and mkswap(8) once they have written.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;

use crate::{blkid, probe};

/// Where the links are.
pub const DIR: &str = "/system/live/disk";

/// The folders of links, and the tag of blkid(8)'s that names each link.
const FOLDERS: [(&str, &str); 4] = [
    ("by-label", "LABEL"),
    ("by-partlabel", "PARTLABEL"),
    ("by-partuuid", "PARTUUID"),
    ("by-uuid", "UUID"),
];

/// A name as udev writes it in a link's: ASCII letters and digits, the
/// characters `#+-.:=@_`, and any other character that is not ASCII stay;
/// every other byte is written `\xNN`, so that a label with a `/` or a
/// space makes one name.
pub fn encoded(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || "#+-.:=@_".contains(c) || !c.is_ascii() {
            out.push(c);
        } else {
            out.push_str(&format!("\\x{:02x}", c as u32));
        }
    }
    out
}

/// The links there should be: in each folder, each name and the device
/// it leads to. Where two devices have one name, the first, in the
/// kernel's order of disks and partitions, has it.
fn wanted(
    devices: &[(String, Vec<(&'static str, String)>)],
) -> BTreeMap<&'static str, BTreeMap<String, String>> {
    let mut wanted: BTreeMap<&'static str, BTreeMap<String, String>> = BTreeMap::new();
    for (folder, tag) in FOLDERS {
        let links = wanted.entry(folder).or_default();
        for (name, tags) in devices {
            for (_, value) in tags.iter().filter(|(t, _)| *t == tag) {
                let link = encoded(value);
                if !link.is_empty() && link != "." && link != ".." {
                    links.entry(link).or_insert_with(|| format!("/devices/{name}"));
                }
            }
        }
    }
    wanted
}

/// Makes the links what the disks and partitions there are now say,
/// taking away those of what went or changed.
///
/// # Errors
///
/// If `/system/live/disk` cannot be made or written: a program without
/// the right to write it leaves the links to those with it.
pub fn refresh() -> io::Result<()> {
    let devices: Vec<(String, Vec<(&'static str, String)>)> = probe::devices()?
        .into_iter()
        .map(|(name, _)| {
            let tags = blkid::tags(&name);
            (name, tags)
        })
        .collect();
    let wanted = wanted(&devices);
    let dir = Path::new(DIR);
    for (folder, links) in &wanted {
        let folder = dir.join(folder);
        fs::create_dir_all(&folder)?;
        let mut there = BTreeSet::new();
        for entry in fs::read_dir(&folder)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let target = fs::read_link(entry.path()).ok();
            let right =
                links.get(&name).is_some_and(|want| target.as_deref() == Some(Path::new(want)));
            if right {
                there.insert(name);
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        for (name, target) in links.iter().filter(|(name, _)| !there.contains(*name)) {
            let link = folder.join(name);
            match symlink(target, &link) {
                // Another tool refreshing at once made it first.
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if fs::read_link(&link).ok().as_deref() != Some(Path::new(target)) {
                        fs::remove_file(&link)?;
                        symlink(target, &link)?;
                    }
                }
                done => done?,
            }
        }
    }
    Ok(())
}

/// [`refresh`], for a tool that has just written a disk: a failure is not
/// the tool's, and says nothing.
pub fn refresh_quietly() {
    let _ = refresh();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_encoded_as_udev_does() {
        assert_eq!(encoded("DUKOVO"), "DUKOVO");
        assert_eq!(encoded("My Stick"), "My\\x20Stick");
        assert_eq!(encoded("a/b"), "a\\x2fb");
        assert_eq!(encoded("Daten-Ä"), "Daten-Ä");
        assert_eq!(encoded("1234-ABCD"), "1234-ABCD");
    }

    #[test]
    fn the_first_device_with_a_name_has_it() {
        let devices = vec![
            ("vda1".to_string(), vec![("UUID", "1234-ABCD".to_string()), ("LABEL", "EFI".into())]),
            (
                "vdb1".to_string(),
                vec![("LABEL", "EFI".to_string()), ("PARTLABEL", "Dukovo".into())],
            ),
            ("vdc".to_string(), vec![("LABEL", "..".to_string())]),
        ];
        let wanted = wanted(&devices);
        assert_eq!(wanted["by-uuid"]["1234-ABCD"], "/devices/vda1");
        assert_eq!(wanted["by-label"]["EFI"], "/devices/vda1");
        assert_eq!(wanted["by-label"].len(), 1);
        assert_eq!(wanted["by-partlabel"]["Dukovo"], "/devices/vdb1");
        assert!(wanted["by-partuuid"].is_empty());
    }
}
