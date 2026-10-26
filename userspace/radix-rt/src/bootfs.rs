// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The boot archive's files, read-only, for Spark: the kernel maps the
//! archive into Spark's address space and says where in its start-up page
//! (radix-startup(2)). Paths are absolute, such as `/system/commands/spark`.

use core::fmt;

use libradix::startup;
use radix_bootfs::{Archive, Entry, Kind};

/// The boot archive, checked.
pub struct BootFs {
    archive: Archive<'static>,
    size: u64,
}

/// Why the archive could not be read.
#[derive(Clone, Copy, Debug)]
pub enum Error {
    /// The program was given no archive.
    Missing,
    /// The archive is damaged.
    Damaged(radix_bootfs::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Missing => f.write_str("no boot archive was given"),
            Error::Damaged(err) => write!(f, "the boot archive is damaged: {err}"),
        }
    }
}

/// A path as the archive names it: without the leading `/`.
fn inside(path: &str) -> &str {
    path.trim_start_matches('/')
}

impl BootFs {
    /// The archive the start-up page names, checked whole.
    pub fn mount() -> Result<BootFs, Error> {
        let start = startup();
        let (address, size) = (start.bootfs_address, start.bootfs_size);
        if address == 0 || size == 0 {
            return Err(Error::Missing);
        }
        // SAFETY: the kernel maps the archive read-only at this address,
        // this long, for the program's whole life (radix-startup(2)).
        let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, size as usize) };
        let archive = radix_bootfs::open(bytes).map_err(Error::Damaged)?;
        Ok(BootFs { archive, size })
    }

    /// The archive's size, in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// How many files it holds.
    pub fn files(&self) -> usize {
        self.archive.entries().filter(|entry| entry.kind == Kind::File).count()
    }

    /// The contents of the file at `path`, if there is one.
    pub fn read(&self, path: &str) -> Option<&'static [u8]> {
        self.archive.find(inside(path)).filter(|entry| entry.kind == Kind::File).map(|e| e.data)
    }

    /// What directory `dir` holds directly.
    pub fn list<'d>(&self, dir: &'d str) -> impl Iterator<Item = Entry<'static>> + use<'d> {
        self.archive.list(inside(dir))
    }
}
