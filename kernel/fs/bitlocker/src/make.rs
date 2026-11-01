// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! BitLocker volumes made, on the host, for tests: a volume's sectors
//! encrypted as Windows 7 and later encrypt them, with a protector for a
//! password, one for a recovery key, or one in the clear, as BitLocker
//! suspended keeps it. dislocker and libbde, which unlock what Windows
//! makes, check that they unlock these too.
//!
//! The volume's last [`RESERVED`] bytes, which its filesystem must leave
//! alone, take the three metadata blocks and the encrypted copy of its
//! first sectors; Windows puts them in files of the filesystem instead.

use std::string::String;
use std::vec::Vec;
use std::{format, vec};

use ccm::KeyInit;
use ccm::aead::AeadInPlace;
use ccm::aead::generic_array::GenericArray;

use crate::cipher::{Aes, Method, Sectors};
use crate::format::{
    BITLOCKER_ID, METADATA_AREA, SIGNATURE, TO_GO_SIGNATURE, kind, protection, value,
};
use crate::keys::{self, Ccm};

/// The bytes of the copy of a volume's first sectors, as Windows 7 and
/// later keep it.
pub const HEADER_SIZE: u64 = 8192;
/// The bytes at the end of a volume that BitLocker's parts take.
pub const RESERVED: u64 = 3 * METADATA_AREA + HEADER_SIZE;
/// The time the volumes are made at, as a FILETIME: 2026-10-31.
const MADE: u64 = 0x01dd_4a7a_0000_0000;
/// The method of the key that wraps the FVEK, as Windows gives it.
const VMK_METHOD: u32 = 0x2000;
/// The method of a stretched key.
const STRETCH_METHOD: u32 = 0x1000;

/// How a volume is made.
pub struct Options<'a> {
    /// How its sectors are encrypted.
    pub method: Method,
    /// Its key, the FVEK: [`Method::key_bytes`] of them.
    pub fvek: &'a [u8],
    /// The volume master key that wraps it.
    pub vmk: [u8; 32],
    /// A password that unlocks it.
    pub password: Option<&'a str>,
    /// A recovery key of 48 digits that unlocks it.
    pub recovery: Option<&'a str>,
    /// Whether it unlocks with no key, as when BitLocker is suspended.
    pub clear: bool,
    /// Whether its first sector is BitLocker To Go's, as on a FAT stick.
    pub to_go: bool,
    /// The bytes of its sectors.
    pub sector: u64,
    /// Its description.
    pub description: &'a str,
}

/// An entry of the metadata: its size, `kind`, `value` and version 1,
/// then `data`, padded to 4 bytes... as Windows has them, unpadded.
fn entry(kind: u16, value: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(&((8 + data.len()) as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// `text` in UTF-16, with a NUL at its end.
fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

/// The key `key` of method `method` wrapped with `with`: an AES-CCM
/// entry's data, its nonce the time and `counter`.
fn wrap(with: &[u8; 32], counter: u32, method: u32, key: &[u8]) -> Vec<u8> {
    let mut plain = Vec::new();
    plain.extend_from_slice(&((keys::KEY_HEADER + key.len()) as u32).to_le_bytes());
    plain.extend_from_slice(&1u16.to_le_bytes());
    plain.extend_from_slice(&1u16.to_le_bytes());
    plain.extend_from_slice(&method.to_le_bytes());
    plain.extend_from_slice(key);
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&MADE.to_le_bytes());
    nonce[8..].copy_from_slice(&counter.to_le_bytes());
    let tag = Ccm::new(GenericArray::from_slice(with))
        .encrypt_in_place_detached(GenericArray::from_slice(&nonce), &[], &mut plain)
        .unwrap();
    let mut out = nonce.to_vec();
    out.extend_from_slice(&tag);
    out.extend_from_slice(&plain);
    out
}

/// A volume master key entry for a protector of `protection`, with its
/// `properties`; its identifier from `id`.
fn vmk_entry(id: u8, protection: u16, properties: &[u8]) -> Vec<u8> {
    let mut data = [id; 16].to_vec();
    data.extend_from_slice(&MADE.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&protection.to_le_bytes());
    data.extend_from_slice(properties);
    entry(kind::VMK, value::VMK, &data)
}

/// The properties of a protector whose key is stretched from `initial`
/// with `salt`: the stretched key's salt, and the VMK wrapped.
fn stretched(initial: &[u8; 32], salt: [u8; 16], vmk: &[u8; 32], counter: u32) -> Vec<u8> {
    let key = keys::stretch(initial, &salt);
    let mut stretch = STRETCH_METHOD.to_le_bytes().to_vec();
    stretch.extend_from_slice(&salt);
    let mut out = entry(kind::PROPERTY, value::STRETCH, &stretch);
    out.extend(entry(kind::PROPERTY, value::CCM, &wrap(&key, counter, VMK_METHOD, vmk)));
    out
}

/// The metadata's entries.
fn entries(options: &Options<'_>, header: u64) -> (Vec<u8>, u32) {
    let mut out = Vec::new();
    let mut counter = 0;
    let mut next = || {
        counter += 1;
        counter
    };
    if let Some(password) = options.password {
        let properties =
            stretched(&keys::password_hash(password), [0x5a; 16], &options.vmk, next());
        out.extend(vmk_entry(0x11, protection::PASSWORD, &properties));
    }
    if let Some(recovery) = options.recovery {
        let key = keys::recovery_key(recovery).expect("a recovery key");
        let properties = stretched(&keys::recovery_hash(&key), [0xa5; 16], &options.vmk, next());
        out.extend(vmk_entry(0x22, protection::RECOVERY, &properties));
    }
    if options.clear {
        let clear = [0x3c; 32];
        let mut key = VMK_METHOD.to_le_bytes().to_vec();
        key.extend_from_slice(&clear);
        let mut properties = entry(kind::PROPERTY, value::KEY, &key);
        properties.extend(entry(
            kind::PROPERTY,
            value::CCM,
            &wrap(&clear, next(), VMK_METHOD, &options.vmk),
        ));
        out.extend(vmk_entry(0x33, protection::CLEAR, &properties));
    }
    let method = u32::from(options.method.number());
    out.extend(entry(kind::FVEK, value::CCM, &wrap(&options.vmk, next(), method, options.fvek)));
    let mut areas = header.to_le_bytes().to_vec();
    areas.extend_from_slice(&HEADER_SIZE.to_le_bytes());
    out.extend(entry(kind::AREAS, value::AREAS, &areas));
    out.extend(entry(kind::DESCRIPTION, value::STRING, &utf16(options.description)));
    (out, counter + 1)
}

/// A metadata block: its block header, metadata header and entries.
fn block(options: &Options<'_>, size: u64, blocks: [u64; 3], header: u64) -> Vec<u8> {
    let (entries, nonce) = entries(options, header);
    let mut out = vec![0u8; 64 + 48];
    out[..8].copy_from_slice(SIGNATURE);
    out[10..12].copy_from_slice(&2u16.to_le_bytes());
    out[12..14].copy_from_slice(&4u16.to_le_bytes());
    out[14..16].copy_from_slice(&4u16.to_le_bytes());
    out[16..24].copy_from_slice(&size.to_le_bytes());
    out[28..32].copy_from_slice(&((HEADER_SIZE / options.sector) as u32).to_le_bytes());
    for (at, offset) in (32..).step_by(8).zip(blocks) {
        out[at..at + 8].copy_from_slice(&offset.to_le_bytes());
    }
    out[56..64].copy_from_slice(&header.to_le_bytes());
    let metadata = (48 + entries.len()) as u32;
    let head = &mut out[64..];
    head[..4].copy_from_slice(&metadata.to_le_bytes());
    head[4..8].copy_from_slice(&1u32.to_le_bytes());
    head[8..12].copy_from_slice(&48u32.to_le_bytes());
    head[12..16].copy_from_slice(&metadata.to_le_bytes());
    head[16..32].copy_from_slice(&[0x44; 16]);
    head[32..36].copy_from_slice(&nonce.to_le_bytes());
    head[36..40].copy_from_slice(&u32::from(options.method.number()).to_le_bytes());
    head[40..48].copy_from_slice(&MADE.to_le_bytes());
    out.extend(entries);
    // Its size, in 16-byte units, as version 2 counts it.
    let units = out.len().div_ceil(16);
    out[8..10].copy_from_slice(&(units as u16).to_le_bytes());
    out.resize(units * 16, 0);
    // Then its validation: the CRC-32 of the block, which dislocker checks.
    let crc = crc32(&out);
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// The CRC-32 of `data`, as zlib's (polynomial 0xedb88320, reflected).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The volume's first sector: BitLocker's, saying where the metadata is.
fn first_sector(options: &Options<'_>, blocks: [u64; 3]) -> [u8; 512] {
    let mut s = [0u8; 512];
    s[..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
    s[3..11].copy_from_slice(if options.to_go { TO_GO_SIGNATURE } else { SIGNATURE });
    s[11..13].copy_from_slice(&(options.sector as u16).to_le_bytes());
    s[13] = 8;
    s[21] = 0xf8;
    s[24..26].copy_from_slice(&0x3fu16.to_le_bytes());
    s[26..28].copy_from_slice(&0xffu16.to_le_bytes());
    s[36..40].copy_from_slice(&0x1fe0u32.to_le_bytes());
    s[48..50].copy_from_slice(&1u16.to_le_bytes());
    s[50..52].copy_from_slice(&6u16.to_le_bytes());
    s[64] = 0x80;
    s[66] = 0x29;
    s[67..71].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    s[71..82].copy_from_slice(b"NO NAME    ");
    s[82..90].copy_from_slice(b"FAT32   ");
    let (id, offsets) = if options.to_go { (424, 440) } else { (160, 176) };
    s[id..id + 16].copy_from_slice(&BITLOCKER_ID);
    for (at, offset) in (offsets..).step_by(8).zip(blocks) {
        s[at..at + 8].copy_from_slice(&offset.to_le_bytes());
    }
    s[510] = 0x55;
    s[511] = 0xaa;
    s
}

/// Encrypts the volume `image` in place as `options` say: its last
/// [`RESERVED`] bytes, which its filesystem leaves alone, take BitLocker's
/// parts.
///
/// # Panics
///
/// If the volume is too small, or `options` are not a volume's.
pub fn make(image: &mut [u8], options: &Options<'_>) {
    let size = image.len() as u64;
    assert!(size > 2 * RESERVED && size.is_multiple_of(options.sector));
    let base = size - RESERVED;
    let blocks = [base, base + METADATA_AREA, base + 2 * METADATA_AREA];
    let header = base + 3 * METADATA_AREA;
    let h = header as usize;
    let first = HEADER_SIZE as usize;
    image.copy_within(..first, h);
    image[..first].fill(0);
    image[..512].copy_from_slice(&first_sector(options, blocks));
    let metadata = block(options, size, blocks, header);
    for b in blocks {
        let b = b as usize;
        image[b..b + METADATA_AREA as usize].fill(0);
        image[b..b + metadata.len()].copy_from_slice(&metadata);
    }
    let (data, tweak) = options.method.keys(options.fvek).expect("an FVEK of its method");
    let data = Aes::new(data).unwrap();
    let tweak = tweak.map(|t| Aes::new(t).unwrap());
    let sectors = Sectors::new(options.method, &data, tweak.as_ref(), options.sector).unwrap();
    let step = options.sector as usize;
    for (at, sector) in image.chunks_exact_mut(step).enumerate() {
        let offset = (at * step) as u64;
        let metadata = blocks.iter().any(|&b| offset >= b && offset - b < METADATA_AREA);
        if offset >= HEADER_SIZE && !metadata {
            sectors.encrypt(offset, sector);
        }
    }
}

/// A recovery key of 48 digits from `seed`, as Windows prints them:
/// eight groups of six, each 11 times a 16-bit number.
pub fn recovery_key(seed: u16) -> String {
    let groups: Vec<String> = (0..8u32)
        .map(|i| format!("{:06}", (u32::from(seed).wrapping_mul(2654 + i * 977) % 65536) * 11))
        .collect();
    groups.join("-")
}
