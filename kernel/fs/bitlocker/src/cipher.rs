// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A BitLocker volume's sectors, decrypted and encrypted with its key,
//! the FVEK, one sector at a time, each by its offset on the volume.
//!
//! - **XTS-AES** (IEEE 1619), of Windows 10 and later: the tweak is the
//!   sector's number, little-endian.
//! - **AES-CBC**, of Windows 7 and BitLocker To Go: the initialization
//!   vector is the sector's offset in bytes, little-endian, encrypted with
//!   the FVEK.
//! - **AES-CBC with the Elephant diffuser** (Niels Ferguson's, 2006), of
//!   Windows Vista and 7: before CBC, the sector is XORed with a sector
//!   key, from its offset encrypted with the TWEAK key, and its 32-bit
//!   words mixed by the diffusers A and B.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::{Aes128, Aes256, Block};

/// How a volume's sectors are encrypted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// AES-CBC of 128 bits with the Elephant diffuser.
    Elephant128,
    /// AES-CBC of 256 bits with the Elephant diffuser.
    Elephant256,
    /// AES-CBC of 128 bits.
    Cbc128,
    /// AES-CBC of 256 bits.
    Cbc256,
    /// XTS-AES of 128 bits.
    Xts128,
    /// XTS-AES of 256 bits.
    Xts256,
}

impl Method {
    /// Every method.
    pub const ALL: [Method; 6] = [
        Method::Elephant128,
        Method::Elephant256,
        Method::Cbc128,
        Method::Cbc256,
        Method::Xts128,
        Method::Xts256,
    ];

    /// The method the metadata's number `number` names.
    pub fn from_number(number: u16) -> Option<Method> {
        Method::ALL.into_iter().find(|m| m.number() == number)
    }

    /// Its number in the metadata.
    pub fn number(self) -> u16 {
        match self {
            Method::Elephant128 => 0x8000,
            Method::Elephant256 => 0x8001,
            Method::Cbc128 => 0x8002,
            Method::Cbc256 => 0x8003,
            Method::Xts128 => 0x8004,
            Method::Xts256 => 0x8005,
        }
    }

    /// Its name, as Windows' manage-bde gives it.
    pub fn name(self) -> &'static str {
        match self {
            Method::Elephant128 => "AES 128 with Diffuser",
            Method::Elephant256 => "AES 256 with Diffuser",
            Method::Cbc128 => "AES 128",
            Method::Cbc256 => "AES 256",
            Method::Xts128 => "XTS-AES 128",
            Method::Xts256 => "XTS-AES 256",
        }
    }

    /// The bytes of its key, the FVEK, as the metadata wraps it: with the
    /// diffuser, the key's 32 bytes and the TWEAK key's 32, of which a
    /// method of 128 bits uses the first 16 each; for XTS, its two keys.
    pub fn key_bytes(self) -> usize {
        match self {
            Method::Elephant128 | Method::Elephant256 | Method::Xts256 => 64,
            Method::Cbc128 => 16,
            Method::Cbc256 | Method::Xts128 => 32,
        }
    }

    /// The keys of AES in the FVEK `fvek`: the one that encrypts the
    /// sectors, and the one that encrypts their tweaks (XTS) or makes their
    /// sector keys (the diffuser). `None` if it is too short.
    pub fn keys(self, fvek: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
        let fvek = fvek.get(..self.key_bytes())?;
        Some(match self {
            Method::Elephant128 => (&fvek[..16], Some(&fvek[32..48])),
            Method::Elephant256 => (&fvek[..32], Some(&fvek[32..])),
            Method::Cbc128 | Method::Cbc256 => (fvek, None),
            Method::Xts128 => (&fvek[..16], Some(&fvek[16..])),
            Method::Xts256 => (&fvek[..32], Some(&fvek[32..])),
        })
    }
}

/// An AES key, expanded. Its users keep it boxed, as the kernel does
/// (`device::expand`): its variants are of 704 and 960 bytes.
#[allow(clippy::large_enum_variant)]
pub enum Aes {
    /// Of 128 bits.
    Aes128(Aes128),
    /// Of 256 bits.
    Aes256(Aes256),
}

impl Aes {
    /// The key `key`, of 16 or 32 bytes; `None` for another length.
    pub fn new(key: &[u8]) -> Option<Aes> {
        match key.len() {
            16 => Aes::new_128(key),
            32 => Aes::new_256(key),
            _ => None,
        }
    }

    /// The key `key` of 128 bits; `None` for another length.
    pub fn new_128(key: &[u8]) -> Option<Aes> {
        Aes128::new_from_slice(key).ok().map(Aes::Aes128)
    }

    /// The key `key` of 256 bits; `None` for another length.
    pub fn new_256(key: &[u8]) -> Option<Aes> {
        Aes256::new_from_slice(key).ok().map(Aes::Aes256)
    }

    /// Encrypts `blocks`, each on its own.
    pub fn encrypt(&self, blocks: &mut [Block]) {
        match self {
            Aes::Aes128(aes) => aes.encrypt_blocks(blocks),
            Aes::Aes256(aes) => aes.encrypt_blocks(blocks),
        }
    }

    /// Decrypts `blocks`, each on its own.
    pub fn decrypt(&self, blocks: &mut [Block]) {
        match self {
            Aes::Aes128(aes) => aes.decrypt_blocks(blocks),
            Aes::Aes256(aes) => aes.decrypt_blocks(blocks),
        }
    }

    /// The bits of the key.
    fn bits(&self) -> u32 {
        match self {
            Aes::Aes128(_) => 128,
            Aes::Aes256(_) => 256,
        }
    }

    /// `value`, little-endian, encrypted.
    fn encrypt_value(&self, value: u128) -> u128 {
        let mut block = [Block::from(value.to_le_bytes())];
        self.encrypt(&mut block);
        u128::from_le_bytes(block[0].into())
    }
}

/// The blocks encrypted at once: the software AES takes four at a time.
const BATCH: usize = 32;

/// The Elephant diffusers' rotations, by word.
const ROTATE_A: [u32; 4] = [9, 0, 13, 0];
const ROTATE_B: [u32; 4] = [0, 10, 0, 25];

/// A volume's sectors, as their method and keys encrypt them.
pub struct Sectors<'a> {
    method: Method,
    data: &'a Aes,
    tweak: Option<&'a Aes>,
    /// The bytes of each sector.
    size: u64,
}

impl<'a> Sectors<'a> {
    /// The sectors of `size` bytes (a multiple of 16) that `method`
    /// encrypts with `data` and `tweak`, the keys [`Method::keys`] gives;
    /// `None` if they are not the keys it takes.
    pub fn new(method: Method, data: &'a Aes, tweak: Option<&'a Aes>, size: u64) -> Option<Self> {
        let bits = match method {
            Method::Elephant128 | Method::Cbc128 | Method::Xts128 => 128,
            _ => 256,
        };
        let tweaked = !matches!(method, Method::Cbc128 | Method::Cbc256);
        let fine = data.bits() == bits
            && tweak.is_some() == tweaked
            && tweak.is_none_or(|t| t.bits() == bits)
            && size.is_multiple_of(16)
            && size > 0;
        fine.then_some(Sectors { method, data, tweak, size })
    }

    /// Decrypts `sector`, its bytes, which are at `offset` on the volume.
    pub fn decrypt(&self, offset: u64, sector: &mut [u8]) {
        match (self.method, self.tweak) {
            (Method::Xts128 | Method::Xts256, Some(tweak)) => {
                self.xts(tweak, offset, sector, false);
            }
            (Method::Cbc128 | Method::Cbc256, _) => self.cbc_decrypt(offset, sector),
            (_, Some(tweak)) => {
                self.cbc_decrypt(offset, sector);
                let (words, _) = sector.as_chunks_mut::<4>();
                diffuse_b(words, false);
                diffuse_a(words, false);
                xor_sector_key(tweak, offset, sector);
            }
            (_, None) => {}
        }
    }

    /// Encrypts `sector`, which goes at `offset` on the volume.
    pub fn encrypt(&self, offset: u64, sector: &mut [u8]) {
        match (self.method, self.tweak) {
            (Method::Xts128 | Method::Xts256, Some(tweak)) => self.xts(tweak, offset, sector, true),
            (Method::Cbc128 | Method::Cbc256, _) => self.cbc_encrypt(offset, sector),
            (_, Some(tweak)) => {
                xor_sector_key(tweak, offset, sector);
                let (words, _) = sector.as_chunks_mut::<4>();
                diffuse_a(words, true);
                diffuse_b(words, true);
                self.cbc_encrypt(offset, sector);
            }
            (_, None) => {}
        }
    }

    /// XTS: each block XORed with its tweak, encrypted or decrypted, and
    /// XORed again; the first tweak is the sector's number encrypted, and
    /// each next one the last times x in GF(2^128).
    fn xts(&self, tweak: &Aes, offset: u64, sector: &mut [u8], encrypt: bool) {
        let mut t = tweak.encrypt_value(u128::from(offset / self.size));
        let mut blocks = [Block::default(); BATCH];
        let mut tweaks = [0u128; BATCH];
        for chunk in sector.chunks_mut(16 * BATCH) {
            let (words, _) = chunk.as_chunks_mut::<16>();
            for ((word, block), kept) in words.iter().zip(&mut blocks).zip(&mut tweaks) {
                *kept = t;
                *block = Block::from((u128::from_le_bytes(*word) ^ t).to_le_bytes());
                t = (t << 1) ^ if t >> 127 != 0 { 0x87 } else { 0 };
            }
            let n = words.len();
            if encrypt {
                self.data.encrypt(&mut blocks[..n]);
            } else {
                self.data.decrypt(&mut blocks[..n]);
            }
            for ((word, block), kept) in words.iter_mut().zip(&blocks).zip(&tweaks) {
                *word = (u128::from_le_bytes((*block).into()) ^ kept).to_le_bytes();
            }
        }
    }

    /// The initialization vector of the sector at `offset`.
    fn iv(&self, offset: u64) -> u128 {
        self.data.encrypt_value(u128::from(offset))
    }

    fn cbc_encrypt(&self, offset: u64, sector: &mut [u8]) {
        let mut last = self.iv(offset);
        for word in sector.as_chunks_mut::<16>().0 {
            let mut block = [Block::from((u128::from_le_bytes(*word) ^ last).to_le_bytes())];
            self.data.encrypt(&mut block);
            *word = block[0].into();
            last = u128::from_le_bytes(*word);
        }
    }

    fn cbc_decrypt(&self, offset: u64, sector: &mut [u8]) {
        let mut last = self.iv(offset);
        let mut blocks = [Block::default(); BATCH];
        for chunk in sector.chunks_mut(16 * BATCH) {
            let (words, _) = chunk.as_chunks_mut::<16>();
            for (word, block) in words.iter().zip(&mut blocks) {
                *block = Block::from(*word);
            }
            self.data.decrypt(&mut blocks[..words.len()]);
            for (word, block) in words.iter_mut().zip(&blocks) {
                let encrypted = u128::from_le_bytes(*word);
                *word = (u128::from_le_bytes((*block).into()) ^ last).to_le_bytes();
                last = encrypted;
            }
        }
    }
}

/// XORs `sector` with its sector key, 32 bytes again and again: its
/// offset encrypted with the TWEAK key, and its offset with its last byte
/// 0x80.
fn xor_sector_key(tweak: &Aes, offset: u64, sector: &mut [u8]) {
    let mut key = [Block::from(u128::from(offset).to_le_bytes()), Block::default()];
    key[1] = Block::from((u128::from(offset) | 0x80u128 << 120).to_le_bytes());
    tweak.encrypt(&mut key);
    for (at, byte) in sector.iter_mut().enumerate() {
        *byte ^= key[at / 16 % 2][at % 16];
    }
}

/// One step of a diffuser on the 32-bit word `i` of `words`, from the
/// words `near` and `far` after or before it: added as the volume is
/// decrypted, taken away as it is encrypted.
fn step(words: &mut [[u8; 4]], i: usize, near: usize, far: usize, rotate: u32, encrypt: bool) {
    let word = |at: usize| u32::from_le_bytes(words[at]);
    let mix = word(near) ^ word(far).rotate_left(rotate);
    let value = if encrypt { word(i).wrapping_sub(mix) } else { word(i).wrapping_add(mix) };
    words[i] = value.to_le_bytes();
}

/// The Elephant diffuser A, five times over the words: each from the two
/// and five before it.
fn diffuse_a(words: &mut [[u8; 4]], encrypt: bool) {
    let n = words.len();
    for _ in 0..5 {
        for k in 0..n {
            let i = if encrypt { n - 1 - k } else { k };
            step(words, i, (i + n - 2) % n, (i + n - 5) % n, ROTATE_A[i % 4], encrypt);
        }
    }
}

/// The Elephant diffuser B, three times over the words: each from the two
/// and five after it.
fn diffuse_b(words: &mut [[u8; 4]], encrypt: bool) {
    let n = words.len();
    for _ in 0..3 {
        for k in 0..n {
            let i = if encrypt { n - 1 - k } else { k };
            step(words, i, (i + 2) % n, (i + 5) % n, ROTATE_B[i % 4], encrypt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> [u8; 32] {
        let mut out = [0; 32];
        for (at, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * at..2 * at + 2], 16).unwrap();
        }
        out
    }

    /// IEEE 1619's first two XTS-AES-128 vectors, of 32-byte units.
    #[test]
    fn xts_meets_ieee_1619() {
        let cases = [
            (
                [0u8; 16],
                [0u8; 16],
                0u64,
                [0u8; 32],
                "917cf69ebd68b2ec9b9fe9a3eadda692cd43d2f59598ed858c02c2652fbf922e",
            ),
            (
                [0x11; 16],
                [0x22; 16],
                0x33_3333_3333,
                [0x44; 32],
                "c454185e6a16936e39334038acef838bfb186fff7480adc4289382ecd6d394f0",
            ),
        ];
        for (k1, k2, unit, plain, expected) in cases {
            let (data, tweak) = (Aes::new(&k1).unwrap(), Aes::new(&k2).unwrap());
            let sectors = Sectors::new(Method::Xts128, &data, Some(&tweak), 32).unwrap();
            let mut bytes = plain;
            sectors.encrypt(unit * 32, &mut bytes);
            assert_eq!(bytes, hex(expected));
            sectors.decrypt(unit * 32, &mut bytes);
            assert_eq!(bytes, plain);
        }
    }

    /// Every method gives back what it encrypted, and changes it.
    #[test]
    fn every_method_round_trips() {
        let fvek: [u8; 64] = core::array::from_fn(|i| (i * 7 + 3) as u8);
        for method in Method::ALL {
            let (data, tweak) = method.keys(&fvek).unwrap();
            let data = Aes::new(data).unwrap();
            let tweak = tweak.map(|t| Aes::new(t).unwrap());
            for size in [512u64, 4096] {
                let sectors = Sectors::new(method, &data, tweak.as_ref(), size).unwrap();
                let plain: [u8; 4096] = core::array::from_fn(|i| (i % 251) as u8);
                let mut bytes = plain;
                let sector = &mut bytes[..size as usize];
                sectors.encrypt(size * 9, sector);
                assert_ne!(sector, &plain[..size as usize], "{method:?}");
                sectors.decrypt(size * 9, sector);
                assert_eq!(sector, &plain[..size as usize], "{method:?}");
            }
        }
    }
}
