// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Writing boot archives: ustar files that [`crate::open`] accepts, and
//! that are the same bytes every time for the same files, so images can
//! be reproduced (owner 0, no user or group names, the times given).

use super::{BLOCK, CHECKSUM, Error, MAX_PATH, MODE, MTIME, NAME, PREFIX, SIZE, TYPE};

/// An archive being written. Entries go in in order of their paths, a
/// directory before what is in it.
#[derive(Default)]
pub struct Writer {
    bytes: Vec<u8>,
    last: Option<String>,
}

impl Writer {
    /// An empty archive.
    pub fn new() -> Writer {
        Writer::default()
    }

    /// Adds a directory.
    pub fn directory(&mut self, path: &str, mode: u32, mtime: u64) -> Result<(), Error> {
        self.entry(path, b'5', mode, mtime, &[])
    }

    /// Adds a regular file.
    pub fn file(&mut self, path: &str, mode: u32, mtime: u64, data: &[u8]) -> Result<(), Error> {
        self.entry(path, b'0', mode, mtime, data)
    }

    /// The archive's bytes, with the two blocks of zeros that end it.
    pub fn finish(mut self) -> Vec<u8> {
        self.bytes.resize(self.bytes.len() + 2 * BLOCK, 0);
        self.bytes
    }

    fn entry(
        &mut self,
        path: &str,
        kind: u8,
        mode: u32,
        mtime: u64,
        data: &[u8],
    ) -> Result<(), Error> {
        let path = path.trim_matches('/');
        let named = |c: &str| !c.is_empty() && c != "." && c != "..";
        if path.is_empty() || path.len() > MAX_PATH || !path.split('/').all(named) {
            return Err(Error::BadPath);
        }
        let parts = |p: &str| p.split('/').map(str::to_owned).collect::<Vec<_>>();
        if self.last.as_deref().is_some_and(|last| parts(last) >= parts(path)) {
            return Err(Error::Order);
        }
        // A name of up to 100 bytes, and what does not fit in the prefix,
        // split at a `/`.
        let (prefix, name) = if path.len() <= NAME.1 {
            ("", path)
        } else {
            let split = path
                .char_indices()
                .filter(|&(i, c)| c == '/' && i <= PREFIX.1 && path.len() - i - 1 <= NAME.1)
                .map(|(i, _)| i)
                .next()
                .ok_or(Error::BadPath)?;
            (&path[..split], &path[split + 1..])
        };
        let mut header = [0u8; BLOCK];
        header[NAME.0..NAME.0 + name.len()].copy_from_slice(name.as_bytes());
        octal(&mut header, MODE, u64::from(mode & 0o7777))?;
        octal(&mut header, (108, 8), 0)?;
        octal(&mut header, (116, 8), 0)?;
        octal(&mut header, SIZE, data.len() as u64)?;
        octal(&mut header, MTIME, mtime)?;
        header[TYPE] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[PREFIX.0..PREFIX.0 + prefix.len()].copy_from_slice(prefix.as_bytes());
        header[CHECKSUM.0..CHECKSUM.0 + CHECKSUM.1].fill(b' ');
        let sum: u64 = header.iter().map(|&b| u64::from(b)).sum();
        // Six digits, a NUL and a space, as tar writes it.
        let digits = format!("{sum:06o}\0 ");
        header[CHECKSUM.0..CHECKSUM.0 + CHECKSUM.1].copy_from_slice(digits.as_bytes());
        self.bytes.extend_from_slice(&header);
        self.bytes.extend_from_slice(data);
        self.bytes.resize(self.bytes.len().next_multiple_of(BLOCK), 0);
        self.last = Some(path.to_owned());
        Ok(())
    }
}

/// Writes `value` in octal into field `(offset, len)`: zero-padded digits
/// and a NUL.
fn octal(header: &mut [u8; BLOCK], (offset, len): (usize, usize), value: u64) -> Result<(), Error> {
    let digits = format!("{value:0width$o}\0", width = len - 1);
    if digits.len() != len {
        return Err(Error::BadNumber);
    }
    header[offset..offset + len].copy_from_slice(digits.as_bytes());
    Ok(())
}
