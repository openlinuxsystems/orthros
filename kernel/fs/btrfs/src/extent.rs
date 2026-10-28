// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! File extents: where a file's bytes are, from an offset in it
//! (`EXTENT_DATA` items, keyed by that offset), and the checksums of the
//! data they point to (`EXTENT_CSUM` items in the checksum tree, keyed by
//! the logical address of their first sector).
//!
//! A file's bytes are inline in the item when the file is small, in an
//! extent elsewhere on the disk (a part of which the item may use, when
//! later writes replaced the rest), or preallocated and not yet written,
//! which reads as zeros. Bytes no item covers are a hole, also zeros.

use radix_block::SegmentMut;
use radix_vfs::{Error, Result};

use crate::format::{Key, item, tree, u16_at, u64_at};
use crate::tree::{Cursor, Root};
use crate::volume::Volume;

/// How the data of an extent is compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// Not at all.
    None,
    /// With zlib (RFC 1950).
    Zlib,
    /// With LZO1X, in Btrfs's segments.
    Lzo,
    /// With Zstandard (RFC 8878).
    Zstd,
}

/// What an `EXTENT_DATA` item says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extent<'a> {
    /// Bytes kept in the item: `ram_bytes` of them, once uncompressed.
    Inline {
        /// Their size, uncompressed.
        ram_bytes: u64,
        /// How they are compressed.
        compression: Compression,
        /// Them, as kept.
        data: &'a [u8],
    },
    /// Bytes of an extent elsewhere on the disk.
    Regular(Regular),
    /// `num_bytes` bytes allocated and not yet written: zeros.
    Prealloc {
        /// How many.
        num_bytes: u64,
    },
}

/// `num_bytes` bytes from `offset` into the extent of `disk_num_bytes`
/// bytes at logical address `disk_bytenr` (`ram_bytes` once uncompressed);
/// a hole if `disk_bytenr` is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regular {
    /// The extent's logical address.
    pub disk_bytenr: u64,
    /// Its size on the disk.
    pub disk_num_bytes: u64,
    /// Where the file's bytes start in it, uncompressed.
    pub offset: u64,
    /// How many of its bytes the file has.
    pub num_bytes: u64,
    /// Its size, uncompressed.
    pub ram_bytes: u64,
    /// How it is compressed.
    pub compression: Compression,
}

/// The size of an item's header, and of a regular or preallocated one.
const INLINE_START: usize = 21;
const REGULAR_SIZE: usize = 53;

impl<'a> Extent<'a> {
    /// The extent the `EXTENT_DATA` item `bytes` describes.
    pub fn read(bytes: &'a [u8]) -> Result<Extent<'a>> {
        if bytes.len() < INLINE_START {
            return Err(Error::Corrupt);
        }
        let ram_bytes = u64_at(bytes, 8);
        let compression = match bytes[16] {
            0 => Compression::None,
            1 => Compression::Zlib,
            2 => Compression::Lzo,
            3 => Compression::Zstd,
            _ => return Err(Error::Corrupt),
        };
        // Encryption and other encodings are reserved, and no Linux sets
        // them.
        if bytes[17] != 0 || u16_at(bytes, 18) != 0 {
            return Err(Error::NotSupported);
        }
        let regular = || -> Result<(u64, u64, u64, u64)> {
            if bytes.len() != REGULAR_SIZE {
                return Err(Error::Corrupt);
            }
            Ok((u64_at(bytes, 21), u64_at(bytes, 29), u64_at(bytes, 37), u64_at(bytes, 45)))
        };
        match bytes[20] {
            0 => {
                let data = &bytes[INLINE_START..];
                if compression == Compression::None && data.len() as u64 != ram_bytes {
                    return Err(Error::Corrupt);
                }
                Ok(Extent::Inline { ram_bytes, compression, data })
            }
            1 => {
                let (disk_bytenr, disk_num_bytes, offset, num_bytes) = regular()?;
                Ok(Extent::Regular(Regular {
                    disk_bytenr,
                    disk_num_bytes,
                    offset,
                    num_bytes,
                    ram_bytes,
                    compression,
                }))
            }
            2 => {
                let (_, _, _, num_bytes) = regular()?;
                Ok(Extent::Prealloc { num_bytes })
            }
            _ => Err(Error::Corrupt),
        }
    }

    /// How many of the file's bytes it covers.
    pub fn len(&self) -> u64 {
        match *self {
            Extent::Inline { ram_bytes, .. } => ram_bytes,
            Extent::Regular(Regular { num_bytes, .. }) | Extent::Prealloc { num_bytes } => {
                num_bytes
            }
        }
    }

    /// Whether it covers none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The checksum the checksum tree `csums` keeps for the data sector at
/// logical address `logical`; `None` if it keeps none.
pub fn data_sum(volume: &Volume, csums: &Root, logical: u64) -> Result<Option<[u8; 32]>> {
    let key = Key::new(tree::EXTENT_CSUM, item::EXTENT_CSUM, logical);
    let Some(cursor) = Cursor::seek_last(volume, csums, &key)? else { return Ok(None) };
    let Some((found, sums)) = cursor.get() else { return Ok(None) };
    if !found.is(tree::EXTENT_CSUM, item::EXTENT_CSUM) {
        return Ok(None);
    }
    // One checksum for each sector from the item's address on.
    let sb = volume.superblock();
    let size = sb.csum_type.size();
    let sector = u64::from(sb.sectorsize);
    if sums.len() % size != 0 {
        return Err(Error::Corrupt);
    }
    let nth = (logical - found.offset) / sector;
    let Some(at) = usize::try_from(nth).ok().and_then(|n| n.checked_mul(size)) else {
        return Ok(None);
    };
    let Some(sum) = sums.get(at..at + size) else { return Ok(None) };
    let mut out = [0; 32];
    out[..size].copy_from_slice(sum);
    Ok(Some(out))
}

/// Checks the data sectors in the pages of `sectors`, which a read put
/// there from logical address `logical` on, one each, against the
/// checksum tree `csums`, walking its items once; calls `bad` with the
/// place in `sectors` of each one whose checksum differs or is missing.
pub fn check_sums(
    volume: &Volume,
    csums: &Root,
    logical: u64,
    sectors: &[SegmentMut<'_>],
    bad: &mut dyn FnMut(usize),
) -> Result<()> {
    let csum = volume.superblock().csum_type;
    each_sum(volume, csums, logical, sectors.len(), &mut |at, sum| {
        if !sum.is_some_and(|sum| csum.matches(sectors[at].page.bytes(), sum)) {
            bad(at);
        }
    })
}

/// Calls `f` with the place of each of the `count` data sectors from
/// logical address `logical` on, in order, and the checksum the tree
/// `csums` keeps for it, if it keeps one; walks the tree's items once.
pub fn each_sum(
    volume: &Volume,
    csums: &Root,
    logical: u64,
    count: usize,
    f: &mut dyn FnMut(usize, Option<&[u8]>),
) -> Result<()> {
    let sb = volume.superblock();
    let (size, sector) = (sb.csum_type.size(), u64::from(sb.sectorsize));
    let key = Key::new(tree::EXTENT_CSUM, item::EXTENT_CSUM, logical);
    // The item that covers `logical`, or else the first after it.
    let mut cursor = match Cursor::seek_last(volume, csums, &key)? {
        Some(cursor) => cursor,
        None => Cursor::seek(volume, csums, &key)?,
    };
    let mut at = 0;
    while at < count {
        let address = logical + at as u64 * sector;
        let Some((found, sums)) = cursor.get() else { break };
        if !found.is(tree::EXTENT_CSUM, item::EXTENT_CSUM) {
            break;
        }
        if !sums.len().is_multiple_of(size) {
            return Err(Error::Corrupt);
        }
        let end = found.offset.saturating_add((sums.len() / size) as u64 * sector);
        if address >= end {
            cursor.advance()?;
            continue;
        }
        if address < found.offset {
            // No checksum for the sectors before the item.
            f(at, None);
        } else {
            let from = ((address - found.offset) / sector) as usize * size;
            f(at, Some(&sums[from..from + size]));
        }
        at += 1;
    }
    // Past the last item, no sector has its checksum.
    (at..count).for_each(|at| f(at, None));
    Ok(())
}
