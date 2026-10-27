// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! LZO1X, in which Btrfs keeps extents compressed with LZO, in segments
//! of its own.
//!
//! An extent starts with its compressed length (4 bytes, little-endian,
//! itself included), then holds segments: each a length (4 bytes) and
//! that many bytes of one LZO1X stream, which makes at most one sector.
//! A segment's length never crosses a sector: where fewer than 4 bytes
//! are left in one, they are zeros, and the next segment starts in the
//! next sector. This is the layout Linux and `mkfs.btrfs` write, which
//! the tests read.
//!
//! LZO1X is a stream of literal runs and matches, each one instruction
//! whose meaning, below 16, hangs on how many literals the one before it
//! copied; the format is as `Documentation/staging/lzo.rst` in Linux
//! describes it. A stream ends with the match of distance 16384 and
//! length 3 (bytes 0x11 0 0), and must end there: Linux fails a segment
//! with bytes after its end.

use radix_vfs::{Error, Result};

use crate::format::u32_at;

/// The most a segment of a sector of 4 KiB holds: LZO1X's worst case for
/// that much data, as Linux bounds it.
pub const SEGMENT_MAX: usize = 4096 + 4096 / 16 + 64 + 3;

/// The length fields of the extent and of each segment.
const LEN: usize = 4;

fn bad<T>() -> Result<T> {
    Err(Error::Io)
}

/// Uncompresses the Btrfs LZO extent `input`, of sectors of `sector`
/// bytes, into `out` until it holds `want` bytes or the extent ends;
/// returns how many it made. `out` has [`SEGMENT_MAX`] bytes of room
/// past `want`, as a last segment may make more than is wanted.
pub fn btrfs(input: &[u8], out: &mut [u8], want: usize, sector: usize) -> Result<usize> {
    if input.len() < LEN {
        return bad();
    }
    let total = u32_at(input, 0) as usize;
    if total < LEN || total > input.len() {
        return bad();
    }
    let (mut at, mut made) = (LEN, 0);
    while at < total && made < want {
        if sector - at % sector < LEN {
            at = at.next_multiple_of(sector);
            continue;
        }
        if at + LEN > total {
            return bad();
        }
        let len = u32_at(input, at) as usize;
        at += LEN;
        if len > SEGMENT_MAX || len > total - at {
            return bad();
        }
        let room = (made + SEGMENT_MAX).min(out.len());
        made += decompress(&input[at..at + len], &mut out[made..room])?;
        at += len;
    }
    Ok(made.min(want))
}

/// Uncompresses the LZO1X stream `input` into `out`; returns how many
/// bytes it made. A stream that would make more than `out` holds, reach
/// before its start, or read past its end fails.
pub fn decompress(input: &[u8], out: &mut [u8]) -> Result<usize> {
    let mut s = Stream { input, at: 0, out, made: 0 };
    // How many literals the last instruction copied: 0, 1 to 3, or 4 for
    // a run of 4 or more.
    let mut state;
    let mut op = s.byte()?;
    if op > 17 {
        let n = usize::from(op - 17);
        s.literals(n)?;
        state = n.min(4);
        op = s.byte()?;
    } else {
        state = 0;
    }
    loop {
        let (dist, len, next) = if op < 16 {
            match state {
                0 => {
                    // A run of literals: 3 more than its length.
                    let n = if op == 0 { 15 + s.extended()? } else { usize::from(op) };
                    s.literals(n + 3)?;
                    state = 4;
                    op = s.byte()?;
                    continue;
                }
                // Two bytes from up to 1 KiB back.
                1..=3 => {
                    let high = usize::from(s.byte()?);
                    ((high << 2) + usize::from(op >> 2) + 1, 2, op & 3)
                }
                // Three bytes from 2 KiB to 3 KiB back.
                _ => {
                    let high = usize::from(s.byte()?);
                    ((high << 2) + usize::from(op >> 2) + 2049, 3, op & 3)
                }
            }
        } else if op >= 64 {
            // 3 to 8 bytes from up to 2 KiB back.
            let high = usize::from(s.byte()?);
            ((high << 3) + usize::from((op >> 2) & 7) + 1, usize::from(op >> 5) + 1, op & 3)
        } else if op >= 32 {
            // From up to 16 KiB back.
            let n = match op & 31 {
                0 => 31 + s.extended()?,
                n => usize::from(n),
            };
            let low = s.le16()?;
            ((low >> 2) + 1, n + 2, (low & 3) as u8)
        } else {
            // From 16 KiB to 48 KiB back, or the end of the stream.
            let n = match op & 7 {
                0 => 7 + s.extended()?,
                n => usize::from(n),
            };
            let low = s.le16()?;
            let dist = 16384 + (usize::from(op & 8) << 11) + (low >> 2);
            if dist == 16384 {
                return if n == 1 && s.at == input.len() { Ok(s.made) } else { bad() };
            }
            (dist, n + 2, (low & 3) as u8)
        };
        s.copy(dist, len)?;
        state = usize::from(next);
        s.literals(state)?;
        op = s.byte()?;
    }
}

/// A stream being read, and what it made.
struct Stream<'a, 'o> {
    input: &'a [u8],
    at: usize,
    out: &'o mut [u8],
    made: usize,
}

impl Stream<'_, '_> {
    fn byte(&mut self) -> Result<u8> {
        let Some(&byte) = self.input.get(self.at) else { return bad() };
        self.at += 1;
        Ok(byte)
    }

    fn le16(&mut self) -> Result<usize> {
        let low = self.byte()?;
        let high = self.byte()?;
        Ok(usize::from(low) | usize::from(high) << 8)
    }

    /// A length past its instruction's field: 255 for each zero byte,
    /// then the byte that ends it.
    fn extended(&mut self) -> Result<usize> {
        let mut n = 0usize;
        loop {
            match self.byte()? {
                0 => {
                    n += 255;
                    // No output is that long.
                    if n > self.out.len() {
                        return bad();
                    }
                }
                last => return Ok(n + usize::from(last)),
            }
        }
    }

    /// Copies `n` literals.
    fn literals(&mut self, n: usize) -> Result<()> {
        let (Some(from), Some(to)) = (
            self.input.get(self.at..self.at.saturating_add(n)),
            self.out.get_mut(self.made..self.made.saturating_add(n)),
        ) else {
            return bad();
        };
        to.copy_from_slice(from);
        self.at += n;
        self.made += n;
        Ok(())
    }

    /// Copies `len` bytes from `dist` back, overlapping as they may.
    fn copy(&mut self, dist: usize, len: usize) -> Result<()> {
        if dist > self.made || len > self.out.len() - self.made {
            return bad();
        }
        let from = self.made - dist;
        if dist >= len {
            self.out.copy_within(from..from + len, self.made);
        } else {
            for i in 0..len {
                self.out[self.made + i] = self.out[from + i];
            }
        }
        self.made += len;
        Ok(())
    }
}
