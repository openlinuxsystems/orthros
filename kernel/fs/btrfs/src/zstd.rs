// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Zstandard frames (RFC 8878), in which Btrfs keeps extents compressed
//! with zstd, decoded into a buffer.
//!
//! An extent is one frame, without a dictionary, as Linux writes it and
//! reads it (Linux stops at the end of the first frame). The whole of the
//! output so far is the window, so a frame's window size is not needed;
//! a match may reach back to the frame's first byte and no further. A
//! frame's checksum (XXH64) and content size are checked when it has
//! them. Output that would pass the end of the buffer ends the decoding,
//! with the buffer full, as for [`crate::inflate`].
//!
//! A compressed block holds its literals, raw, as one byte repeated, or
//! Huffman-coded in one or four streams, and then its sequences: each a
//! run of literals and a match, coded with three FSE (tANS) codes, for
//! the literals' length, the match's offset and the match's length. FSE
//! and Huffman streams are read backwards, from their last byte, whose
//! highest set bit marks where they end.

use radix_vfs::mem::{KVec, NoMemory};
use radix_vfs::{Error, Result};

use crate::format::{u16_at, u32_at, u64_at};

pub mod encode;

/// The magic number that starts a frame.
const MAGIC: u32 = 0xFD2F_B528;

/// The most a block holds, compressed or not.
pub const MAX_BLOCK: usize = 128 * 1024;

/// The literals buffer: a block's literals, with room for copies of 16
/// bytes to run past the last.
const LITERALS: usize = MAX_BLOCK + 16;

/// The states of the largest FSE code of sequences (accuracy log 9).
const STATES: usize = 1 << 9;

/// The sequences decoded at a time, before their copies are made: apart,
/// each loop keeps what it needs in registers.
const BATCH: usize = 128;

/// The most bits a Huffman code of literals has.
const HUFFMAN_MAX_BITS: u32 = 11;

/// Why decoding stopped early.
enum Stop {
    /// The output is full: the extent holds no more.
    Full,
    /// The frame is bad: the read fails with `EIO`, as on Linux.
    Bad,
}

type Step<T> = core::result::Result<T, Stop>;

/// Bytes `at..at + len` of `data`, or [`Stop::Bad`] past its end.
fn get(data: &[u8], at: usize, len: usize) -> Step<&[u8]> {
    data.get(at..at.checked_add(len).ok_or(Stop::Bad)?).ok_or(Stop::Bad)
}

/// The little-endian number of the `len` bytes (up to 8) at `at`.
fn le(data: &[u8], at: usize, len: usize) -> Step<u64> {
    let bytes = get(data, at, len)?;
    Ok(bytes.iter().rev().fold(0, |n, &b| n << 8 | u64::from(b)))
}

/// The three codes of a block's sequences.
#[derive(Clone, Copy)]
enum Kind {
    Literals,
    Offsets,
    Matches,
}

impl Kind {
    /// The largest symbol, and the largest accuracy, of the kind's code.
    fn limits(self) -> (usize, u32) {
        match self {
            Kind::Literals => (35, 9),
            Kind::Offsets => (31, 8),
            Kind::Matches => (52, 9),
        }
    }

    /// The code used when a block names none (RFC 8878, 3.1.1.3.2.2):
    /// its accuracy and each symbol's share, -1 for "less than one".
    fn predefined(self) -> (u32, &'static [i16]) {
        match self {
            Kind::Literals => (
                6,
                &[
                    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3,
                    2, 1, 1, 1, 1, 1, -1, -1, -1, -1,
                ],
            ),
            Kind::Offsets => (
                5,
                &[
                    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1,
                    -1, -1, -1,
                ],
            ),
            Kind::Matches => (
                6,
                &[
                    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
                    -1, -1,
                ],
            ),
        }
    }
}

/// The literal lengths of codes 0 to 35, and their extra bits.
const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
const LL_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];

/// The match lengths of codes 0 to 52, and their extra bits.
const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
const ML_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// A stream read forwards, from the low bit of each byte up: the FSE
/// codes' descriptions.
struct Forward<'a> {
    data: &'a [u8],
    bit: usize,
}

impl Forward<'_> {
    /// The next `n` bits (up to 16), zero past the end.
    fn peek(&self, n: u32) -> u32 {
        let (byte, shift) = (self.bit / 8, self.bit % 8);
        let mut word = 0u32;
        for i in 0..3 {
            if let Some(&b) = self.data.get(byte + i) {
                word |= u32::from(b) << (8 * i);
            }
        }
        (word >> shift) & ((1 << n) - 1)
    }

    fn consume(&mut self, n: u32) -> Step<()> {
        self.bit += n as usize;
        if self.bit > self.data.len() * 8 { Err(Stop::Bad) } else { Ok(()) }
    }

    fn take(&mut self, n: u32) -> Step<u32> {
        let value = self.peek(n);
        self.consume(n)?;
        Ok(value)
    }
}

/// A stream read backwards, from its end: Huffman and FSE data. Its
/// bits are read from a window of the 8 bytes before `end`, from the
/// top; reading past the start reads zeros and takes [`Backward::left`]
/// below zero.
struct Backward<'a> {
    data: &'a [u8],
    /// The 8 bytes before `end`, little-endian, zero before the start.
    bits: u64,
    /// How many of `bits`, from the top, are read.
    used: u32,
    end: usize,
}

/// The 8 bytes of `data` before `end`, little-endian, zero before its
/// start.
fn window(data: &[u8], end: usize) -> u64 {
    match end.checked_sub(8).and_then(|start| data.get(start..end)) {
        Some(bytes) => u64_at(bytes, 0),
        None => {
            let mut word = 0;
            for (i, &b) in data.iter().take(end).enumerate() {
                word |= u64::from(b) << (8 * (i + 8 - end));
            }
            word
        }
    }
}

impl<'a> Backward<'a> {
    /// The stream `data`, whose last byte's highest set bit marks its end.
    fn new(data: &'a [u8]) -> Step<Backward<'a>> {
        let last = *data.last().ok_or(Stop::Bad)?;
        if last == 0 {
            return Err(Stop::Bad);
        }
        let end = data.len();
        Ok(Backward { data, bits: window(data, end), used: last.leading_zeros() + 1, end })
    }

    /// The bits not yet read; below zero once reads pass the start.
    fn left(&self) -> isize {
        (self.end * 8) as isize - self.used as isize
    }

    /// Moves the window back over the whole bytes read, as far as the
    /// start.
    #[inline(always)]
    fn refill(&mut self) {
        let back = (self.used >> 3) as usize;
        if back + 8 <= self.end {
            // Most refills: whole bytes back, far from the start.
            self.end -= back;
            self.used &= 7;
            self.bits = u64_at(self.data, self.end - 8);
            return;
        }
        let back = back.min(self.end);
        self.end -= back;
        self.used -= (back as u32) << 3;
        self.bits = match self.data.get(self.end.wrapping_sub(8)..self.end) {
            Some(bytes) => u64_at(bytes, 0),
            None => window(self.data, self.end),
        };
    }

    /// The next `n` bits (up to 56): the first read is the highest.
    #[inline(always)]
    fn peek(&mut self, n: u32) -> u64 {
        if self.used + n > 64 {
            self.refill();
        }
        // In two shifts, so that `n` may be 0.
        (self.bits.wrapping_shl(self.used) >> 1) >> (63 - n)
    }

    #[inline(always)]
    fn skip(&mut self, n: u32) {
        self.used += n;
    }

    #[inline(always)]
    fn read(&mut self, n: u32) -> u64 {
        let value = self.peek(n);
        self.skip(n);
        value
    }

    /// The next `n` bits, read without moving the window: its caller
    /// refills it often enough that they are in it. Past the start, and
    /// in a stream too short for its reads, they are no more than `n`
    /// bits of nonsense, which [`Backward::left`] finds at the end.
    #[inline(always)]
    fn take(&mut self, n: u32) -> u64 {
        let value = (self.bits.wrapping_shl(self.used) >> 1) >> (63 - n);
        self.used += n;
        value
    }
}

/// An FSE code's decoding table: for each state, its symbol, the bits to
/// read for the next state and the base they add to
/// (`symbol | bits << 8 | base << 16`).
struct Fse {
    table: KVec<u32>,
    log: u32,
    /// Whether a block of this frame made it, for the next to repeat.
    ready: bool,
}

/// The parts of a table entry.
fn symbol(entry: u32) -> usize {
    (entry & 0xff) as usize
}

/// The state after `state` of `table`, read from `stream`.
#[inline(always)]
fn next_state(table: &[u32], state: usize, stream: &mut Backward<'_>) -> usize {
    let entry = table[state];
    (entry >> 16) as usize + stream.read((entry >> 8) & 0xff) as usize
}

impl Fse {
    fn new() -> core::result::Result<Fse, NoMemory> {
        Ok(Fse { table: crate::filled(STATES, 0)?, log: 0, ready: false })
    }

    /// Fills `seq` from the table of a code of `kind`: each state as
    /// sequences read it, the next state's base and bits and its value's
    /// extra bits and base (`base | bits << 16 | extra << 24 | value << 32`).
    fn spell(&self, kind: Kind, seq: &mut [u64]) -> Step<()> {
        let states = self.table.get(..1 << self.log).ok_or(Stop::Bad)?;
        for (seq, &entry) in seq.iter_mut().zip(states) {
            let sym = symbol(entry);
            let (extra, value) = match kind {
                Kind::Literals => (LL_BITS.get(sym).copied(), LL_BASE.get(sym).copied()),
                Kind::Matches => (ML_BITS.get(sym).copied(), ML_BASE.get(sym).copied()),
                Kind::Offsets => {
                    (u8::try_from(sym).ok().filter(|&s| s < 32), 1u32.checked_shl(sym as u32))
                }
            };
            let (Some(extra), Some(value)) = (extra, value) else { return Err(Stop::Bad) };
            *seq = u64::from(entry >> 16)
                | u64::from((entry >> 8) & 0xff) << 16
                | u64::from(extra) << 24
                | u64::from(value) << 32;
        }
        Ok(())
    }

    /// Sets the code for a block's sequences, and `seq` from it (as
    /// [`Fse::spell`] fills it), from its mode and the bytes after it;
    /// returns how many it used.
    fn prepare(&mut self, mode: u8, data: &[u8], kind: Kind, seq: &mut [u64]) -> Step<usize> {
        let (max_symbol, max_log) = kind.limits();
        let used = match mode {
            0 => {
                let (log, shares) = kind.predefined();
                build(shares, log, &mut self.table)?;
                self.log = log;
                0
            }
            1 => {
                // One symbol, repeated.
                let sym = *data.first().ok_or(Stop::Bad)?;
                if usize::from(sym) > max_symbol {
                    return Err(Stop::Bad);
                }
                self.table[0] = u32::from(sym);
                self.log = 0;
                1
            }
            2 => {
                let mut shares = [0i16; 53];
                let shares = &mut shares[..=max_symbol];
                let (log, used) = read_shares(data, max_log, shares)?;
                build(shares, log, &mut self.table)?;
                self.log = log;
                used
            }
            _ => {
                if !self.ready {
                    return Err(Stop::Bad);
                }
                return Ok(0);
            }
        };
        self.spell(kind, seq)?;
        self.ready = true;
        Ok(used)
    }
}

/// Reads an FSE code's description (RFC 8878, 4.1.1) from the start of
/// `data` into `shares`, the share of each symbol (-1: less than one);
/// returns its accuracy and the bytes it took.
#[inline(never)]
fn read_shares(data: &[u8], max_log: u32, shares: &mut [i16]) -> Step<(u32, usize)> {
    let mut r = Forward { data, bit: 0 };
    let log = r.take(4)? + 5;
    if log > max_log {
        return Err(Stop::Bad);
    }
    shares.fill(0);
    let mut remaining: i32 = (1 << log) + 1;
    let mut threshold: i32 = 1 << log;
    let mut bits = log + 1;
    let mut sym = 0;
    while remaining > 1 {
        let slot = shares.get_mut(sym).ok_or(Stop::Bad)?;
        // Values below `max` take a bit less than the others.
        let max = 2 * threshold - 1 - remaining;
        let low = r.peek(bits - 1) as i32;
        let value = if low < max {
            r.consume(bits - 1)?;
            low
        } else {
            let value = r.take(bits)? as i32;
            if value >= threshold { value - max } else { value }
        };
        let share = value - 1;
        remaining -= share.abs();
        *slot = share as i16;
        sym += 1;
        if share == 0 {
            // Runs of symbols with no share: 2 bits each, 3 to go on.
            loop {
                let run = r.take(2)? as usize;
                if sym + run > shares.len() {
                    return Err(Stop::Bad);
                }
                sym += run;
                if run < 3 {
                    break;
                }
            }
        }
        if remaining < 1 {
            return Err(Stop::Bad);
        }
        while remaining < threshold {
            bits -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return Err(Stop::Bad);
    }
    Ok((log, r.bit.div_ceil(8)))
}

/// Builds the decoding table of the code of accuracy `log` whose
/// symbols have `shares` (RFC 8878, 4.1.1): symbols of less than one
/// share at the table's end, the others spread over the rest.
#[inline(never)]
fn build(shares: &[i16], log: u32, table: &mut [u32]) -> Step<()> {
    let size = 1usize << log;
    let table = table.get_mut(..size).ok_or(Stop::Bad)?;
    let mut next = [0u16; 256];
    let next = next.get_mut(..shares.len()).ok_or(Stop::Bad)?;
    let mut high = size;
    for (sym, &share) in shares.iter().enumerate() {
        if share == -1 {
            high = high.checked_sub(1).ok_or(Stop::Bad)?;
            table[high] = sym as u32;
            next[sym] = 1;
        } else if share > 0 {
            next[sym] = share as u16;
        }
    }
    let step = (size >> 1) + (size >> 3) + 3;
    let mask = size - 1;
    let mut pos = 0;
    for (sym, &share) in shares.iter().enumerate() {
        for _ in 0..share.max(0) {
            if pos >= high {
                return Err(Stop::Bad);
            }
            table[pos] = sym as u32;
            pos = (pos + step) & mask;
            while pos >= high {
                pos = (pos + step) & mask;
            }
        }
    }
    if pos != 0 {
        return Err(Stop::Bad);
    }
    for entry in table.iter_mut() {
        let sym = symbol(*entry);
        let x = u32::from(next[sym]);
        if x == 0 {
            return Err(Stop::Bad);
        }
        next[sym] += 1;
        let bits = log - x.ilog2();
        let base = (x << bits) - size as u32;
        *entry = sym as u32 | bits << 8 | base << 16;
    }
    Ok(())
}

/// What decoding frames needs, kept between them.
pub struct Zstd {
    /// A block's literals, decoded, and room after them.
    literals: KVec<u8>,
    /// The Huffman code of literals: for each value of its next bits, the
    /// symbol and its code's length (`symbol | length << 8`).
    huffman: KVec<u16>,
    /// The longest code of the Huffman code; 0 for none in this frame.
    huffman_bits: u32,
    ll: Fse,
    of: Fse,
    ml: Fse,
    /// The three codes of sequences' states, as [`Fse::spell`] gives
    /// them: literals' lengths, offsets and matches' lengths.
    seqs: KVec<u64>,
    /// Sequences decoded and not yet copied: each its literals' length,
    /// its match's length and its offset.
    batch: KVec<[usize; 3]>,
}

/// The output buffer, and how much of it is made.
struct Output<'o> {
    buf: &'o mut [u8],
    made: usize,
}

/// Copies `src` to the start of `dst`, the short copies most sequences
/// make in words rather than through `memcpy`, whose start costs more
/// than they do.
#[inline(always)]
fn copy_short(dst: &mut [u8], src: &[u8]) {
    let n = src.len();
    let dst = &mut dst[..n];
    if n > 64 {
        dst.copy_from_slice(src);
    } else if n >= 8 {
        // Words, the last one ending where the copy does.
        let mut at = 0;
        while at + 8 < n {
            dst[at..at + 8].copy_from_slice(&src[at..at + 8]);
            at += 8;
        }
        dst[n - 8..].copy_from_slice(&src[n - 8..]);
    } else if n >= 4 {
        dst[..4].copy_from_slice(&src[..4]);
        dst[n - 4..].copy_from_slice(&src[n - 4..]);
    } else {
        for (d, &s) in dst.iter_mut().zip(src) {
            *d = s;
        }
    }
}

impl Output<'_> {
    #[inline(always)]
    fn put(&mut self, bytes: &[u8]) -> Step<()> {
        let room = self.buf.len() - self.made;
        let n = bytes.len().min(room);
        copy_short(&mut self.buf[self.made..], &bytes[..n]);
        self.made += n;
        if n < bytes.len() { Err(Stop::Full) } else { Ok(()) }
    }

    fn fill(&mut self, byte: u8, len: usize) -> Step<()> {
        let room = self.buf.len() - self.made;
        let n = len.min(room);
        self.buf[self.made..self.made + n].fill(byte);
        self.made += n;
        if n < len { Err(Stop::Full) } else { Ok(()) }
    }

    /// Copies the `len` bytes at `at` of `from`, 16 at a time when both
    /// buffers have 16 bytes past the copy for the last to run into.
    #[inline(always)]
    fn put_from(&mut self, from: &[u8], at: usize, len: usize) -> Step<()> {
        let to = self.made;
        if to + len + 16 <= self.buf.len() && at + len + 16 <= from.len() {
            let mut done = 0;
            loop {
                self.buf[to + done..to + done + 16]
                    .copy_from_slice(&from[at + done..at + done + 16]);
                done += 16;
                if done >= len {
                    break;
                }
            }
            self.made += len;
            return Ok(());
        }
        self.put(from.get(at..at + len).ok_or(Stop::Bad)?)
    }

    /// Copies `len` bytes from `offset` back, overlapping as they may.
    #[inline(always)]
    fn copy(&mut self, offset: usize, len: usize) -> Step<()> {
        if offset == 0 || offset > self.made {
            return Err(Stop::Bad);
        }
        let room = self.buf.len() - self.made;
        let from = self.made - offset;
        if offset >= 16 && len + 16 <= room {
            // 16 bytes at a time, each of bytes made before it, the last
            // running past the end.
            let to = self.made;
            let mut done = 0;
            loop {
                let (made, rest) = self.buf.split_at_mut(to + done);
                rest[..16].copy_from_slice(&made[from + done..from + done + 16]);
                done += 16;
                if done >= len {
                    break;
                }
            }
            self.made += len;
            return Ok(());
        }
        let n = len.min(room);
        if offset >= n {
            let (made, rest) = self.buf.split_at_mut(self.made);
            copy_short(rest, &made[from..from + n]);
        } else if offset >= 8 {
            // Overlapping: words, each of bytes made before it.
            let to = self.made;
            let mut at = 0;
            while at + 8 <= n {
                let word = u64_at(&self.buf[from + at..from + at + 8], 0);
                self.buf[to + at..to + at + 8].copy_from_slice(&word.to_le_bytes());
                at += 8;
            }
            for at in at..n {
                self.buf[to + at] = self.buf[from + at];
            }
        } else {
            for at in 0..n {
                self.buf[self.made + at] = self.buf[from + at];
            }
        }
        self.made += n;
        if n < len { Err(Stop::Full) } else { Ok(()) }
    }
}

impl Zstd {
    /// Tables and buffers for decoding, on the heap.
    pub fn new() -> core::result::Result<Zstd, NoMemory> {
        Ok(Zstd {
            literals: crate::filled(LITERALS, 0)?,
            huffman: crate::filled(1 << HUFFMAN_MAX_BITS, 0)?,
            huffman_bits: 0,
            ll: Fse::new()?,
            of: Fse::new()?,
            ml: Fse::new()?,
            seqs: crate::filled(3 * STATES, 0)?,
            batch: crate::filled(BATCH, [0; 3])?,
        })
    }

    /// Decodes the frame at the start of `input` into `out`; returns how
    /// many bytes it made, `out.len()` when they fill it.
    pub fn decompress(&mut self, input: &[u8], out: &mut [u8]) -> Result<usize> {
        let len = out.len();
        match self.frame(input, out) {
            Ok(made) => Ok(made),
            Err(Stop::Full) => Ok(len),
            Err(Stop::Bad) => Err(Error::Io),
        }
    }

    fn frame(&mut self, input: &[u8], out: &mut [u8]) -> Step<usize> {
        if le(input, 0, 4)? != u64::from(MAGIC) {
            return Err(Stop::Bad);
        }
        let descriptor = *input.get(4).ok_or(Stop::Bad)?;
        let single = descriptor & 0x20 != 0;
        let checksum = descriptor & 0x04 != 0;
        // A reserved bit.
        if descriptor & 0x08 != 0 {
            return Err(Stop::Bad);
        }
        // The window descriptor, which the whole output stands for.
        let mut at = if single { 5 } else { 6 };
        let dict = [0, 1, 2, 4][usize::from(descriptor & 3)];
        if le(input, at, dict)? != 0 {
            return Err(Stop::Bad);
        }
        at += dict;
        let size_len = match descriptor >> 6 {
            0 => usize::from(single),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let content = match size_len {
            0 => None,
            2 => Some(le(input, at, 2)? + 256),
            n => Some(le(input, at, n)?),
        };
        at += size_len;
        self.huffman_bits = 0;
        self.ll.ready = false;
        self.of.ready = false;
        self.ml.ready = false;
        let mut repeats = [1, 4, 8];
        let mut o = Output { buf: out, made: 0 };
        loop {
            let header = le(input, at, 3)? as usize;
            at += 3;
            let size = header >> 3;
            if size > MAX_BLOCK {
                return Err(Stop::Bad);
            }
            match (header >> 1) & 3 {
                0 => {
                    o.put(get(input, at, size)?)?;
                    at += size;
                }
                1 => {
                    o.fill(*input.get(at).ok_or(Stop::Bad)?, size)?;
                    at += 1;
                }
                2 => {
                    self.block(get(input, at, size)?, &mut o, &mut repeats)?;
                    at += size;
                }
                _ => return Err(Stop::Bad),
            }
            if header & 1 != 0 {
                break;
            }
        }
        if checksum && le(input, at, 4)? != xxh64(&o.buf[..o.made], 0) & 0xffff_ffff {
            return Err(Stop::Bad);
        }
        if content.is_some_and(|n| n != o.made as u64) {
            return Err(Stop::Bad);
        }
        Ok(o.made)
    }

    /// Decodes the compressed block `data` into `o`.
    fn block(&mut self, data: &[u8], o: &mut Output<'_>, repeats: &mut [usize; 3]) -> Step<()> {
        let Zstd { literals, huffman, huffman_bits, ll, of, ml, seqs, batch } = self;
        let (lits_len, rest) = read_literals(data, literals, huffman, huffman_bits)?;
        let lits = <&[u8; LITERALS]>::try_from(&literals[..]).map_err(|_| Stop::Bad)?;
        let codes = [(ll, Kind::Literals), (of, Kind::Offsets), (ml, Kind::Matches)];
        let batch = <&mut [[usize; 3]; BATCH]>::try_from(&mut batch[..]).map_err(|_| Stop::Bad)?;
        let seqs = <&mut [u64; 3 * STATES]>::try_from(&mut seqs[..]).map_err(|_| Stop::Bad)?;
        sequences(rest, (lits, lits_len), (codes, seqs), batch, o, repeats)
    }
}

/// Reads a block's literals section into the start of `buf`; returns how
/// many there are and the bytes after it.
fn read_literals<'d>(
    data: &'d [u8],
    buf: &mut KVec<u8>,
    huffman: &mut KVec<u16>,
    huffman_bits: &mut u32,
) -> Step<(usize, &'d [u8])> {
    let first = *data.first().ok_or(Stop::Bad)?;
    let format = (first >> 2) & 3;
    match first & 3 {
        kind @ (0 | 1) => {
            let (size, header) = match format {
                0 | 2 => (usize::from(first >> 3), 1),
                1 => ((le(data, 0, 2)? >> 4) as usize, 2),
                _ => ((le(data, 0, 3)? >> 4) as usize, 3),
            };
            let lits = buf.get_mut(..size).filter(|_| size <= MAX_BLOCK).ok_or(Stop::Bad)?;
            if kind == 0 {
                // Copied, so that sequences find room after them as after
                // the others.
                lits.copy_from_slice(get(data, header, size)?);
                Ok((size, &data[header + size..]))
            } else {
                lits.fill(*data.get(header).ok_or(Stop::Bad)?);
                Ok((size, &data[header + 1..]))
            }
        }
        kind => {
            let (regenerated, compressed, header, streams) = match format {
                0 | 1 => {
                    let h = le(data, 0, 3)? as usize;
                    ((h >> 4) & 0x3ff, (h >> 14) & 0x3ff, 3, if format == 0 { 1 } else { 4 })
                }
                2 => {
                    let h = le(data, 0, 4)? as usize;
                    ((h >> 4) & 0x3fff, (h >> 18) & 0x3fff, 4, 4)
                }
                _ => {
                    let h = le(data, 0, 5)? as usize;
                    ((h >> 4) & 0x3ffff, (h >> 22) & 0x3ffff, 5, 4)
                }
            };
            let body = get(data, header, compressed)?;
            let streams_data = if kind == 2 {
                let used = read_huffman(body, huffman, huffman_bits)?;
                &body[used..]
            } else if *huffman_bits == 0 {
                // Literals coded with the last block's code, of which
                // there is none.
                return Err(Stop::Bad);
            } else {
                body
            };
            let lits =
                buf.get_mut(..regenerated).filter(|_| regenerated <= MAX_BLOCK).ok_or(Stop::Bad)?;
            huffman_literals(streams_data, streams, lits, huffman, *huffman_bits)?;
            Ok((regenerated, &data[header + compressed..]))
        }
    }
}

/// Reads a Huffman code's description (RFC 8878, 4.2.1) into `table`;
/// returns the bytes it took.
#[inline(never)]
fn read_huffman(body: &[u8], table: &mut [u16], max_bits: &mut u32) -> Step<usize> {
    let first = *body.first().ok_or(Stop::Bad)?;
    let mut weights = [0u8; 256];
    let (count, used) = if first < 128 {
        // Weights coded with FSE, in `first` bytes.
        let size = usize::from(first);
        (fse_weights(get(body, 1, size)?, &mut weights)?, 1 + size)
    } else {
        // Weights of 4 bits each.
        let count = usize::from(first - 127);
        let bytes = get(body, 1, count.div_ceil(2))?;
        for (i, weight) in weights[..count].iter_mut().enumerate() {
            let byte = bytes[i / 2];
            *weight = if i % 2 == 0 { byte >> 4 } else { byte & 15 };
        }
        (count, 1 + count.div_ceil(2))
    };
    // The last symbol's weight is what makes the shares a power of two.
    let mut total = 0u32;
    for &weight in &weights[..count] {
        if u32::from(weight) > HUFFMAN_MAX_BITS {
            return Err(Stop::Bad);
        }
        if weight > 0 {
            total += 1 << (weight - 1);
        }
    }
    if total == 0 || count >= weights.len() {
        return Err(Stop::Bad);
    }
    let bits = 32 - total.leading_zeros();
    let left = (1 << bits) - total;
    if bits > HUFFMAN_MAX_BITS || !left.is_power_of_two() {
        return Err(Stop::Bad);
    }
    weights[count] = (left.trailing_zeros() + 1) as u8;
    let weights = &weights[..=count];
    // Codes go to the lightest symbols first, each taking 2^(weight - 1)
    // entries of the table.
    let mut start = [0u32; HUFFMAN_MAX_BITS as usize + 2];
    for &weight in weights {
        if weight > 0 {
            start[usize::from(weight) + 1] += 1 << (weight - 1);
        }
    }
    for w in 1..start.len() {
        start[w] += start[w - 1];
    }
    for (sym, &weight) in weights.iter().enumerate() {
        if weight == 0 {
            continue;
        }
        let at = &mut start[usize::from(weight)];
        let n = 1u32 << (weight - 1);
        let entry = sym as u16 | ((bits + 1 - u32::from(weight)) as u16) << 8;
        table[*at as usize..(*at + n) as usize].fill(entry);
        *at += n;
    }
    *max_bits = bits;
    Ok(used)
}

/// Decodes the Huffman weights coded with FSE in `data` into `weights`;
/// returns how many it decoded.
#[inline(never)]
fn fse_weights(data: &[u8], weights: &mut [u8; 256]) -> Step<usize> {
    let mut shares = [0i16; 256];
    let (log, used) = read_shares(data, 6, &mut shares)?;
    let mut table = [0u32; 64];
    build(&shares, log, &mut table)?;
    let mut stream = Backward::new(&data[used..])?;
    // Two states take turns, the first decoding the even weights, until
    // the stream runs out.
    let mut states = [stream.read(log) as usize, stream.read(log) as usize];
    let mut count = 0;
    'decode: loop {
        for turn in 0..2 {
            *weights.get_mut(count).ok_or(Stop::Bad)? = symbol(table[states[turn]]) as u8;
            count += 1;
            states[turn] = next_state(&table, states[turn], &mut stream);
            if stream.left() < 0 {
                let other = states[1 - turn];
                *weights.get_mut(count).ok_or(Stop::Bad)? = symbol(table[other]) as u8;
                count += 1;
                break 'decode;
            }
        }
    }
    Ok(count)
}

/// Decodes `out.len()` literals from `streams` (1 or 4) Huffman streams.
fn huffman_literals(
    data: &[u8],
    streams: usize,
    out: &mut [u8],
    table: &[u16],
    bits: u32,
) -> Step<()> {
    if streams == 1 {
        return huffman_stream(data, out, table, bits);
    }
    // A jump table of the first three streams' sizes; each stream but
    // the last makes a quarter of the literals, rounded up.
    if data.len() < 10 || out.len() < 6 {
        return Err(Stop::Bad);
    }
    let sizes = [0, 2, 4].map(|at| usize::from(u16_at(data, at)));
    let last = (data.len() - 6).checked_sub(sizes.iter().sum()).ok_or(Stop::Bad)?;
    let quarter = out.len().div_ceil(4);
    if 3 * quarter > out.len() {
        return Err(Stop::Bad);
    }
    let [a, b, c] = sizes;
    let mut s0 = Backward::new(get(data, 6, a)?)?;
    let mut s1 = Backward::new(get(data, 6 + a, b)?)?;
    let mut s2 = Backward::new(get(data, 6 + a + b, c)?)?;
    let mut s3 = Backward::new(get(data, 6 + a + b + c, last)?)?;
    let (o0, rest) = out.split_at_mut(quarter);
    let (o1, rest) = rest.split_at_mut(quarter);
    let (o2, o3) = rest.split_at_mut(quarter);
    let table = huffman_table(table)?;
    // The four in turn, as far as the last goes, for the processor to
    // decode them at once; then the rest of the first three.
    let both = o3.len();
    for at in 0..both {
        o0[at] = huffman_symbol(&mut s0, table, bits);
        o1[at] = huffman_symbol(&mut s1, table, bits);
        o2[at] = huffman_symbol(&mut s2, table, bits);
        o3[at] = huffman_symbol(&mut s3, table, bits);
    }
    for (stream, out) in [(&mut s0, o0), (&mut s1, o1), (&mut s2, o2)] {
        for byte in &mut out[both..] {
            *byte = huffman_symbol(stream, table, bits);
        }
    }
    if [s0, s1, s2, s3].iter().any(|s| s.left() != 0) {
        return Err(Stop::Bad);
    }
    Ok(())
}

/// The Huffman table, as long as its longest code takes.
fn huffman_table(table: &[u16]) -> Step<&[u16; 1 << HUFFMAN_MAX_BITS]> {
    table.get(..1 << HUFFMAN_MAX_BITS).and_then(|t| t.try_into().ok()).ok_or(Stop::Bad)
}

/// The next symbol of a Huffman stream whose longest code takes `bits`.
#[inline(always)]
fn huffman_symbol(
    stream: &mut Backward<'_>,
    table: &[u16; 1 << HUFFMAN_MAX_BITS],
    bits: u32,
) -> u8 {
    let entry = table[stream.peek(bits) as usize & ((1 << HUFFMAN_MAX_BITS) - 1)];
    stream.skip(u32::from(entry >> 8));
    entry as u8
}

/// Decodes `out.len()` literals from one Huffman stream, which they must
/// use up.
fn huffman_stream(data: &[u8], out: &mut [u8], table: &[u16], bits: u32) -> Step<()> {
    let mut stream = Backward::new(data)?;
    let table = huffman_table(table)?;
    for byte in out.iter_mut() {
        *byte = huffman_symbol(&mut stream, table, bits);
    }
    if stream.left() != 0 {
        return Err(Stop::Bad);
    }
    Ok(())
}

/// Decodes a block's sequences section `data` and runs its sequences,
/// with the literals `lits` (and any bytes after them in their buffer),
/// into `o`.
fn sequences(
    data: &[u8],
    lits: (&[u8; LITERALS], usize),
    codes: ([(&mut Fse, Kind); 3], &mut [u64; 3 * STATES]),
    batch: &mut [[usize; 3]; BATCH],
    o: &mut Output<'_>,
    repeats: &mut [usize; 3],
) -> Step<()> {
    let (lits, lits_len) = lits;
    let first = *data.first().ok_or(Stop::Bad)?;
    let (count, mut at) = match first {
        0..=127 => (usize::from(first), 1),
        128..=254 => (usize::from(first - 128) << 8 | le(data, 1, 1)? as usize, 2),
        255 => (le(data, 1, 2)? as usize + 0x7f00, 3),
    };
    if count == 0 {
        // Only literals; the codes stay as they were.
        if data.len() != at {
            return Err(Stop::Bad);
        }
        return o.put(&lits[..lits_len]);
    }
    let modes = *data.get(at).ok_or(Stop::Bad)?;
    at += 1;
    if modes & 3 != 0 {
        return Err(Stop::Bad);
    }
    let ([(ll, ll_kind), (of, of_kind), (ml, ml_kind)], tables) = codes;
    let (ll_seq, rest) = tables.split_at_mut(STATES);
    let (of_seq, ml_seq) = rest.split_at_mut(STATES);
    at += ll.prepare(modes >> 6, &data[at..], ll_kind, ll_seq)?;
    at += of.prepare((modes >> 4) & 3, &data[at..], of_kind, of_seq)?;
    at += ml.prepare((modes >> 2) & 3, &data[at..], ml_kind, ml_seq)?;
    let tables: &[u64; 3 * STATES] = tables;
    let mut stream = Backward::new(&data[at..])?;
    // States are below the size of their tables, which is at most
    // `STATES`; masked, they need no other check.
    let ll_state = stream.read(ll.log) as usize;
    let of_state = stream.read(of.log) as usize;
    let ml_state = stream.read(ml.log) as usize;
    let mut states = [ll_state, of_state, ml_state];
    let mut lit = 0;
    let mut left = count;
    while left != 0 {
        let n = left.min(BATCH);
        left -= n;
        let batch = &mut batch[..n];
        decode(&mut stream, tables, &mut states, repeats, batch, left != 0);
        lit = execute(batch, (lits, lits_len), lit, o)?;
    }
    if stream.left() != 0 {
        return Err(Stop::Bad);
    }
    o.put(&lits[lit..lits_len])
}

/// Decodes `batch.len()` sequences from `stream` into `batch`, each its
/// literals' length, its match's length and its offset, with the codes'
/// `tables` from `states` on; the last moves the states on only when
/// `more` follow it.
#[inline(never)]
fn decode(
    stream: &mut Backward<'_>,
    tables: &[u64; 3 * STATES],
    states: &mut [usize; 3],
    repeats: &mut [usize; 3],
    batch: &mut [[usize; 3]],
    more: bool,
) {
    let [mut ll_state, mut of_state, mut ml_state] = *states;
    let [mut r0, mut r1, mut r2] = *repeats;
    let mut entries = (0, 0, 0);
    for slot in batch {
        // States are below the size of their tables, which is at most
        // `STATES`; masked, they need no other check.
        let (l, f, m) = (
            tables[ll_state % STATES],
            tables[STATES + of_state % STATES],
            tables[2 * STATES + ml_state % STATES],
        );
        entries = (l, f, m);
        // Up to 31 bits for the offset, 16 each for the lengths and 26
        // for the states: the window is refilled where they could run
        // past its 64, which they seldom do.
        stream.refill();
        let value = (f >> 32) as usize + stream.take(extra(f)) as usize;
        if stream.used > 32 {
            stream.refill();
        }
        let ml_len = (m >> 32) as usize + stream.take(extra(m)) as usize;
        let ll_len = (l >> 32) as usize + stream.take(extra(l)) as usize;
        let offset = if value > 3 {
            (r0, r1, r2) = (value - 3, r0, r1);
            r0
        } else {
            // The last three offsets: with no literals, the next one, and
            // the last less one byte after the third.
            match value - 1 + usize::from(ll_len == 0) {
                0 => r0,
                1 => {
                    (r0, r1) = (r1, r0);
                    r0
                }
                2 => {
                    (r0, r1, r2) = (r2, r0, r1);
                    r0
                }
                _ => {
                    (r0, r1, r2) = (r0.wrapping_sub(1), r0, r1);
                    r0
                }
            }
        };
        if stream.used > 38 {
            stream.refill();
        }
        ll_state = state(l) + stream.take(state_bits(l)) as usize;
        ml_state = state(m) + stream.take(state_bits(m)) as usize;
        of_state = state(f) + stream.take(state_bits(f)) as usize;
        *slot = [ll_len, ml_len, offset];
    }
    if !more {
        // The last sequence has no next states: their bits are not read.
        let (l, f, m) = entries;
        stream.used -= state_bits(l) + state_bits(m) + state_bits(f);
    }
    *states = [ll_state, of_state, ml_state];
    *repeats = [r0, r1, r2];
}

/// Makes the copies of the sequences of `batch` into `o`, their literals
/// from `lit` on of `lits`; returns where the next literals start.
#[inline(never)]
fn execute(
    batch: &[[usize; 3]],
    lits: (&[u8; LITERALS], usize),
    lit: usize,
    o: &mut Output<'_>,
) -> Step<usize> {
    let (lits, lits_len) = lits;
    let (mut lit, mut made) = (lit, o.made);
    for &[ll_len, ml_len, offset] in batch {
        let (lit_end, to) = (lit + ll_len, made + ll_len);
        if (ll_len | ml_len) < 16
            && lit_end <= lits_len
            && to + 32 <= o.buf.len()
            && (16..=to).contains(&offset)
        {
            // Most sequences: their literals and their match are a copy of
            // 16 bytes each, which may run past them.
            o.buf[made..made + 16].copy_from_slice(&lits[lit..lit + 16]);
            let (before, after) = o.buf.split_at_mut(to);
            after[..16].copy_from_slice(&before[to - offset..to - offset + 16]);
            made = to + ml_len;
        } else {
            if lit_end > lits_len {
                return Err(Stop::Bad);
            }
            o.made = made;
            o.put_from(lits, lit, ll_len)?;
            o.copy(offset, ml_len)?;
            made = o.made;
        }
        lit = lit_end;
    }
    o.made = made;
    Ok(lit)
}

/// The parts of a [`Fse::seq`] entry: the next state's base, its bits,
/// and the value's extra bits.
fn state(entry: u64) -> usize {
    (entry & 0xffff) as usize
}

fn state_bits(entry: u64) -> u32 {
    ((entry >> 16) & 0xff) as u32
}

fn extra(entry: u64) -> u32 {
    ((entry >> 24) & 0xff) as u32
}

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

fn merge(acc: u64, lane: u64) -> u64 {
    (acc ^ round(0, lane)).wrapping_mul(P1).wrapping_add(P4)
}

/// XXH64 of `data` with `seed`, as the xxHash specification defines it:
/// the checksum of a frame is its low 32 bits, of seed 0.
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let mut rest = data;
    let mut h = if data.len() >= 32 {
        let mut lanes = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        while rest.len() >= 32 {
            for (i, lane) in lanes.iter_mut().enumerate() {
                *lane = round(*lane, u64_at(rest, i * 8));
            }
            rest = &rest[32..];
        }
        let mut h = lanes[0]
            .rotate_left(1)
            .wrapping_add(lanes[1].rotate_left(7))
            .wrapping_add(lanes[2].rotate_left(12))
            .wrapping_add(lanes[3].rotate_left(18));
        for lane in lanes {
            h = merge(h, lane);
        }
        h
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(data.len() as u64);
    while rest.len() >= 8 {
        h ^= round(0, u64_at(rest, 0));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        rest = &rest[8..];
    }
    if rest.len() >= 4 {
        h ^= u64::from(u32_at(rest, 0)).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        rest = &rest[4..];
    }
    for &byte in rest {
        h ^= u64::from(byte).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ h >> 32
}
