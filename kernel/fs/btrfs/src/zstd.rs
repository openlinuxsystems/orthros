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

/// The magic number that starts a frame.
const MAGIC: u32 = 0xFD2F_B528;

/// The most a block holds, compressed or not.
pub const MAX_BLOCK: usize = 128 * 1024;

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

/// A stream read backwards, from its end: Huffman and FSE data. `left`
/// is the bits not yet read; reading past the start reads zeros and
/// takes it below zero.
struct Backward<'a> {
    data: &'a [u8],
    left: isize,
}

impl<'a> Backward<'a> {
    /// The stream `data`, whose last byte's highest set bit marks its end.
    fn new(data: &'a [u8]) -> Step<Backward<'a>> {
        let last = *data.last().ok_or(Stop::Bad)?;
        if last == 0 {
            return Err(Stop::Bad);
        }
        let left = data.len() * 8 - last.leading_zeros() as usize - 1;
        Ok(Backward { data, left: left as isize })
    }

    /// Eight bytes from `at`, zero past the end.
    fn word(&self, at: usize) -> u64 {
        match self.data.get(at..at + 8) {
            Some(bytes) => u64_at(bytes, 0),
            None => {
                let mut word = 0;
                for (i, &b) in self.data.iter().skip(at).take(8).enumerate() {
                    word |= u64::from(b) << (8 * i);
                }
                word
            }
        }
    }

    /// The next `n` bits (up to 56): the first read is the highest.
    fn peek(&self, n: u32) -> u64 {
        if n == 0 || self.left <= 0 {
            return 0;
        }
        let low = self.left - n as isize;
        if low >= 0 {
            let low = low as usize;
            (self.word(low / 8) >> (low % 8)) & ((1 << n) - 1)
        } else {
            (self.word(0) & ((1 << self.left) - 1)) << -low
        }
    }

    fn skip(&mut self, n: u32) {
        self.left -= n as isize;
    }

    fn read(&mut self, n: u32) -> u64 {
        let value = self.peek(n);
        self.skip(n);
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
fn next_state(table: &[u32], state: usize, stream: &mut Backward<'_>) -> usize {
    let entry = table[state];
    (entry >> 16) as usize + stream.read((entry >> 8) & 0xff) as usize
}

impl Fse {
    fn new() -> core::result::Result<Fse, NoMemory> {
        Ok(Fse { table: crate::filled(1 << 9, 0)?, log: 0, ready: false })
    }

    /// Sets the code for a block's sequences from its mode and the bytes
    /// after it; returns how many it used.
    fn prepare(&mut self, mode: u8, data: &[u8], kind: Kind) -> Step<usize> {
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
                0
            }
        };
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
    /// A block's literals, decoded.
    literals: KVec<u8>,
    /// The Huffman code of literals: for each value of its next bits, the
    /// symbol and its code's length (`symbol | length << 8`).
    huffman: KVec<u16>,
    /// The longest code of the Huffman code; 0 for none in this frame.
    huffman_bits: u32,
    ll: Fse,
    of: Fse,
    ml: Fse,
}

/// The output buffer, and how much of it is made.
struct Output<'o> {
    buf: &'o mut [u8],
    made: usize,
}

impl Output<'_> {
    fn put(&mut self, bytes: &[u8]) -> Step<()> {
        let room = self.buf.len() - self.made;
        let n = bytes.len().min(room);
        self.buf[self.made..self.made + n].copy_from_slice(&bytes[..n]);
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

    /// Copies `len` bytes from `offset` back, overlapping as they may.
    fn copy(&mut self, offset: usize, len: usize) -> Step<()> {
        if offset == 0 || offset > self.made {
            return Err(Stop::Bad);
        }
        let room = self.buf.len() - self.made;
        let n = len.min(room);
        let from = self.made - offset;
        if offset >= n {
            self.buf.copy_within(from..from + n, self.made);
        } else {
            for i in 0..n {
                self.buf[self.made + i] = self.buf[from + i];
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
            literals: crate::filled(MAX_BLOCK, 0)?,
            huffman: crate::filled(1 << HUFFMAN_MAX_BITS, 0)?,
            huffman_bits: 0,
            ll: Fse::new()?,
            of: Fse::new()?,
            ml: Fse::new()?,
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
        let Zstd { literals, huffman, huffman_bits, ll, of, ml } = self;
        let (lits, rest) = read_literals(data, literals, huffman, huffman_bits)?;
        let codes = [(ll, Kind::Literals), (of, Kind::Offsets), (ml, Kind::Matches)];
        sequences(rest, lits, codes, o, repeats)
    }
}

/// Reads a block's literals section; returns its literals and the bytes
/// after it.
fn read_literals<'d>(
    data: &'d [u8],
    buf: &'d mut KVec<u8>,
    huffman: &mut KVec<u16>,
    huffman_bits: &mut u32,
) -> Step<(&'d [u8], &'d [u8])> {
    let first = *data.first().ok_or(Stop::Bad)?;
    let format = (first >> 2) & 3;
    match first & 3 {
        kind @ (0 | 1) => {
            let (size, header) = match format {
                0 | 2 => (usize::from(first >> 3), 1),
                1 => ((le(data, 0, 2)? >> 4) as usize, 2),
                _ => ((le(data, 0, 3)? >> 4) as usize, 3),
            };
            if kind == 0 {
                Ok((get(data, header, size)?, &data[header + size..]))
            } else {
                let byte = *data.get(header).ok_or(Stop::Bad)?;
                let lits = buf.get_mut(..size).ok_or(Stop::Bad)?;
                lits.fill(byte);
                let buf: &'d KVec<u8> = buf;
                Ok((&buf[..size], &data[header + 1..]))
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
            let lits = buf.get_mut(..regenerated).ok_or(Stop::Bad)?;
            huffman_literals(streams_data, streams, lits, huffman, *huffman_bits)?;
            let buf: &'d KVec<u8> = buf;
            Ok((&buf[..regenerated], &data[header + compressed..]))
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
            if stream.left < 0 {
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
    let mut at = 6;
    for (i, size) in sizes.into_iter().chain([last]).enumerate() {
        let from = (i * quarter).min(out.len());
        let to = if i == 3 { out.len() } else { (from + quarter).min(out.len()) };
        huffman_stream(get(data, at, size)?, &mut out[from..to], table, bits)?;
        at += size;
    }
    Ok(())
}

/// Decodes `out.len()` literals from one Huffman stream, which they must
/// use up.
fn huffman_stream(data: &[u8], out: &mut [u8], table: &[u16], bits: u32) -> Step<()> {
    let mut stream = Backward::new(data)?;
    for byte in out.iter_mut() {
        let entry = table[stream.peek(bits) as usize];
        *byte = entry as u8;
        stream.skip(u32::from(entry >> 8));
    }
    if stream.left != 0 {
        return Err(Stop::Bad);
    }
    Ok(())
}

/// Decodes a block's sequences section `data` and runs its sequences,
/// with the literals `lits`, into `o`.
fn sequences(
    data: &[u8],
    lits: &[u8],
    codes: [(&mut Fse, Kind); 3],
    o: &mut Output<'_>,
    repeats: &mut [usize; 3],
) -> Step<()> {
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
        return o.put(lits);
    }
    let modes = *data.get(at).ok_or(Stop::Bad)?;
    at += 1;
    if modes & 3 != 0 {
        return Err(Stop::Bad);
    }
    let [(ll, ll_kind), (of, of_kind), (ml, ml_kind)] = codes;
    at += ll.prepare(modes >> 6, &data[at..], ll_kind)?;
    at += of.prepare((modes >> 4) & 3, &data[at..], of_kind)?;
    at += ml.prepare((modes >> 2) & 3, &data[at..], ml_kind)?;
    let mut stream = Backward::new(&data[at..])?;
    let (ll_table, of_table, ml_table) = (&ll.table[..], &of.table[..], &ml.table[..]);
    let mut ll_state = stream.read(ll.log) as usize;
    let mut of_state = stream.read(of.log) as usize;
    let mut ml_state = stream.read(ml.log) as usize;
    let mut lit = 0;
    for i in 0..count {
        let of_code = symbol(of_table[of_state]) as u32;
        let ml_code = symbol(ml_table[ml_state]);
        let ll_code = symbol(ll_table[ll_state]);
        let value = (1usize << of_code) + stream.read(of_code) as usize;
        let ml_len = (ML_BASE[ml_code] + stream.read(u32::from(ML_BITS[ml_code])) as u32) as usize;
        let ll_len = (LL_BASE[ll_code] + stream.read(u32::from(LL_BITS[ll_code])) as u32) as usize;
        let offset = if value > 3 {
            let offset = value - 3;
            *repeats = [offset, repeats[0], repeats[1]];
            offset
        } else {
            // The last three offsets: with no literals, the next one, and
            // the last less one byte after the third.
            match value - 1 + usize::from(ll_len == 0) {
                0 => repeats[0],
                1 => {
                    repeats.swap(0, 1);
                    repeats[0]
                }
                2 => {
                    *repeats = [repeats[2], repeats[0], repeats[1]];
                    repeats[0]
                }
                _ => {
                    let offset = repeats[0] - 1;
                    *repeats = [offset, repeats[0], repeats[1]];
                    offset
                }
            }
        };
        o.put(get(lits, lit, ll_len)?)?;
        lit += ll_len;
        o.copy(offset, ml_len)?;
        if i + 1 < count {
            ll_state = next_state(ll_table, ll_state, &mut stream);
            ml_state = next_state(ml_table, ml_state, &mut stream);
            of_state = next_state(of_table, of_state, &mut stream);
        }
    }
    if stream.left != 0 {
        return Err(Stop::Bad);
    }
    o.put(&lits[lit..])
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
