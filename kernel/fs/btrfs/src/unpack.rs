// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compressed extents, uncompressed whole into a buffer: with zlib
//! ([`crate::inflate`]), LZO ([`crate::lzo`]) or zstd ([`crate::zstd`]).
//!
//! An extent compressed holds up to 128 KiB of a file's data in up to
//! 128 KiB of the disk, as Linux makes them, and its sectors' checksums
//! are of the compressed bytes: [`crate::node`] reads and checks each
//! sector, then has the extent uncompressed here. Data that ends before
//! the extent's size reads as zeros after its end, as on Linux; data that
//! does not decode fails the read with `Io`.
//!
//! A filesystem has one [`Workspace`] of buffers and tables, made at its
//! first compressed read and dropped when memory runs short; reads of
//! compressed extents take its lock in turn. A file's page cache keeps
//! every page of an extent once it is uncompressed, so it is uncompressed
//! once.

use radix_block::PAGE_SIZE;
use radix_block::mem::CachePage;
use radix_vfs::mem::KVec;
use radix_vfs::{Error, Result};

use crate::extent::Compression;
use crate::inflate::Inflate;
use crate::lzo;
use crate::zstd::{self, Zstd};

/// The most data an extent compressed holds.
pub const MAX_UNCOMPRESSED: usize = 128 * 1024;

/// The most of the disk an extent compressed takes.
pub const MAX_COMPRESSED: usize = 128 * 1024;

/// Buffers and tables for uncompressing extents.
pub(crate) struct Workspace {
    /// A sector being read.
    pub(crate) sector: CachePage,
    /// An extent's compressed bytes.
    pub(crate) input: KVec<u8>,
    /// Its data: room for an extent and for a segment of LZO past it.
    output: KVec<u8>,
    inflate: Option<Inflate>,
    zstd: Option<Zstd>,
}

impl Workspace {
    pub(crate) fn new() -> Result<Workspace> {
        Ok(Workspace {
            sector: CachePage::new()?,
            input: crate::zeroed(MAX_COMPRESSED)?,
            output: crate::zeroed(MAX_UNCOMPRESSED + lzo::SEGMENT_MAX)?,
            inflate: None,
            zstd: None,
        })
    }

    /// Uncompresses the first `input` bytes of [`Workspace::input`] into
    /// `len` bytes of data; returns them.
    pub(crate) fn unpack_input(
        &mut self,
        compression: Compression,
        input: usize,
        len: usize,
        sector: usize,
    ) -> Result<&[u8]> {
        let Workspace { input: buf, output, inflate, zstd, .. } = self;
        let input = buf.get(..input).ok_or(Error::Corrupt)?;
        unpack(compression, input, output, len, sector, inflate, zstd)?;
        Ok(&output[..len])
    }

    /// Uncompresses `input` into `len` bytes of data; returns them.
    pub(crate) fn unpack(
        &mut self,
        compression: Compression,
        input: &[u8],
        len: usize,
        sector: usize,
    ) -> Result<&[u8]> {
        let Workspace { output, inflate, zstd, .. } = self;
        unpack(compression, input, output, len, sector, inflate, zstd)?;
        Ok(&output[..len])
    }

    /// The pages of memory it holds, about.
    pub(crate) fn pages(&self) -> usize {
        let tables = self.zstd.as_ref().map_or(0, |_| zstd::MAX_BLOCK + 8 * 1024)
            + self.inflate.as_ref().map_or(0, |_| 5 * 1024);
        (PAGE_SIZE + self.input.capacity() + self.output.capacity() + tables).div_ceil(PAGE_SIZE)
    }
}

/// Uncompresses `input` into the first `len` bytes of `output`, with
/// zeros after the data when it ends early.
#[inline(never)]
fn unpack(
    compression: Compression,
    input: &[u8],
    output: &mut [u8],
    len: usize,
    sector: usize,
    inflate: &mut Option<Inflate>,
    zstd: &mut Option<Zstd>,
) -> Result<()> {
    if len > MAX_UNCOMPRESSED {
        return Err(Error::Corrupt);
    }
    let made = match compression {
        Compression::None => return Err(Error::Corrupt),
        Compression::Zlib => {
            let inflate = match inflate {
                Some(inflate) => inflate,
                empty => empty.insert(Inflate::new()?),
            };
            inflate.zlib(input, &mut output[..len])?
        }
        Compression::Lzo => lzo::btrfs(input, output, len, sector)?,
        Compression::Zstd => {
            let zstd = match zstd {
                Some(zstd) => zstd,
                empty => empty.insert(Zstd::new()?),
            };
            zstd.decompress(input, &mut output[..len])?
        }
    };
    if made < len {
        output[made..len].fill(0);
    }
    Ok(())
}
