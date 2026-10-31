// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reparse points and the links they make, as Microsoft's *[MS-FSCC]:
//! File System Control Codes* specifies them (section 2.1.2).
//!
//! A file or directory with a reparse point has a `$REPARSE_POINT`
//! attribute: a tag, which says what it is, and its data. Radix reads
//! three kinds as symbolic links: Windows' symbolic links, which point to
//! a path relative to the link or to an absolute one; junctions (mount
//! points), which point to an absolute path; and the links WSL makes,
//! which hold a POSIX path. A link ntfs-3g made on Linux is an Interix
//! one instead: a system file whose data is `IntxLNK\1` and the path.
//!
//! An absolute Windows path names a drive (`\??\C:\Users`), and a volume
//! does not know which drive it was. Radix reads such a path as one on
//! its own volume, so that the links Windows makes on its own drive
//! (`Documents and Settings` to `C:\Users`) lead where they did; the link
//! becomes a relative one to the volume's root ([`Target::Absolute`]).

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::record::{u16_at, u32_at};

/// The reparse tags Radix knows.
pub mod tag {
    /// A Windows symbolic link.
    pub const SYMLINK: u32 = 0xa000_000c;
    /// A junction: a directory that stands for another.
    pub const MOUNT_POINT: u32 = 0xa000_0003;
    /// A symbolic link WSL made.
    pub const LX_SYMLINK: u32 = 0xa000_001d;
    /// A file whose data Windows' deduplication moved to its chunk store.
    pub const DEDUP: u32 = 0x8000_0013;
}

/// A link's flag: its path is relative to the link's directory.
const RELATIVE: u32 = 1;

/// The start of an Interix link's data.
pub const INTERIX_LINK: &[u8; 8] = b"IntxLNK\x01";

/// Whether a file with reparse tag `tag` is a symbolic link.
pub fn is_link(tag: u32) -> bool {
    matches!(tag, tag::SYMLINK | tag::MOUNT_POINT | tag::LX_SYMLINK)
}

/// Where a link points.
pub enum Target {
    /// A path as it is, UTF-8, `/` between its names.
    Path(KVec<u8>),
    /// An absolute path on the volume, from its root, UTF-8, `/` between
    /// its names and none first: the link needs a `../` for each
    /// directory it is below the root in.
    Absolute(KVec<u8>),
}

/// Where the reparse point `value` (its tag, length and data) points, for
/// a link ([`is_link`]).
pub fn target(value: &[u8]) -> Result<Target> {
    if value.len() < 8 {
        return Err(Error::Corrupt);
    }
    let tag = u32_at(value, 0);
    let len = usize::from(u16_at(value, 4));
    let data = value.get(8..8 + len).ok_or(Error::Corrupt)?;
    match tag {
        tag::LX_SYMLINK => {
            // A version, 2, then the path.
            let path = data.get(4..).ok_or(Error::Corrupt)?;
            Ok(Target::Path(mem::copy(path)?))
        }
        tag::SYMLINK | tag::MOUNT_POINT => {
            let (buffer, relative) = if tag == tag::SYMLINK {
                let flags = u32_at(data.get(..12).ok_or(Error::Corrupt)?, 8);
                (&data[12..], flags & RELATIVE != 0)
            } else {
                (data.get(8..).ok_or(Error::Corrupt)?, false)
            };
            let at = usize::from(u16_at(data, 0));
            let n = usize::from(u16_at(data, 2));
            let name = buffer.get(at..at + n).ok_or(Error::Corrupt)?;
            let units = name.as_chunks::<2>().0.iter().map(|u| u16::from_le_bytes(*u));
            windows_path(units, relative)
        }
        _ => Err(Error::Corrupt),
    }
}

/// The path an Interix link's data `data` holds, after its start.
pub fn interix(data: &[u8]) -> Result<Target> {
    let units = data.get(INTERIX_LINK.len()..).ok_or(Error::Corrupt)?;
    let mut out = mem::vec();
    utf8_of(units.as_chunks::<2>().0.iter().map(|u| u16::from_le_bytes(*u)), &mut out, false)?;
    Ok(Target::Path(out))
}

/// The Windows path `units` as a target: relative, or absolute on a drive
/// or volume.
fn windows_path(units: impl Iterator<Item = u16> + Clone, relative: bool) -> Result<Target> {
    let mut out = mem::vec();
    if relative {
        utf8_of(units, &mut out, true)?;
        return Ok(Target::Path(out));
    }
    // `\??\C:\path`, or `\??\Volume{GUID}\path`: the path after the drive.
    let all: KVec<u16> = {
        let mut v = mem::vec();
        for unit in units {
            mem::push(&mut v, unit)?;
        }
        v
    };
    let mut rest = &all[..];
    for prefix in [r"\??\", r"\\?\"] {
        if starts_with(rest, prefix) {
            rest = &rest[prefix.len()..];
        }
    }
    let drive = rest.len() >= 2 && rest[1] == u16::from(b':');
    let volume = starts_with(rest, "Volume{");
    if !drive && !volume {
        // A server's share, or something else: as it is.
        utf8_of(all.iter().copied(), &mut out, true)?;
        return Ok(Target::Path(out));
    }
    let after = rest.iter().position(|&u| u == u16::from(b'\\')).map_or(rest.len(), |i| i + 1);
    utf8_of(rest[after..].iter().copied(), &mut out, true)?;
    while out.last() == Some(&b'/') {
        out.pop();
    }
    Ok(Target::Absolute(out))
}

/// Whether `units` starts with the ASCII `prefix`.
fn starts_with(units: &[u16], prefix: &str) -> bool {
    units.len() >= prefix.len() && units.iter().zip(prefix.bytes()).all(|(&u, b)| u == u16::from(b))
}

/// Appends `units` to `out` as UTF-8, `\` as `/` if `slashes`; units that
/// are not valid UTF-16 as U+FFFD.
fn utf8_of(units: impl Iterator<Item = u16>, out: &mut KVec<u8>, slashes: bool) -> Result<()> {
    for c in char::decode_utf16(units) {
        let c = match c.unwrap_or(char::REPLACEMENT_CHARACTER) {
            '\\' if slashes => '/',
            c => c,
        };
        let mut buf = [0u8; 4];
        mem::extend(out, c.encode_utf8(&mut buf).as_bytes())?;
    }
    Ok(())
}
