// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! LZNT1, the compression of NTFS's compressed files, as Microsoft's
//! *[MS-XCA]: Xpress Compression Algorithm* specifies it (section 2.5).
//!
//! A compressed file is compressed a unit at a time: 16 clusters, which
//! are stored in fewer when they compress, the rest of the unit a hole.
//! A unit's data is chunks of 4 KiB each. A chunk's two-byte header gives
//! its length, the header's included, less 3, and whether it is
//! compressed; a header of 0 ends the data, and what is left of the unit
//! is zeros. A compressed chunk is groups of a flag byte and eight
//! items: a byte as it is for a clear bit, and for a set one a two-byte
//! token that copies bytes already written in the chunk. The token's top
//! bits are how far back, less 1, and the rest how many, less 3; the
//! split moves as the chunk fills, so that what is far back can be
//! reached: 12 bits of length in the first 16 bytes, then one fewer each
//! time the position doubles.

use radix_vfs::{Error, Result};

/// The bytes a chunk holds uncompressed.
pub const CHUNK: usize = 4096;

/// Uncompresses the unit `input` into `out`, all of which it writes: what
/// the data does not reach is zeros. Fails with [`Error::Corrupt`] for
/// data that copies from before its chunk's start or past its end.
pub fn decompress(input: &[u8], out: &mut [u8]) -> Result<()> {
    out.fill(0);
    let mut at = 0;
    let mut start = 0;
    while start < out.len() && at + 2 <= input.len() {
        let header = u16::from_le_bytes([input[at], input[at + 1]]);
        if header == 0 {
            break;
        }
        let len = usize::from(header & 0xfff) + 3;
        let data = input.get(at + 2..at + len).ok_or(Error::Corrupt)?;
        let end = (start + CHUNK).min(out.len());
        let chunk = &mut out[start..end];
        if header & 0x8000 == 0 {
            // Stored as it is.
            let n = data.len().min(chunk.len());
            chunk[..n].copy_from_slice(&data[..n]);
        } else {
            chunk_into(data, chunk)?;
        }
        at += len;
        start += CHUNK;
    }
    Ok(())
}

/// Uncompresses one compressed chunk's `data` into `out`.
fn chunk_into(data: &[u8], out: &mut [u8]) -> Result<()> {
    let mut at = 0;
    let mut pos = 0;
    while at < data.len() {
        let flags = data[at];
        at += 1;
        for bit in 0..8 {
            if at >= data.len() {
                break;
            }
            if flags & (1 << bit) == 0 {
                *out.get_mut(pos).ok_or(Error::Corrupt)? = data[at];
                pos += 1;
                at += 1;
                continue;
            }
            let token = data.get(at..at + 2).ok_or(Error::Corrupt)?;
            let token = u16::from_le_bytes([token[0], token[1]]);
            at += 2;
            // The bits of the length shrink as the position grows past
            // 16 bytes.
            let mut shift = 0;
            let mut p = pos.checked_sub(1).ok_or(Error::Corrupt)?;
            while p >= 0x10 {
                p >>= 1;
                shift += 1;
            }
            let back = usize::from(token >> (12 - shift)) + 1;
            let len = usize::from(token & (0xfff >> shift)) + 3;
            if back > pos || pos + len > out.len() {
                return Err(Error::Corrupt);
            }
            // Byte by byte, as the copy may overlap what it writes.
            let end = pos + len;
            while pos < end {
                out[pos] = out[pos - back];
                pos += 1;
            }
        }
    }
    Ok(())
}
