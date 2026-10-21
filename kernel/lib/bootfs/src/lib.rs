// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The boot archive: the files a machine starts with, before it has a file
//! system of its own.
//!
//! `ob` writes the archive as a POSIX ustar file (the `pax`/`tar`
//! interchange format of POSIX.1-1988, which every `tar` reads), and the
//! boot loader loads it as a module beside the kernel. The kernel finds
//! Spark in it, and Spark serves its files as a read-only file system.
//!
//! [`open`] checks the whole archive once before anything uses it, because
//! it is data from outside: every header's checksum, every number, every
//! size inside the file, every path relative and made of names other than
//! `.` and `..`, and the paths in order, each once (so checking takes one
//! pass, and a name cannot mean two files). After that, looking a file up
//! or listing a directory cannot fail. Any input gives a result or an error, never a
//! panic: the property tests feed it garbage. Only regular files and
//! directories are allowed; links and devices are refused.
//!
//! With the `write` feature, [`write::Writer`] makes archives, for `ob`
//! and the tests.

#![cfg_attr(not(any(test, feature = "write")), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

#[cfg(test)]
mod tests;
#[cfg(any(test, feature = "write"))]
pub mod write;

/// The size of a header, and of the blocks file data is padded to.
pub const BLOCK: usize = 512;

/// The most entries an archive may hold.
pub const MAX_ENTRIES: usize = 4096;

/// The longest path, prefix and name together.
pub const MAX_PATH: usize = 255;

const NAME: (usize, usize) = (0, 100);
const MODE: (usize, usize) = (100, 8);
const SIZE: (usize, usize) = (124, 12);
const MTIME: (usize, usize) = (136, 12);
const CHECKSUM: (usize, usize) = (148, 8);
const TYPE: usize = 156;
const MAGIC: (usize, usize) = (257, 6);
const PREFIX: (usize, usize) = (345, 155);

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
}

/// A file or directory in the archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    path: Path<'a>,
    /// What it is.
    pub kind: Kind,
    /// Its permission bits.
    pub mode: u32,
    /// When it was last changed, in seconds since 1970.
    pub mtime: u64,
    /// Its contents: empty for a directory.
    pub data: &'a [u8],
}

impl<'a> Entry<'a> {
    /// Its path, relative to the archive's root.
    pub fn path(&self) -> Path<'a> {
        self.path
    }
}

/// A path in the archive: the header's prefix and name, joined by `/`
/// when both are there, without a leading `./` or a trailing `/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Path<'a> {
    prefix: &'a str,
    name: &'a str,
}

impl<'a> Path<'a> {
    /// Its names, from the root down.
    pub fn components(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        // Checked by `open`: "." only comes first, as `./`.
        let named = |c: &&str| !c.is_empty() && *c != ".";
        self.prefix.split('/').chain(self.name.split('/')).filter(named)
    }

    /// Its last name: the file's own.
    pub fn file_name(&self) -> &'a str {
        self.components().last().unwrap_or("")
    }

    /// Whether it is `path` (relative, `/`-separated, with or without a
    /// leading `/`).
    pub fn is(&self, path: &str) -> bool {
        self.components().eq(path.split('/').filter(|c| !c.is_empty()))
    }

    /// Whether it is directly inside the directory `dir` (`""` or `"/"`
    /// for the root).
    pub fn is_in(&self, dir: &str) -> bool {
        let mut mine = self.components();
        for component in dir.split('/').filter(|c| !c.is_empty()) {
            if mine.next() != Some(component) {
                return false;
            }
        }
        mine.next().is_some() && mine.next().is_none()
    }
}

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, component) in self.components().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            f.write_str(component)?;
        }
        Ok(())
    }
}

/// Why an archive was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A header or a file's data runs past the end.
    Truncated,
    /// A header's checksum is wrong.
    Checksum,
    /// A header is not a ustar header.
    NotUstar,
    /// A number field is not octal, or too large.
    BadNumber,
    /// A path is empty, absolute, too long, not UTF-8, or has `.` or `..`
    /// in it.
    BadPath,
    /// The paths are not in order, or one is there twice.
    Order,
    /// An entry is something other than a file or a directory.
    Unsupported,
    /// More than [`MAX_ENTRIES`] entries.
    TooMany,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Truncated => "a header or a file's data runs past the end of the archive",
            Error::Checksum => "a header's checksum is wrong",
            Error::NotUstar => "a header is not a ustar header",
            Error::BadNumber => "a number in a header is not octal or is too large",
            Error::BadPath => "a path is empty, absolute, too long, not UTF-8 or has . or .. in it",
            Error::Order => "the paths are not in order, or one is there twice",
            Error::Unsupported => "an entry is a link or a device, not a file or a directory",
            Error::TooMany => "the archive has too many entries",
        })
    }
}

/// A checked archive.
#[derive(Clone, Copy, Debug)]
pub struct Archive<'a> {
    bytes: &'a [u8],
    count: usize,
}

/// Checks the archive in `bytes`.
pub fn open(bytes: &[u8]) -> Result<Archive<'_>, Error> {
    let archive = Archive { bytes, count: 0 };
    let mut count = 0;
    let mut at = 0;
    let mut last: Option<Path<'_>> = None;
    while let Some((entry, next)) = archive.entry_at(at)? {
        count += 1;
        if count > MAX_ENTRIES {
            return Err(Error::TooMany);
        }
        // In order of their names, a directory before what is in it, so
        // each path is there once.
        if last.is_some_and(|last| !last.components().lt(entry.path.components())) {
            return Err(Error::Order);
        }
        last = Some(entry.path);
        at = next;
    }
    Ok(Archive { bytes, count })
}

impl<'a> Archive<'a> {
    /// How many entries it has.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether it has no entries.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Its entries, in the order they are in the file.
    pub fn entries(&self) -> impl Iterator<Item = Entry<'a>> + use<'a> {
        let archive = *self;
        let mut at = 0;
        core::iter::from_fn(move || {
            // Checked by `open`: every header up to the end is good.
            let (entry, next) = archive.entry_at(at).ok()??;
            at = next;
            Some(entry)
        })
    }

    /// The entry at `path`, if there is one.
    pub fn find(&self, path: &str) -> Option<Entry<'a>> {
        self.entries().find(|entry| entry.path.is(path))
    }

    /// The entries directly inside the directory `dir`.
    pub fn list<'d>(&self, dir: &'d str) -> impl Iterator<Item = Entry<'a>> + use<'a, 'd> {
        self.entries().filter(move |entry| entry.path.is_in(dir))
    }

    /// The entry whose header is at `at`, and where the next header is;
    /// `None` at the end, which is two blocks of zeros or the end of the
    /// file.
    fn entry_at(&self, at: usize) -> Result<Option<(Entry<'a>, usize)>, Error> {
        if at == self.bytes.len() {
            return Ok(None);
        }
        let header = self.bytes.get(at..at + BLOCK).ok_or(Error::Truncated)?;
        if header.iter().all(|&b| b == 0) {
            return Ok(None);
        }
        let stored = number(field(header, CHECKSUM))?;
        let sum: u64 = header
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                let in_checksum = (CHECKSUM.0..CHECKSUM.0 + CHECKSUM.1).contains(&i);
                u64::from(if in_checksum { b' ' } else { b })
            })
            .sum();
        if stored != sum {
            return Err(Error::Checksum);
        }
        // "ustar\0" (POSIX) or "ustar " (old GNU tar).
        if &field(header, MAGIC)[..5] != b"ustar" {
            return Err(Error::NotUstar);
        }
        let kind = match header[TYPE] {
            b'0' | 0 => Kind::File,
            b'5' => Kind::Directory,
            _ => return Err(Error::Unsupported),
        };
        let size = number(field(header, SIZE))?;
        let mode = number(field(header, MODE))?;
        let mtime = number(field(header, MTIME))?;
        let path = Path { prefix: text(field(header, PREFIX))?, name: text(field(header, NAME))? };
        check_path(&path)?;
        let size = usize::try_from(size).map_err(|_| Error::Truncated)?;
        let start = at + BLOCK;
        let data = match kind {
            Kind::File => {
                let end = start.checked_add(size).ok_or(Error::Truncated)?;
                self.bytes.get(start..end).ok_or(Error::Truncated)?
            }
            Kind::Directory if size == 0 => &[],
            Kind::Directory => return Err(Error::BadNumber),
        };
        let next = start + size.div_ceil(BLOCK) * BLOCK;
        if next > self.bytes.len() {
            return Err(Error::Truncated);
        }
        let mode = u32::try_from(mode & 0o7777).map_err(|_| Error::BadNumber)?;
        Ok(Some((Entry { path, kind, mode, mtime, data }, next)))
    }
}

/// The bytes of header field `(offset, len)`.
fn field(header: &[u8], (offset, len): (usize, usize)) -> &[u8] {
    &header[offset..offset + len]
}

/// A NUL-terminated (or full) text field.
fn text(bytes: &[u8]) -> Result<&str, Error> {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..len]).map_err(|_| Error::BadPath)
}

/// An octal number field: optional spaces, at least one digit, then
/// spaces or NULs.
fn number(bytes: &[u8]) -> Result<u64, Error> {
    let start = bytes.iter().position(|&b| b != b' ').unwrap_or(bytes.len());
    let digits = bytes[start..].iter().take_while(|b| (b'0'..=b'7').contains(*b)).count();
    let rest = &bytes[start + digits..];
    if digits == 0 || rest.iter().any(|&b| b != b' ' && b != 0) {
        return Err(Error::BadNumber);
    }
    bytes[start..start + digits]
        .iter()
        .try_fold(0u64, |value, &digit| value.checked_mul(8)?.checked_add(u64::from(digit - b'0')))
        .ok_or(Error::BadNumber)
}

/// Paths are relative, short enough, and named: no empty path, no `.` or
/// `..` (a leading `./` is allowed, as `tar` writes it).
fn check_path(path: &Path<'_>) -> Result<(), Error> {
    if path.prefix.len() + 1 + path.name.len() > MAX_PATH + 1
        || path.prefix.starts_with('/')
        || (path.prefix.is_empty() && path.name.starts_with('/'))
    {
        return Err(Error::BadPath);
    }
    let pieces = path.prefix.split('/').chain(path.name.split('/'));
    let mut named = false;
    for (i, piece) in pieces.filter(|piece| !piece.is_empty()).enumerate() {
        match piece {
            "." if i == 0 => {}
            "." | ".." => return Err(Error::BadPath),
            _ => named = true,
        }
    }
    if !named {
        return Err(Error::BadPath);
    }
    Ok(())
}
