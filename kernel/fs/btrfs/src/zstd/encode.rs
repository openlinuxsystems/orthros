// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Zstandard frames made from an extent's data (RFC 8878), as Linux's
//! zstd makes them at level 1, its `fast` strategy, for a Btrfs mounted to
//! compress.
//!
//! A frame holds one compressed block of all the data, at most
//! [`MAX_BLOCK`], and its size. Matches are found through a table of the
//! last place each hash of five bytes was seen, after a check for the
//! last offset again one byte on; a search that finds nothing takes
//! longer strides the longer it goes, so data that does not compress
//! costs little. After a match the offset before last is tried at once.
//! Literals are Huffman-coded, in four streams, when that saves room and
//! no literal is above 128 (so the code is described by its weights, four
//! bits each); otherwise they are kept as they are, or as one byte
//! repeated. Sequences use the codes the format predefines, which need no
//! description. A frame larger than the caller's limit is not made: the
//! extent is written uncompressed.

use radix_vfs::mem::{self, KVec, NoMemory};

use super::{
    HUFFMAN_MAX_BITS, Kind, LL_BASE, LL_BITS, MAGIC, MAX_BLOCK, ML_BASE, ML_BITS, copy_short,
};

/// The bits of a hash of six bytes, which index the table of matches.
const HASH_LOG: u32 = 14;

/// The shortest match taken.
const MIN_MATCH: usize = 4;

/// How near the data's end matches are no longer looked for: a hash reads
/// eight bytes.
const TAIL: usize = 8;

/// The fewest literals that are worth a Huffman code.
const MIN_HUFFMAN: usize = 64;

/// The most sequences a block has: each covers a match.
const MAX_SEQUENCES: usize = MAX_BLOCK / MIN_MATCH + 1;

/// The frame's header: its magic number, its descriptor (one segment, a
/// content size of four bytes) and that size.
const FRAME_HEADER: usize = 9;

/// A block's header.
const BLOCK_HEADER: usize = 3;

/// A run of literals and the match after it.
#[derive(Clone, Copy, Default)]
struct Sequence {
    literals: u32,
    /// The offset as the format codes it: 1 for the last repeated, or
    /// the offset plus 3.
    offset: u32,
    len: u32,
}

/// The code of each literal length below 64, from [`LL_BASE`].
const LL_CODE: [u8; 64] = {
    let mut codes = [0; 64];
    let (mut len, mut code) = (0, 0);
    while len < 64 {
        while code + 1 < LL_BASE.len() && LL_BASE[code + 1] as usize <= len {
            code += 1;
        }
        codes[len] = code as u8;
        len += 1;
    }
    codes
};

/// The code of each match length less 3 below 128, from [`ML_BASE`].
const ML_CODE: [u8; 128] = {
    let mut codes = [0; 128];
    let (mut len, mut code) = (0, 0);
    while len < 128 {
        while code + 1 < ML_BASE.len() && ML_BASE[code + 1] as usize - 3 <= len {
            code += 1;
        }
        codes[len] = code as u8;
        len += 1;
    }
    codes
};

fn ll_code(len: u32) -> usize {
    if len < 64 { usize::from(LL_CODE[len as usize]) } else { len.ilog2() as usize + 19 }
}

fn ml_code(len: u32) -> usize {
    let base = len - 3;
    if base < 128 { usize::from(ML_CODE[base as usize]) } else { base.ilog2() as usize + 36 }
}

/// `bits`, with zeros after them to 64.
const fn padded<const N: usize>(bits: &[u8; N]) -> [u8; 64] {
    let mut out = [0; 64];
    let mut i = 0;
    while i < N {
        out[i] = bits[i];
        i += 1;
    }
    out
}

/// The data compressed, with room after the most there is for a word
/// read at its last byte.
const WINDOW: usize = MAX_BLOCK + 8;
type Window = [u8; WINDOW];

/// Where `at` is in a window: the data's places are below [`MAX_BLOCK`],
/// so the mask leaves them be and lets reads go unchecked. Only words
/// wholly inside the data are read.
#[inline(always)]
fn place(at: usize) -> usize {
    at & (MAX_BLOCK - 1)
}

/// The eight bytes at `at`, little-endian.
#[inline(always)]
fn word(src: &Window, at: usize) -> u64 {
    src[place(at)..].first_chunk::<8>().map_or(0, |b| u64::from_le_bytes(*b))
}

/// The four bytes at `at`, little-endian.
#[inline(always)]
fn word4(src: &Window, at: usize) -> u32 {
    src[place(at)..].first_chunk::<4>().map_or(0, |b| u32::from_le_bytes(*b))
}

/// The hash of the six bytes at the bottom of `word`.
#[inline(always)]
fn hash(word: u64) -> usize {
    const PRIME: u64 = 227_718_039_650_203;
    ((word << 16).wrapping_mul(PRIME) >> (64 - HASH_LOG)) as usize
}

/// How many bytes from `a` on are as from `b` on, `b` before `a`, up to
/// `end`, the data's end.
#[inline(always)]
fn alike(src: &Window, end: usize, a: usize, b: usize) -> usize {
    let mut len = 0;
    while a + len + 8 <= end {
        let diff = word(src, a + len) ^ word(src, b + len);
        if diff != 0 {
            return len + (diff.trailing_zeros() / 8) as usize;
        }
        len += 8;
    }
    while a + len < end && src[place(a + len)] == src[place(b + len)] {
        len += 1;
    }
    len
}

/// The masks of the low 0 to 63 bits.
const MASKS: [u64; 64] = {
    let mut masks = [0; 64];
    let mut i = 0;
    while i < 64 {
        masks[i] = (1 << i) - 1;
        i += 1;
    }
    masks
};

/// Bits written from the low end of each byte up, as the decoder reads
/// them from the last byte down.
struct Bits<'o> {
    out: &'o mut [u8],
    at: usize,
    acc: u64,
    n: u32,
}

impl<'o> Bits<'o> {
    fn new(out: &'o mut [u8]) -> Bits<'o> {
        Bits { out, at: 0, acc: 0, n: 0 }
    }

    /// Adds the low `bits` of `value`; the bits held stay below 64.
    #[inline(always)]
    fn add(&mut self, value: u64, bits: u32) {
        self.acc |= (value & MASKS[bits as usize & 63]) << self.n;
        self.n += bits;
    }

    /// Writes the whole bytes held, or fails at the end of the room.
    #[inline(always)]
    fn flush(&mut self) -> Option<()> {
        let bytes = self.n / 8;
        let to = self.out.get_mut(self.at..)?.first_chunk_mut::<8>()?;
        *to = self.acc.to_le_bytes();
        self.at += bytes as usize;
        // At most 7 bytes: fewer than 64 bits are held.
        self.acc >>= bytes * 8;
        self.n %= 8;
        Some(())
    }

    /// Ends the stream with its mark, a set bit; returns its length.
    fn close(mut self) -> Option<usize> {
        self.add(1, 1);
        self.flush()?;
        Some(self.at + usize::from(self.n > 0))
    }
}

/// The most states a code of sequences has, and the most symbols.
const MAX_STATES: usize = 1 << 9;
const MAX_SYMBOLS: usize = 53;

/// The fewest sequences that are worth codes of their own.
const MIN_FITTED: usize = 64;

/// An FSE code's encoding table, built as the reference encoder builds
/// it so that its states are the decoder's.
struct Table {
    log: u32,
    /// The state after each, by symbol and the bits written.
    states: KVec<u16>,
    /// Each symbol's bits to write, less the state's share of them, and
    /// where its states start in `states` less its share.
    symbols: KVec<(u32, i32)>,
    /// The symbol of each state, spread as the decoder spreads them.
    spread: KVec<u8>,
}

impl Table {
    fn new() -> Result<Table, NoMemory> {
        Ok(Table {
            log: 0,
            states: crate::filled(MAX_STATES, 0)?,
            symbols: crate::filled(64, (0, 0))?,
            spread: crate::zeroed(MAX_STATES)?,
        })
    }

    /// The predefined code of `kind`.
    fn predefined(kind: Kind) -> Result<Table, NoMemory> {
        let mut table = Table::new()?;
        let (log, shares) = kind.predefined();
        table.build(log, shares);
        Ok(table)
    }

    /// Makes it the code of accuracy `log` whose symbols have `shares`
    /// of its states, -1 counting one; they sum to `2^log`.
    fn build(&mut self, log: u32, shares: &[i16]) {
        let size = 1usize << log;
        let Table { states, symbols, spread, .. } = self;
        let mut start = [0u16; MAX_SYMBOLS + 1];
        let mut high = size;
        for (sym, &share) in shares.iter().enumerate() {
            let n = if share == -1 {
                high -= 1;
                spread[high] = sym as u8;
                1
            } else {
                share.max(0) as u16
            };
            start[sym + 1] = start[sym] + n;
        }
        let step = (size >> 1) + (size >> 3) + 3;
        let mut pos = 0;
        for (sym, &share) in shares.iter().enumerate() {
            for _ in 0..share.max(0) {
                spread[pos] = sym as u8;
                pos = (pos + step) & (size - 1);
                while pos >= high {
                    pos = (pos + step) & (size - 1);
                }
            }
        }
        for (u, &sym) in spread[..size].iter().enumerate() {
            let at = &mut start[usize::from(sym)];
            states[usize::from(*at)] = (size + u) as u16;
            *at += 1;
        }
        let mut total = 0i32;
        for (out, &share) in symbols.iter_mut().zip(shares) {
            *out = match share {
                0 => (((log + 1) << 16) - (1 << log), 0),
                -1 | 1 => {
                    total += 1;
                    ((log << 16) - (1 << log), total - 2)
                }
                n => {
                    let n = n as u32;
                    let bits = log - (n - 1).ilog2();
                    total += n as i32;
                    ((bits << 16) - (n << bits), total - 2 * n as i32)
                }
            };
        }
        self.log = log;
    }

    /// Makes it the code of the one symbol `sym`, which takes no bits.
    fn one(&mut self, sym: usize) {
        self.log = 0;
        self.states[0] = 0;
        self.symbols[sym] = (0, 0);
    }
}

/// A [`Table`] seen whole, which its users index without checks.
struct Coder<'t> {
    log: u32,
    states: &'t [u16; MAX_STATES],
    symbols: &'t [(u32, i32); 64],
}

impl<'t> Coder<'t> {
    fn of(table: &'t Table) -> Option<Coder<'t>> {
        Some(Coder {
            log: table.log,
            states: table.states.as_slice().try_into().ok()?,
            symbols: table.symbols.as_slice().try_into().ok()?,
        })
    }

    /// The state that starts a stream with `sym`, the last symbol coded.
    fn first(&self, sym: usize) -> u32 {
        let (bits, find) = self.symbols[sym & 63];
        let n = (bits + (1 << 15)) >> 16;
        let value = (n << 16) - bits;
        u32::from(self.states[((value >> n) as i32 + find) as usize & (MAX_STATES - 1)])
    }

    /// Codes `sym` from `state`.
    #[inline(always)]
    fn put(&self, out: &mut Bits<'_>, state: &mut u32, sym: usize) {
        let (bits, find) = self.symbols[sym & 63];
        let n = (*state + bits) >> 16;
        out.add(u64::from(*state), n);
        *state = u32::from(self.states[((*state >> n) as i32 + find) as usize & (MAX_STATES - 1)]);
    }
}

/// The accuracy of a code for `count` symbols, the largest `top`, at
/// most `max`: as the reference encoder picks it.
fn accuracy(max: u32, count: usize, top: usize) -> u32 {
    let count = count as u32;
    let least = (count.ilog2() + 1).min((top.max(1) as u32).ilog2() + 2);
    max.min((count - 1).ilog2().saturating_sub(2)).max(least).clamp(5, max)
}

/// Shares of `2^log` states for symbols counted `counts` times, `total`
/// in all, each at least one; false when they cannot be made to sum so.
fn normalize(counts: &[u32], total: usize, log: u32, shares: &mut [i16]) -> bool {
    let (mut left, mut largest, mut most) = (1i64 << log, 0, 0);
    for (sym, (share, &n)) in shares.iter_mut().zip(counts).enumerate() {
        let s = if n == 0 {
            0
        } else {
            ((i64::from(n) << log) + total as i64 / 2).checked_div(total as i64).unwrap_or(0).max(1)
        };
        if s > most {
            (most, largest) = (s, sym);
        }
        *share = s as i16;
        left -= s;
    }
    let fixed = most + left;
    if fixed < 1 {
        return false;
    }
    shares[largest] = fixed as i16;
    true
}

/// Writes the description of an FSE code (RFC 8878, 4.1.1): its accuracy
/// `log` and its symbols' `shares`; returns its length.
fn write_shares(log: u32, shares: &[i16], out: &mut [u8]) -> Option<usize> {
    let mut bits = u64::from(log - 5);
    let mut n = 4;
    let mut at = 0;
    let mut put16 = |bits: &mut u64, at: &mut usize| -> Option<()> {
        out.get_mut(*at..*at + 2)?.copy_from_slice(&(*bits as u16).to_le_bytes());
        *at += 2;
        *bits >>= 16;
        Some(())
    };
    let (mut remaining, mut threshold, mut width) = ((1i32 << log) + 1, 1i32 << log, log + 1);
    let (mut sym, mut after_zero) = (0, false);
    while sym < shares.len() && remaining > 1 {
        if after_zero {
            let mut start = sym;
            while sym < shares.len() && shares[sym] == 0 {
                sym += 1;
            }
            if sym == shares.len() {
                return None;
            }
            while sym >= start + 24 {
                start += 24;
                bits |= 0xffff << n;
                put16(&mut bits, &mut at)?;
            }
            while sym >= start + 3 {
                start += 3;
                bits |= 3 << n;
                n += 2;
            }
            bits |= ((sym - start) as u64) << n;
            n += 2;
            if n > 16 {
                put16(&mut bits, &mut at)?;
                n -= 16;
            }
        }
        let mut count = i32::from(shares[sym]);
        sym += 1;
        let max = 2 * threshold - 1 - remaining;
        remaining -= count.abs();
        count += 1;
        if count >= threshold {
            count += max;
        }
        bits |= (count as u64) << n;
        n += width;
        if count < max {
            n -= 1;
        }
        after_zero = count == 1;
        if remaining < 1 {
            return None;
        }
        while remaining < threshold {
            width -= 1;
            threshold >>= 1;
        }
        if n > 16 {
            put16(&mut bits, &mut at)?;
            n -= 16;
        }
    }
    if remaining != 1 {
        return None;
    }
    put16(&mut bits, &mut at)?;
    Some(at - 2 + n.div_ceil(8) as usize)
}

/// Tables of a Huffman code of literals, made for each block.
struct Huffman {
    counts: KVec<u32>,
    lens: KVec<u8>,
    codes: KVec<u16>,
    /// The symbols, rarest first.
    order: KVec<u8>,
    /// The weights of the code's tree: leaves, as in `order`, then the
    /// nodes joining them.
    weights: KVec<u32>,
    parents: KVec<u16>,
    depths: KVec<u8>,
}

impl Huffman {
    fn new() -> Result<Huffman, NoMemory> {
        Ok(Huffman {
            counts: crate::filled(256, 0)?,
            lens: crate::zeroed(256)?,
            codes: crate::filled(256, 0)?,
            order: crate::zeroed(256)?,
            weights: crate::filled(512, 0)?,
            parents: crate::filled(512, 0)?,
            depths: crate::zeroed(512)?,
        })
    }

    /// Gives symbols `0..=max` with a count codes of at most
    /// [`HUFFMAN_MAX_BITS`] bits, in `lens`; returns the longest, or
    /// `None` when no code fits.
    fn lengths(&mut self, max: usize) -> Option<u32> {
        let Huffman { counts, lens, order, weights, parents, depths, .. } = self;
        let mut leaves = 0;
        for sym in 0..=max {
            lens[sym] = 0;
            if counts[sym] > 0 {
                order[leaves] = sym as u8;
                leaves += 1;
            }
        }
        let order = &mut order[..leaves];
        order.sort_unstable_by_key(|&sym| counts[usize::from(sym)]);
        for (weight, &sym) in weights.iter_mut().zip(order.iter()) {
            *weight = counts[usize::from(sym)];
        }
        // Two queues: the leaves, and the nodes as they are made, whose
        // weights grow; each node joins the lightest two.
        let (mut leaf, mut node, mut made) = (0, leaves, leaves);
        while made < 2 * leaves - 1 {
            let mut lightest = || {
                if leaf < leaves && (node >= made || weights[leaf] <= weights[node]) {
                    leaf += 1;
                    leaf - 1
                } else {
                    node += 1;
                    node - 1
                }
            };
            let (a, b) = (lightest(), lightest());
            weights[made] = weights[a] + weights[b];
            parents[a] = made as u16;
            parents[b] = made as u16;
            made += 1;
        }
        depths[made - 1] = 0;
        for i in (0..made - 1).rev() {
            depths[i] = depths[usize::from(parents[i])] + 1;
        }
        let mut longest = 0;
        for (i, &sym) in order.iter().enumerate() {
            lens[usize::from(sym)] = depths[i];
            longest = longest.max(u32::from(depths[i]));
        }
        if longest <= HUFFMAN_MAX_BITS {
            return Some(longest);
        }
        // Too long: cut the longest codes, lengthen the rarest of the rest
        // until the code is no larger than whole, then shorten the
        // commonest until it is whole again.
        const L: u32 = HUFFMAN_MAX_BITS;
        let unit = |len: u8| 1i32 << (L - u32::from(len));
        let mut total = 0;
        for &sym in order.iter() {
            let len = &mut lens[usize::from(sym)];
            *len = (*len).min(L as u8);
            total += unit(*len);
        }
        for &sym in order.iter() {
            let len = &mut lens[usize::from(sym)];
            while total > 1 << L && u32::from(*len) < L {
                total -= unit(*len) / 2;
                *len += 1;
            }
        }
        while total < 1 << L {
            let before = total;
            for &sym in order.iter().rev() {
                let len = &mut lens[usize::from(sym)];
                while *len > 1 && total + unit(*len) <= 1 << L {
                    total += unit(*len);
                    *len -= 1;
                }
            }
            if total == before {
                return None;
            }
        }
        if total != 1 << L {
            return None;
        }
        Some(order.iter().map(|&sym| u32::from(lens[usize::from(sym)])).max().unwrap_or(0))
    }

    /// Gives symbols `0..=max` their codes from their lengths, as the
    /// decoder assigns them: the lightest weights first, in symbol order.
    fn assign(&mut self, max: usize, bits: u32) {
        let mut start = [0u32; HUFFMAN_MAX_BITS as usize + 2];
        for &len in &self.lens[..=max] {
            if len > 0 {
                let weight = bits + 1 - u32::from(len);
                start[weight as usize + 1] += 1 << (weight - 1);
            }
        }
        for w in 1..start.len() {
            start[w] += start[w - 1];
        }
        for (code, &len) in self.codes.iter_mut().zip(&self.lens[..=max]) {
            if len > 0 {
                let weight = bits + 1 - u32::from(len);
                let at = &mut start[weight as usize];
                *code = (*at >> (weight - 1)) as u16;
                *at += 1 << (weight - 1);
            }
        }
    }

    /// Codes `lits` into one stream, the last first; returns its length.
    fn stream(&self, lits: &[u8], out: &mut [u8]) -> Option<usize> {
        let mut bits = Bits::new(out);
        for (i, &sym) in lits.iter().rev().enumerate() {
            let sym = usize::from(sym);
            bits.add(u64::from(self.codes[sym]), u32::from(self.lens[sym]));
            if i % 4 == 3 {
                bits.flush()?;
            }
        }
        bits.close()
    }
}

/// Writes the header of literals kept as they are (`kind` 0) or as one
/// byte repeated (1), `n` of them; returns its length.
fn plain_header(kind: u32, n: usize, out: &mut [u8]) -> Option<usize> {
    let n = n as u32;
    let (value, len) = match n {
        0..32 => (kind | n << 3, 1),
        32..4096 => (kind | 1 << 2 | n << 4, 2),
        _ => (kind | 3 << 2 | n << 4, 3),
    };
    out.get_mut(..len)?.copy_from_slice(&value.to_le_bytes()[..len]);
    Some(len)
}

/// What [`Encoder::find`] has found so far: the block's literals and
/// sequences.
struct Found<'e> {
    literals: &'e mut [u8],
    sequences: &'e mut [Sequence],
    count: usize,
    lits: usize,
}

impl Found<'_> {
    /// Adds the sequence of `literals` and a match of `len` bytes at
    /// offset value `offset`; with no room, the block is cut short there.
    #[inline(always)]
    fn emit(&mut self, literals: &[u8], offset: usize, len: usize) {
        let n = literals.len();
        let Some(to) = self.literals.get_mut(self.lits..self.lits + n) else { return };
        let Some(seq) = self.sequences.get_mut(self.count) else { return };
        copy_short(to, literals);
        *seq = Sequence { literals: n as u32, offset: offset as u32, len: len as u32 };
        self.count += 1;
        self.lits += n;
    }
}

/// Makes zstd frames.
pub struct Encoder {
    /// The data to compress.
    input: KVec<u8>,
    /// The last place each hash was seen.
    table: KVec<u32>,
    /// The block's literals, in order.
    literals: KVec<u8>,
    sequences: KVec<Sequence>,
    /// Each sequence's codes: of its literals' length, its match's length
    /// and its offset.
    codes: KVec<[u8; 3]>,
    huffman: Huffman,
    /// The predefined codes of literals' lengths, offsets and matches'
    /// lengths, then the block's own: on the heap, which keeps an encoder
    /// small enough to move.
    tables: KVec<Table>,
    /// How often the block's sequences use each symbol of the three codes,
    /// 64 apart.
    counts: KVec<u32>,
    shares: KVec<i16>,
}

impl Encoder {
    /// An encoder, with its buffers and tables on the heap.
    #[inline(never)]
    pub fn new() -> Result<Encoder, NoMemory> {
        Ok(Encoder {
            input: crate::zeroed(WINDOW)?,
            table: crate::filled(1 << HASH_LOG, 0)?,
            literals: crate::zeroed(MAX_BLOCK)?,
            sequences: crate::filled(MAX_SEQUENCES, Sequence::default())?,
            codes: crate::filled(MAX_SEQUENCES, [0; 3])?,
            huffman: Huffman::new()?,
            tables: {
                let mut tables = mem::vec_with_capacity(6)?;
                for kind in [Kind::Literals, Kind::Offsets, Kind::Matches] {
                    mem::push(&mut tables, Table::predefined(kind)?)?;
                }
                for _ in 0..3 {
                    mem::push(&mut tables, Table::new()?)?;
                }
                tables
            },
            counts: crate::filled(3 * 64, 0)?,
            shares: crate::filled(64, 0)?,
        })
    }

    /// The bytes of memory it holds, about.
    pub fn bytes(&self) -> usize {
        self.input.capacity()
            + self.table.capacity() * 4
            + self.literals.capacity()
            + self.sequences.capacity() * size_of::<Sequence>()
            + self.codes.capacity() * 3
            + 20 * 1024
    }

    /// Where the data to compress goes: [`MAX_BLOCK`] bytes.
    pub fn input(&mut self) -> &mut [u8] {
        &mut self.input[..MAX_BLOCK]
    }

    /// Compresses the first `n` bytes of the input, 1 to [`MAX_BLOCK`],
    /// into a frame at the start of `out`, if it takes at most `limit`
    /// bytes; returns its length. `out` needs 8 bytes more than `limit`,
    /// for the streams' last words.
    pub fn compress(&mut self, n: usize, out: &mut [u8], limit: usize) -> Option<usize> {
        if n == 0 || n > MAX_BLOCK || out.len() < limit.checked_add(8)? {
            return None;
        }
        let head = FRAME_HEADER + BLOCK_HEADER;
        let (sequences, literals) = self.find(n);
        let body = out.get_mut(head..)?;
        let mut size = self.literals_section(literals, body)?;
        size += self.sequences_section(sequences, body.get_mut(size..)?)?;
        let total = head + size;
        if total > limit {
            return None;
        }
        out[..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4] = 0xa0;
        out[5..9].copy_from_slice(&(n as u32).to_le_bytes());
        let block = (size as u32) << 3 | 2 << 1 | 1;
        out[9..12].copy_from_slice(&block.to_le_bytes()[..3]);
        Some(total)
    }

    /// Finds the sequences and literals of the first `n` bytes of the
    /// input; returns how many of each.
    fn find(&mut self, n: usize) -> (usize, usize) {
        let Encoder { input, table, literals, sequences, .. } = self;
        let Ok(src) = <&Window>::try_from(&input[..]) else { return (0, 0) };
        let table: &mut [u32; 1 << HASH_LOG] = match table.as_mut_slice().try_into() {
            Ok(table) => table,
            Err(_) => return (0, 0),
        };
        table.fill(0);
        let mut found = Found {
            literals: literals.as_mut_slice(),
            sequences: sequences.as_mut_slice(),
            count: 0,
            lits: 0,
        };
        let (mut rep, mut before) = (1, 4);
        let end = n.saturating_sub(TAIL);
        let (mut anchor, mut ip) = (0, 1);
        while ip < end {
            let h = hash(word(src, ip));
            let seen = table[h] as usize;
            table[h] = ip as u32;
            let start = ip;
            let next = ip + 1;
            if rep <= next && word4(src, next - rep) == word4(src, next) {
                let len = MIN_MATCH + alike(src, n, next + 4, next + 4 - rep);
                found.emit(&src[anchor..next], 1, len);
                ip = next + len;
            } else if seen < ip && word4(src, seen) == word4(src, ip) {
                let (mut at, mut from) = (ip, seen);
                let mut len = MIN_MATCH + alike(src, n, ip + 4, seen + 4);
                while at > anchor && from > 0 && src[place(at - 1)] == src[place(from - 1)] {
                    (at, from, len) = (at - 1, from - 1, len + 1);
                }
                let offset = at - from;
                found.emit(&src[anchor..at], offset + 3, len);
                (before, rep) = (rep, offset);
                ip = at + len;
            } else {
                ip += 1 + ((ip - anchor) >> 8);
                continue;
            }
            anchor = ip;
            if ip < end {
                // Two places more in the table: in the match, and near its end.
                table[hash(word(src, start + 2))] = (start + 2) as u32;
                table[hash(word(src, ip - 2))] = (ip - 2) as u32;
            }
            // With no literals, offset value 1 is the offset before last.
            while ip < end && before <= ip && word4(src, ip - before) == word4(src, ip) {
                let len = MIN_MATCH + alike(src, n, ip + 4, ip + 4 - before);
                (rep, before) = (before, rep);
                table[hash(word(src, ip))] = ip as u32;
                found.emit(&src[anchor..ip], 1, len);
                ip += len;
                anchor = ip;
            }
        }
        let Found { literals, count, mut lits, .. } = found;
        if let Some(to) = literals.get_mut(lits..lits + (n - anchor)) {
            to.copy_from_slice(&src[anchor..n]);
            lits += n - anchor;
        }
        (count, lits)
    }

    /// Writes the block's `n` literals; returns their section's length.
    fn literals_section(&mut self, n: usize, out: &mut [u8]) -> Option<usize> {
        let lits = self.literals.get(..n)?;
        let raw = |out: &mut [u8]| {
            let at = plain_header(0, n, out)?;
            out.get_mut(at..at + n)?.copy_from_slice(lits);
            Some(at + n)
        };
        if n < MIN_HUFFMAN {
            return raw(out);
        }
        let h = &mut self.huffman;
        h.counts.fill(0);
        for &b in lits {
            h.counts[usize::from(b)] += 1;
        }
        let max = h.counts.iter().rposition(|&c| c > 0)?;
        if h.counts[max] as usize == n {
            let at = plain_header(1, n, out)?;
            *out.get_mut(at)? = lits[0];
            return Some(at + 1);
        }
        if max > 128 {
            return raw(out);
        }
        let Some(bits) = h.lengths(max) else { return raw(out) };
        let payload: usize = h.counts[..=max]
            .iter()
            .zip(&h.lens[..=max])
            .map(|(&c, &l)| c as usize * usize::from(l))
            .sum();
        let (header, streams) = match n {
            0..256 => (3, 1),
            256..1024 => (3, 4),
            1024..16384 => (4, 4),
            _ => (5, 4),
        };
        let tree = 1 + max.div_ceil(2);
        let jump = if streams == 4 { 6 } else { 0 };
        if header + tree + jump + payload / 8 + streams >= n {
            return raw(out);
        }
        h.assign(max, bits);
        // The code's description: the weights of symbols 0 to max - 1, the
        // last implied.
        let desc = out.get_mut(header..header + tree)?;
        desc[0] = 127 + max as u8;
        desc[1..].fill(0);
        for (sym, &len) in h.lens[..max].iter().enumerate() {
            let weight = if len > 0 { (bits + 1 - u32::from(len)) as u8 } else { 0 };
            desc[1 + sym / 2] |= if sym % 2 == 0 { weight << 4 } else { weight };
        }
        let mut at = header + tree;
        if streams == 1 {
            at += h.stream(lits, out.get_mut(at..)?)?;
        } else {
            let part = n.div_ceil(4);
            let table = at;
            at += jump;
            for (i, chunk) in lits.chunks(part).enumerate() {
                let len = h.stream(chunk, out.get_mut(at..)?)?;
                if i < 3 {
                    let to = out.get_mut(table + 2 * i..table + 2 * i + 2)?;
                    to.copy_from_slice(&u16::try_from(len).ok()?.to_le_bytes());
                }
                at += len;
            }
        }
        let compressed = at - header;
        if at >= n + plain_header(0, n, &mut [0; 3])? {
            return raw(out);
        }
        let (n, c) = (n as u64, compressed as u64);
        let value = match header {
            3 => 2 | u64::from(streams == 4) << 2 | n << 4 | c << 14,
            4 => 2 | 2 << 2 | n << 4 | c << 18,
            _ => 2 | 3 << 2 | n << 4 | c << 22,
        };
        out.get_mut(..header)?.copy_from_slice(&value.to_le_bytes()[..header]);
        Some(at)
    }

    /// Writes the block's first `count` sequences; returns their
    /// section's length.
    fn sequences_section(&mut self, count: usize, out: &mut [u8]) -> Option<usize> {
        let mut at = match count {
            0 => {
                *out.first_mut()? = 0;
                return Some(1);
            }
            1..128 => {
                *out.first_mut()? = count as u8;
                1
            }
            128..0x7f00 => {
                out.get_mut(..2)?.copy_from_slice(&[(count >> 8) as u8 + 128, count as u8]);
                2
            }
            _ => {
                let rest = (count - 0x7f00) as u16;
                out.get_mut(..3)?.copy_from_slice(&[255, rest as u8, (rest >> 8) as u8]);
                3
            }
        };
        let Encoder { sequences, codes, tables, counts, shares, .. } = self;
        let (predefined, fitted) = tables.split_at_mut(3);
        let seqs = sequences.get(..count)?;
        let codes = codes.get_mut(..count)?;
        let tally: &mut [u32; 3 * 64] = counts.as_mut_slice().try_into().ok()?;
        tally.fill(0);
        for (s, code) in seqs.iter().zip(codes.iter_mut()) {
            let (ll, ml, of) = (ll_code(s.literals), ml_code(s.len), s.offset.ilog2() as usize);
            tally[ll & 63] += 1;
            tally[64 + (of & 63)] += 1;
            tally[128 + (ml & 63)] += 1;
            *code = [ll as u8, ml as u8, of as u8];
        }
        // Each code: one symbol, the block's own, or the predefined one
        // for a few sequences or counts that do not fit.
        let modes = at;
        at += 1;
        let mut mode = 0;
        let mut own = [false; 3];
        for (k, kind) in [Kind::Literals, Kind::Offsets, Kind::Matches].into_iter().enumerate() {
            let (last, max) = kind.limits();
            let counts = &counts[64 * k..=64 * k + last];
            let top = counts.iter().rposition(|&n| n > 0)?;
            let m = if counts[top] as usize == count && count > 2 {
                *out.get_mut(at)? = top as u8;
                at += 1;
                fitted[k].one(top);
                1
            } else if count >= MIN_FITTED {
                let log = accuracy(max, count, top);
                let shares = &mut shares[..=top];
                match normalize(&counts[..=top], count, log, shares)
                    .then(|| write_shares(log, shares, out.get_mut(at..)?))
                    .flatten()
                {
                    Some(len) => {
                        at += len;
                        fitted[k].build(log, shares);
                        2
                    }
                    None => 0,
                }
            } else {
                0
            };
            own[k] = m != 0;
            mode |= m << (6 - 2 * k);
        }
        *out.get_mut(modes)? = mode;
        let [ll_table, of_table, ml_table] =
            core::array::from_fn(|k| Coder::of(if own[k] { &fitted[k] } else { &predefined[k] }));
        let (ll_table, of_table, ml_table) = (ll_table?, of_table?, ml_table?);
        let mut bits = Bits::new(out.get_mut(at..)?);
        // The bases are multiples of their codes' ranges: the extra bits
        // are the low bits of the lengths (less 3, for matches).
        let (ll_bits, ml_bits) = (padded(&LL_BITS), padded(&ML_BITS));
        let last = &seqs[count - 1];
        let [ll, ml, of] = codes[count - 1].map(usize::from);
        let mut ml_state = ml_table.first(ml);
        let mut of_state = of_table.first(of);
        let mut ll_state = ll_table.first(ll);
        bits.add(u64::from(last.literals), u32::from(ll_bits[ll & 63]));
        bits.add(u64::from(last.len - 3), u32::from(ml_bits[ml & 63]));
        bits.add(u64::from(last.offset), of as u32);
        bits.flush()?;
        for (s, code) in seqs[..count - 1].iter().zip(&codes[..count - 1]).rev() {
            let [ll, ml, of] = code.map(usize::from);
            of_table.put(&mut bits, &mut of_state, of);
            ml_table.put(&mut bits, &mut ml_state, ml);
            ll_table.put(&mut bits, &mut ll_state, ll);
            let (ll_extra, ml_extra) = (u32::from(ll_bits[ll & 63]), u32::from(ml_bits[ml & 63]));
            // The states take at most 26 bits, the extra bits 49.
            if ll_extra + ml_extra + of as u32 >= 31 {
                bits.flush()?;
            }
            bits.add(u64::from(s.literals), ll_extra);
            bits.add(u64::from(s.len - 3), ml_extra);
            bits.add(u64::from(s.offset), of as u32);
            bits.flush()?;
        }
        bits.add(u64::from(ml_state), ml_table.log);
        bits.add(u64::from(of_state), of_table.log);
        bits.add(u64::from(ll_state), ll_table.log);
        Some(at + bits.close()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zstd::Zstd;

    /// Compresses `data` and has the decoder give it back.
    fn round_trip(data: &[u8]) -> Option<usize> {
        let mut encoder = Encoder::new().unwrap();
        let mut frame = vec![0; MAX_BLOCK + 64];
        encoder.input()[..data.len()].copy_from_slice(data);
        let made = encoder.compress(data.len(), &mut frame, MAX_BLOCK + 32)?;
        let mut out = vec![0; data.len()];
        let got = Zstd::new().unwrap().decompress(&frame[..made], &mut out).unwrap();
        assert_eq!(got, data.len());
        assert!(out == data, "the frame of {} bytes gives other bytes", data.len());
        Some(made)
    }

    fn text(seed: u64, len: usize) -> Vec<u8> {
        const WORDS: &[&str] = &[
            "the", "of", "and", "to", "a", "in", "is", "it", "that", "for", "on", "was", "with",
            "file", "disk", "data", "block", "tree", "page", "read", "write", "copy", "space",
        ];
        let mut state = seed | 1;
        let mut out = Vec::new();
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(WORDS[(state >> 40) as usize % WORDS.len()].as_bytes());
            out.push(if state & 0xf == 0 { b'\n' } else { b' ' });
        }
        out.truncate(len);
        out
    }

    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect()
    }

    #[test]
    fn text_compresses_and_decodes_as_it_was() {
        for len in [64, 100, 255, 256, 1000, 1023, 1024, 4096, 16383, 16384, 65536, MAX_BLOCK] {
            let data = text(len as u64, len);
            let made = round_trip(&data).unwrap();
            if len >= 4096 {
                assert!(made < len / 2, "{len} bytes of text took {made}");
            }
        }
    }

    #[test]
    fn short_and_repeated_data_decodes_as_it_was() {
        for len in 1..80 {
            round_trip(&text(7, len));
            round_trip(&vec![b'x'; len]);
        }
        assert!(round_trip(&vec![0; MAX_BLOCK]).unwrap() < 64);
        let mut runs = Vec::new();
        for n in 0..4000u32 {
            runs.extend(core::iter::repeat_n((n % 7) as u8, (n % 13 + 1) as usize));
        }
        round_trip(&runs);
    }

    #[test]
    fn data_with_high_bytes_and_skewed_literals_decodes_as_it_was() {
        // Literals above 128 are kept as they are.
        let mut data = text(3, MAX_BLOCK);
        for (i, b) in data.iter_mut().enumerate() {
            if i % 97 == 0 {
                *b = 200 + (i % 50) as u8;
            }
        }
        round_trip(&data);
        // Counts that grow as Fibonacci's numbers need codes cut to 11
        // bits.
        let mut skewed = Vec::new();
        let (mut a, mut b) = (1usize, 1usize);
        for sym in 0..20u8 {
            skewed.extend(core::iter::repeat_n(sym, a));
            (a, b) = (b, a + b);
        }
        let mut state = 5u64;
        for i in (1..skewed.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            skewed.swap(i, state as usize % (i + 1));
        }
        skewed.truncate(MAX_BLOCK);
        round_trip(&skewed);
    }

    #[test]
    fn data_that_does_not_compress_makes_no_frame() {
        let data = noise(9, MAX_BLOCK);
        let mut encoder = Encoder::new().unwrap();
        let mut frame = vec![0; MAX_BLOCK + 64];
        encoder.input().copy_from_slice(&data);
        assert_eq!(encoder.compress(data.len(), &mut frame, MAX_BLOCK - 4096), None);
        // Noise with text after it: the text compresses.
        let mut mixed = noise(11, 8192);
        mixed.extend(text(12, MAX_BLOCK - 8192));
        assert!(round_trip(&mixed).unwrap() < MAX_BLOCK / 2);
    }

    #[test]
    fn huffman_codes_are_cut_to_11_bits_and_stay_whole() {
        let mut h = Huffman::new().unwrap();
        let (mut a, mut b) = (1u32, 1u32);
        for count in &mut h.counts[..30] {
            *count = a;
            (a, b) = (b, a + b);
        }
        let bits = h.lengths(29).unwrap();
        assert!(bits <= HUFFMAN_MAX_BITS);
        let total: u32 = h.lens[..30].iter().map(|&len| 1 << (bits - u32::from(len))).sum();
        assert_eq!(total, 1 << bits);
    }

    #[test]
    fn the_codes_of_lengths_are_the_formats() {
        for len in 0..=131_071u32 {
            let ll = ll_code(len);
            assert!(LL_BASE[ll] <= len && len - LL_BASE[ll] < 1 << LL_BITS[ll], "literals {len}");
        }
        for len in 3..=131_074u32 {
            let ml = ml_code(len);
            assert!(ML_BASE[ml] <= len && len - ML_BASE[ml] < 1 << ML_BITS[ml], "match {len}");
        }
    }
}
