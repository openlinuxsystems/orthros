// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! zlib streams (RFC 1950) of DEFLATE data (RFC 1951), in which Btrfs
//! keeps extents compressed with zlib, inflated into a buffer.
//!
//! They are read as Linux reads them: the zlib header must be valid and
//! name no preset dictionary, the data ends with its last block, and the
//! Adler-32 after it, which Linux does not check, is not read. Output
//! that would pass the end of the buffer ends the inflation, with the
//! buffer full: the extent holds no more than that.
//!
//! Huffman codes are decoded through a table of their first ten bits,
//! which holds every code that short; a longer code goes on from there a
//! bit at a time, in canonical order.

use radix_vfs::mem::{self, KVec, NoMemory};
use radix_vfs::{Error, Result};

/// The most bits a code has.
const MAX_BITS: usize = 15;

/// The bits the fast table looks up.
const FAST_BITS: u32 = 10;

/// Literal and length codes, distance codes, and code length codes.
const LIT_CODES: usize = 288;
const DIST_CODES: usize = 32;
const LEN_CODES: usize = 19;

/// The order in which a dynamic block lists its code length codes.
const LEN_ORDER: [u8; LEN_CODES] =
    [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// The lengths of length codes 257 to 285, and their extra bits.
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];

/// The distances of distance codes 0 to 29, and their extra bits.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The end of a block, among the literal and length codes.
const END_OF_BLOCK: u16 = 256;

/// A bad stream: Linux's read of it fails with `EIO`.
fn bad<T>() -> Result<T> {
    Err(Error::Io)
}

/// The input, read a bit at a time from the low bit of each byte up.
struct Bits<'a> {
    data: &'a [u8],
    /// The next byte to load.
    at: usize,
    /// Bits loaded and not yet used, from the low end.
    buf: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Bits<'a> {
        Bits { data, at: 0, buf: 0, count: 0 }
    }

    /// Loads bytes until 56 bits are held or the input ends.
    fn fill(&mut self) {
        while self.count <= 56 {
            let Some(&byte) = self.data.get(self.at) else { return };
            self.buf |= u64::from(byte) << self.count;
            self.at += 1;
            self.count += 8;
        }
    }

    /// The next `n` bits (up to 32), zero past the end of the input.
    fn peek(&mut self, n: u32) -> u32 {
        if self.count < n {
            self.fill();
        }
        (self.buf & ((1u64 << n) - 1)) as u32
    }

    /// Uses `n` bits, which must be there.
    fn consume(&mut self, n: u32) -> Result<()> {
        if self.count < n {
            self.fill();
            if self.count < n {
                return bad();
            }
        }
        self.buf >>= n;
        self.count -= n;
        Ok(())
    }

    /// The next `n` bits (up to 32).
    fn take(&mut self, n: u32) -> Result<u32> {
        let value = self.peek(n);
        self.consume(n)?;
        Ok(value)
    }

    /// Drops the bits up to the next byte.
    fn align(&mut self) {
        let n = self.count % 8;
        self.buf >>= n;
        self.count -= n;
    }
}

/// A canonical Huffman code: symbols by code, and the fast table.
struct Code {
    /// How many codes have each length.
    count: [u16; MAX_BITS + 1],
    /// The symbols, in the order of their codes.
    symbol: KVec<u16>,
    /// For each value of the next ten bits, the symbol whose code they
    /// start with and its length (`symbol << 4 | length`), or 0 when the
    /// code is longer, or there is none.
    fast: KVec<u16>,
    /// Where the canonical walk stands after ten bits: the first code of
    /// eleven bits, and the index of its symbol.
    first: u32,
    index: u32,
}

impl Code {
    fn new(symbols: usize) -> core::result::Result<Code, NoMemory> {
        let mut symbol = mem::vec_with_capacity(symbols)?;
        let mut fast = mem::vec_with_capacity(1 << FAST_BITS)?;
        for _ in 0..symbols {
            let _ = symbol.push_within_capacity(0);
        }
        for _ in 0..1 << FAST_BITS {
            let _ = fast.push_within_capacity(0);
        }
        Ok(Code { count: [0; MAX_BITS + 1], symbol, fast, first: 0, index: 0 })
    }

    /// Makes the code whose symbols have the bit lengths `lengths` (0:
    /// unused). Fails, as zlib does, when the lengths ask for more codes
    /// than there are, or for fewer, unless the code has one code of one
    /// bit or none at all.
    fn build(&mut self, lengths: &[u8]) -> Result<()> {
        self.count = [0; MAX_BITS + 1];
        for &len in lengths {
            self.count[usize::from(len)] += 1;
        }
        self.count[0] = 0;
        let mut left: i32 = 1;
        let mut longest = 0;
        for (len, &count) in self.count.iter().enumerate().skip(1) {
            left = (left << 1) - i32::from(count);
            if left < 0 {
                return bad();
            }
            if count > 0 {
                longest = len;
            }
        }
        if left > 0 && longest > 1 {
            return bad();
        }
        // The symbols in code order: by length, then by symbol.
        let mut offset = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offset[len + 1] = offset[len] + self.count[len];
        }
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let at = &mut offset[usize::from(len)];
                self.symbol[usize::from(*at)] = sym as u16;
                *at += 1;
            }
        }
        // Every code of up to ten bits, with each value of the bits after
        // it, in the fast table. Codes are read from their high bit, and
        // the input from its low one, so the table is indexed by each
        // code's bits reversed.
        self.fast.fill(0);
        let (mut code, mut index) = (0u32, 0usize);
        for len in 1..=FAST_BITS as usize {
            for _ in 0..self.count[len] {
                let entry = self.symbol[index] << 4 | len as u16;
                let reversed = code.reverse_bits() >> (32 - len);
                for slot in (reversed as usize..1 << FAST_BITS).step_by(1 << len) {
                    self.fast[slot] = entry;
                }
                code += 1;
                index += 1;
            }
            code <<= 1;
        }
        let (mut first, mut index) = (0u32, 0u32);
        for &count in &self.count[1..=FAST_BITS as usize] {
            first = (first + u32::from(count)) << 1;
            index += u32::from(count);
        }
        self.first = first;
        self.index = index;
        Ok(())
    }

    /// The next symbol of `bits`.
    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16> {
        let ten = bits.peek(FAST_BITS);
        let entry = self.fast[ten as usize];
        if entry != 0 {
            bits.consume(u32::from(entry & 15))?;
            return Ok(entry >> 4);
        }
        // Longer than ten bits: on from the eleventh, as the canonical
        // order has it.
        bits.consume(FAST_BITS)?;
        let mut code = (ten.reverse_bits() >> (32 - FAST_BITS)) << 1;
        let (mut first, mut index) = (self.first, self.index);
        for len in FAST_BITS as usize + 1..=MAX_BITS {
            code |= bits.take(1)?;
            let count = u32::from(self.count[len]);
            if code >= first && code - first < count {
                return Ok(self.symbol[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        bad()
    }
}

/// What inflating a stream needs, kept between streams.
pub struct Inflate {
    lit: Code,
    dist: Code,
    lens: Code,
    /// The bit lengths of a block's codes.
    lengths: KVec<u8>,
}

/// How a block's data ended.
enum End {
    /// At its end-of-block code.
    Block,
    /// With the output full.
    Full,
}

impl Inflate {
    /// Tables for inflating, on the heap.
    pub fn new() -> core::result::Result<Inflate, NoMemory> {
        Ok(Inflate {
            lit: Code::new(LIT_CODES)?,
            dist: Code::new(DIST_CODES)?,
            lens: Code::new(LEN_CODES)?,
            lengths: crate::filled(LIT_CODES + DIST_CODES, 0)?,
        })
    }

    /// Inflates the zlib stream `input` into `out`; returns how many
    /// bytes it made, `out.len()` when they fill it.
    pub fn zlib(&mut self, input: &[u8], out: &mut [u8]) -> Result<usize> {
        // CMF and FLG: deflate (8), a window of up to 32 KiB, a check
        // that holds, and no preset dictionary.
        let [cmf, flg, ..] = *input else { return bad() };
        if cmf & 0x0f != 8 || cmf >> 4 > 7 || (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
            return bad();
        }
        if flg & 0x20 != 0 {
            return bad();
        }
        self.raw(&input[2..], out)
    }

    /// Inflates the DEFLATE data `input` into `out`.
    pub fn raw(&mut self, input: &[u8], out: &mut [u8]) -> Result<usize> {
        let mut bits = Bits::new(input);
        let mut made = 0;
        loop {
            let last = bits.take(1)? == 1;
            let end = match bits.take(2)? {
                0 => stored(&mut bits, out, &mut made)?,
                1 => {
                    self.fixed()?;
                    self.codes(&mut bits, out, &mut made)?
                }
                2 => {
                    self.dynamic(&mut bits)?;
                    self.codes(&mut bits, out, &mut made)?
                }
                _ => return bad(),
            };
            if matches!(end, End::Full) || last {
                return Ok(made);
            }
        }
    }

    /// The codes of a block of fixed codes.
    fn fixed(&mut self) -> Result<()> {
        let (lit, dist) = self.lengths.split_at_mut(LIT_CODES);
        lit[..144].fill(8);
        lit[144..256].fill(9);
        lit[256..280].fill(7);
        lit[280..].fill(8);
        dist[..DIST_CODES].fill(5);
        self.lit.build(lit)?;
        self.dist.build(&dist[..DIST_CODES])
    }

    /// Reads the codes of a block of dynamic codes.
    fn dynamic(&mut self, bits: &mut Bits<'_>) -> Result<()> {
        let lits = bits.take(5)? as usize + 257;
        let dists = bits.take(5)? as usize + 1;
        let lens = bits.take(4)? as usize + 4;
        if lits > 286 || dists > 30 {
            return bad();
        }
        let mut len_lengths = [0u8; LEN_CODES];
        for &sym in &LEN_ORDER[..lens] {
            len_lengths[usize::from(sym)] = bits.take(3)? as u8;
        }
        // The code lengths' own code must be complete.
        self.lens.build(&len_lengths)?;
        if self.lens.count.iter().map(|&n| u32::from(n)).sum::<u32>() == 1 {
            return bad();
        }
        let all = lits + dists;
        let mut at = 0;
        while at < all {
            let sym = self.lens.decode(bits)?;
            let (value, times) = match sym {
                0..=15 => (sym as u8, 1),
                16 => {
                    let Some(&previous) = at.checked_sub(1).map(|i| &self.lengths[i]) else {
                        return bad();
                    };
                    (previous, 3 + bits.take(2)? as usize)
                }
                17 => (0, 3 + bits.take(3)? as usize),
                _ => (0, 11 + bits.take(7)? as usize),
            };
            let Some(run) = self.lengths.get_mut(at..at + times).filter(|_| at + times <= all)
            else {
                return bad();
            };
            run.fill(value);
            at += times;
        }
        // A block must be able to end.
        if self.lengths[usize::from(END_OF_BLOCK)] == 0 {
            return bad();
        }
        let (lit, dist) = self.lengths[..all].split_at(lits);
        self.lit.build(lit)?;
        self.dist.build(dist)
    }

    /// Inflates a block's coded data into `out` from `made`.
    fn codes(&self, bits: &mut Bits<'_>, out: &mut [u8], made: &mut usize) -> Result<End> {
        loop {
            let sym = self.lit.decode(bits)?;
            if sym < END_OF_BLOCK {
                let Some(byte) = out.get_mut(*made) else { return Ok(End::Full) };
                *byte = sym as u8;
                *made += 1;
                continue;
            }
            if sym == END_OF_BLOCK {
                return Ok(End::Block);
            }
            let code = usize::from(sym - 257);
            if code >= LEN_BASE.len() {
                return bad();
            }
            let len = usize::from(LEN_BASE[code]) + bits.take(u32::from(LEN_EXTRA[code]))? as usize;
            let code = usize::from(self.dist.decode(bits)?);
            if code >= DIST_BASE.len() {
                return bad();
            }
            let dist =
                usize::from(DIST_BASE[code]) + bits.take(u32::from(DIST_EXTRA[code]))? as usize;
            if dist > *made {
                return bad();
            }
            if copy_match(out, made, dist, len) {
                return Ok(End::Full);
            }
        }
    }
}

/// Copies `len` bytes from `dist` back to `made` in `out`, overlapping as
/// they may; returns whether the output filled first.
fn copy_match(out: &mut [u8], made: &mut usize, dist: usize, len: usize) -> bool {
    let room = out.len() - *made;
    let n = len.min(room);
    let from = *made - dist;
    if dist >= n {
        out.copy_within(from..from + n, *made);
    } else {
        for i in 0..n {
            out[*made + i] = out[from + i];
        }
    }
    *made += n;
    n < len
}

/// Copies a stored block into `out` from `made`.
fn stored(bits: &mut Bits<'_>, out: &mut [u8], made: &mut usize) -> Result<End> {
    bits.align();
    let len = bits.take(16)?;
    if bits.take(16)? != !len & 0xffff {
        return bad();
    }
    for _ in 0..len {
        let byte = bits.take(8)? as u8;
        let Some(slot) = out.get_mut(*made) else { return Ok(End::Full) };
        *slot = byte;
        *made += 1;
    }
    Ok(End::Block)
}
