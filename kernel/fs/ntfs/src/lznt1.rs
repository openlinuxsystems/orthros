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

use radix_vfs::mem::{self, KVec};
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

/// The bits a token's length has at position `pos` of a chunk (past its
/// first byte): 12, then one fewer each time the position doubles past
/// 16 bytes.
fn length_bits(pos: usize) -> u32 {
    let mut shift = 0;
    let mut p = pos - 1;
    while p >= 0x10 {
        p >>= 1;
        shift += 1;
    }
    12 - shift
}

/// How many of the last positions with the same three bytes a match is
/// looked for at.
const TRIES: usize = 64;

/// The positions of a chunk's three-byte sequences, by hash, to find
/// matches with: the last position of each hash, and before each position
/// the last of its hash before it (both plus 1, 0 for none).
pub struct Matcher {
    head: KVec<u16>,
    prev: KVec<u16>,
}

impl Matcher {
    /// A matcher, its tables on the heap.
    pub fn new() -> Result<Matcher> {
        let mut head = mem::vec_with_capacity(CHUNK)?;
        let mut prev = mem::vec_with_capacity(CHUNK)?;
        for _ in 0..CHUNK {
            let _ = head.push_within_capacity(0);
            let _ = prev.push_within_capacity(0);
        }
        Ok(Matcher { head, prev })
    }

    fn hash(bytes: &[u8]) -> usize {
        ((usize::from(bytes[0]) << 6) ^ (usize::from(bytes[1]) << 3) ^ usize::from(bytes[2]))
            & (CHUNK - 1)
    }

    fn insert(&mut self, chunk: &[u8], pos: usize) {
        if pos + 3 <= chunk.len() {
            let h = Self::hash(&chunk[pos..]);
            self.prev[pos] = self.head[h];
            self.head[h] = pos as u16 + 1;
        }
    }

    /// The longest match for position `pos` of `chunk`, at most `max`
    /// bytes and `far` back: how far back and how long, if 3 bytes or
    /// more.
    fn find(&self, chunk: &[u8], pos: usize, max: usize, far: usize) -> Option<(usize, usize)> {
        if pos + 3 > chunk.len() {
            return None;
        }
        let mut best: Option<(usize, usize)> = None;
        let mut cand = self.head[Self::hash(&chunk[pos..])];
        for _ in 0..TRIES {
            let Some(at) = usize::from(cand).checked_sub(1) else { break };
            let back = pos - at;
            if back > far {
                break;
            }
            let len = chunk[at..].iter().zip(&chunk[pos..]).take(max).take_while(|(a, b)| a == b);
            let len = len.count();
            if len >= 3 && best.is_none_or(|(_, l)| len > l) {
                best = Some((back, len));
                if len == max {
                    break;
                }
            }
            cand = self.prev[at];
        }
        best
    }
}

/// Compresses the unit `input` into `out`: each chunk of 4 KiB compressed,
/// or stored as it is where that is no longer, and nothing for the zeros
/// at its end. Returns how many bytes of `out` it wrote, or None if they
/// would not fit in it.
pub fn compress(input: &[u8], out: &mut [u8], matcher: &mut Matcher) -> Option<usize> {
    // The zeros at the end are left out: the data ends before them.
    let used = input.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let mut at = 0;
    for chunk in input[..used.next_multiple_of(CHUNK).min(input.len())].chunks(CHUNK) {
        let room = out.len().checked_sub(at + 2)?;
        let n = match compress_chunk(chunk, &mut out[at + 2..], matcher) {
            Some(n) if n < chunk.len() && n < room.min(0x1000) => {
                put_header(out, at, n, true);
                n
            }
            // Stored as it is: the chunk's whole 4 KiB, as Windows reads
            // them.
            _ => {
                if room < CHUNK {
                    return None;
                }
                out[at + 2..at + 2 + chunk.len()].copy_from_slice(chunk);
                out[at + 2 + chunk.len()..at + 2 + CHUNK].fill(0);
                put_header(out, at, CHUNK, false);
                CHUNK
            }
        };
        at += 2 + n;
    }
    Some(at)
}

/// A chunk's header at `at` of `out`, for `n` bytes of data after it.
fn put_header(out: &mut [u8], at: usize, n: usize, compressed: bool) {
    let header = (n + 2 - 3) as u16 | 0x3000 | if compressed { 0x8000 } else { 0 };
    out[at..at + 2].copy_from_slice(&header.to_le_bytes());
}

/// Compresses one chunk into `out`; returns its length, or None if it
/// does not fit.
fn compress_chunk(chunk: &[u8], out: &mut [u8], matcher: &mut Matcher) -> Option<usize> {
    matcher.head.fill(0);
    let mut at = 0;
    let mut pos = 0;
    while pos < chunk.len() {
        let flags_at = at;
        *out.get_mut(at)? = 0;
        at += 1;
        for bit in 0..8 {
            if pos >= chunk.len() {
                break;
            }
            let found = if pos == 0 {
                None
            } else {
                let bits = length_bits(pos);
                let max = ((1usize << bits) + 2).min(chunk.len() - pos);
                matcher.find(chunk, pos, max, 1 << (16 - bits))
            };
            match found {
                Some((back, len)) => {
                    let bits = length_bits(pos);
                    let token = (((back - 1) << bits) | (len - 3)) as u16;
                    out.get_mut(at..at + 2)?.copy_from_slice(&token.to_le_bytes());
                    at += 2;
                    out[flags_at] |= 1 << bit;
                    for p in pos..pos + len {
                        matcher.insert(chunk, p);
                    }
                    pos += len;
                }
                None => {
                    *out.get_mut(at)? = chunk[pos];
                    at += 1;
                    matcher.insert(chunk, pos);
                    pos += 1;
                }
            }
        }
    }
    Some(at)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(input: &[u8]) -> usize {
        let mut matcher = Matcher::new().unwrap();
        let mut out = vec![0u8; input.len() + input.len() / CHUNK * 2 + 16];
        let n = compress(input, &mut out, &mut matcher).unwrap();
        let mut back = vec![0xaau8; input.len()];
        decompress(&out[..n], &mut back).unwrap();
        assert_eq!(back, input);
        n
    }

    #[test]
    fn units_compress_and_read_back() {
        let unit = 64 << 10;
        // Text compresses; zeros take nothing; noise is stored.
        let text: Vec<u8> = (0..unit).map(|i| b"a line of text, again\n"[i % 22]).collect();
        assert!(round_trip(&text) < unit / 8);
        assert_eq!(round_trip(&vec![0u8; unit]), 0);
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let noise: Vec<u8> = (0..unit)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        assert_eq!(round_trip(&noise), unit / CHUNK * (CHUNK + 2));
        // Data that ends early, and a run longer than a token holds.
        let mut mixed = text.clone();
        mixed[40_000..].fill(0);
        mixed[1000..3000].fill(7);
        round_trip(&mixed);
        round_trip(&text[..8192]);
        round_trip(b"abcabcabcabcabcabcabcabcabcabcabcabcabcx");
    }

    #[test]
    fn a_unit_too_big_does_not_fit() {
        let mut matcher = Matcher::new().unwrap();
        let mut out = vec![0u8; 100];
        let data: Vec<u8> = (0..CHUNK).map(|i| (i * 7 + i / 3) as u8).collect();
        assert_eq!(compress(&data, &mut out, &mut matcher), None);
    }
}
