// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A BitLocker volume's key, the FVEK, unwrapped with a password or
//! recovery key, as Windows does (for unlock(8)).
//!
//! A password, in UTF-16, is hashed with SHA-256 twice; a recovery key's
//! 48 digits are eight numbers, each 11 times a 16-bit one, whose 16 bytes
//! are hashed once. Either hash is then stretched: hashed again a million
//! times (2^20) with its protector's salt, into the AES-256 key that
//! unwraps the volume master key (VMK) with AES-CCM. The VMK unwraps the
//! FVEK. A volume whose BitLocker is suspended keeps its VMK's key in the
//! clear.

use ccm::KeyInit;
use ccm::aead::AeadInPlace;
use ccm::aead::generic_array::GenericArray;
use ccm::consts::{U12, U16};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;
/// A value wiped when dropped, as the keys here are.
pub use zeroize::Zeroizing;

use crate::format::{Entry, Error, Metadata, kind, protection, u32_at, value};

/// AES-CCM as BitLocker wraps keys: AES-256, a 12-byte nonce and a
/// 16-byte tag.
pub type Ccm = ccm::Ccm<aes::Aes256, U16, U12>;

/// The times a key is stretched.
pub const ROUNDS: u64 = 0x10_0000;
/// The bytes of a wrapped key before it: its nonce and tag.
pub const WRAPPED: usize = 12 + 16;
/// The bytes of a key unwrapped before it: its size, two types and its
/// method.
pub const KEY_HEADER: usize = 12;
/// The most bytes of a key: an FVEK of 512 bits.
pub const MAX_KEY: usize = 64;

/// A key, unwrapped.
#[derive(Clone)]
pub struct Key {
    /// Its method's number: for the FVEK, the volume's encryption
    /// ([`crate::cipher::Method`]).
    pub method: u16,
    len: usize,
    bytes: [u8; MAX_KEY],
}

impl Key {
    /// Its bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Zeroize for Key {
    fn zeroize(&mut self) {
        self.bytes.zeroize();
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// What a volume is unlocked with.
#[derive(Clone, Copy)]
pub enum Secret<'a> {
    /// A password.
    Password(&'a str),
    /// A recovery key's 16 bytes ([`recovery_key`]).
    Recovery(&'a [u8; 16]),
}

/// The 16 bytes of the recovery key `text`: 48 digits, in eight groups of
/// six that dashes or spaces may part, each 11 times a 16-bit number.
/// `None` if it is not one.
pub fn recovery_key(text: &str) -> Option<Zeroizing<[u8; 16]>> {
    let mut digits = Zeroizing::new([0u8; 48]);
    let mut n = 0;
    for c in text.trim().chars() {
        match c {
            '0'..='9' if n < 48 => {
                digits[n] = c as u8 - b'0';
                n += 1;
            }
            '-' | ' ' => {}
            _ => return None,
        }
    }
    if n != 48 {
        return None;
    }
    let mut key = Zeroizing::new([0u8; 16]);
    for (group, out) in digits.chunks(6).zip(key.as_chunks_mut::<2>().0) {
        let number = group.iter().fold(0u32, |n, &d| n * 10 + u32::from(d));
        if !number.is_multiple_of(11) || number / 11 > 0xffff {
            return None;
        }
        *out = ((number / 11) as u16).to_le_bytes();
    }
    Some(key)
}

/// The hash a password is stretched from: SHA-256, twice, of its UTF-16.
pub fn password_hash(password: &str) -> Zeroizing<[u8; 32]> {
    let mut hasher = Sha256::new();
    for unit in password.encode_utf16() {
        hasher.update(unit.to_le_bytes());
    }
    let once = Zeroizing::new(<[u8; 32]>::from(hasher.finalize()));
    Zeroizing::new(Sha256::digest(*once).into())
}

/// The hash a recovery key's bytes are stretched from: their SHA-256.
pub fn recovery_hash(key: &[u8; 16]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(Sha256::digest(key).into())
}

/// `initial`, a hash, stretched with `salt` into a protector's key: the
/// SHA-256 of the last hash, the first, the salt and a count, [`ROUNDS`]
/// times.
pub fn stretch(initial: &[u8; 32], salt: &[u8; 16]) -> Zeroizing<[u8; 32]> {
    let mut state = Zeroizing::new([0u8; 88]);
    state[32..64].copy_from_slice(initial);
    state[64..80].copy_from_slice(salt);
    for count in 0..ROUNDS {
        state[80..].copy_from_slice(&count.to_le_bytes());
        let last: [u8; 32] = Sha256::digest(&state[..]).into();
        state[..32].copy_from_slice(&last);
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&state[..32]);
    key
}

/// Unwraps the key `wrapped`, an AES-CCM entry's data, with `key`.
/// Fails with [`Error::WrongKey`] if its tag does not match, and
/// [`Error::Damaged`] if what it holds is not a key.
pub fn unwrap(key: &[u8; 32], wrapped: &[u8]) -> Result<Key, Error> {
    let encrypted = wrapped.get(WRAPPED..).ok_or(Error::Damaged)?;
    if encrypted.len() < KEY_HEADER || encrypted.len() > KEY_HEADER + MAX_KEY {
        return Err(Error::Damaged);
    }
    let mut plain = Zeroizing::new([0u8; KEY_HEADER + MAX_KEY]);
    let plain = &mut plain[..encrypted.len()];
    plain.copy_from_slice(encrypted);
    let nonce = GenericArray::from_slice(&wrapped[..12]);
    let tag = GenericArray::from_slice(&wrapped[12..WRAPPED]);
    Ccm::new(GenericArray::from_slice(key))
        .decrypt_in_place_detached(nonce, &[], plain, tag)
        .map_err(|_| Error::WrongKey)?;
    let size = u32_at(plain, 0) as usize;
    if size < KEY_HEADER || size > plain.len() {
        return Err(Error::Damaged);
    }
    let mut out = Key {
        method: crate::format::u16_at(plain, 8),
        len: size - KEY_HEADER,
        bytes: [0; MAX_KEY],
    };
    out.bytes[..out.len].copy_from_slice(&plain[KEY_HEADER..size]);
    Ok(out)
}

/// The first property of `properties` that holds `value`.
fn property<'a>(mut properties: impl Iterator<Item = Entry<'a>>, value: u16) -> Option<Entry<'a>> {
    properties.find(|p| p.value == value)
}

/// The FVEK of the volume of `metadata`, unwrapped with `secret` through
/// a protector of its kind. Fails with [`Error::NoProtector`] if it has
/// none, [`Error::WrongKey`] if `secret` opens none of them, and
/// [`Error::Damaged`] if the FVEK does not unwrap.
pub fn unlock(metadata: &Metadata<'_>, secret: Secret<'_>) -> Result<Key, Error> {
    let (wanted, initial) = match secret {
        Secret::Password(password) => (protection::PASSWORD, password_hash(password)),
        Secret::Recovery(key) => (protection::RECOVERY, recovery_hash(key)),
    };
    let mut found = false;
    for vmk in metadata.vmks().filter(|v| v.protection == wanted) {
        found = true;
        let stretched = property(vmk.properties, value::STRETCH).and_then(|p| p.data.get(4..20));
        let (Some(salt), Some(wrapped)) = (stretched, property(vmk.properties, value::CCM)) else {
            continue;
        };
        let mut salted = [0u8; 16];
        salted.copy_from_slice(salt);
        let key = stretch(&initial, &salted);
        match unwrap(&key, wrapped.data) {
            Ok(vmk) => return fvek(metadata, &vmk),
            Err(Error::WrongKey) => {}
            Err(err) => return Err(err),
        }
    }
    Err(if found { Error::WrongKey } else { Error::NoProtector })
}

/// The FVEK of a volume whose BitLocker is suspended, unwrapped with the
/// key its clear protector keeps. Fails with [`Error::NoProtector`] if it
/// has none.
pub fn unlock_clear(metadata: &Metadata<'_>) -> Result<Key, Error> {
    for vmk in metadata.vmks().filter(|v| v.protection == protection::CLEAR) {
        let key = property(vmk.properties, value::KEY).and_then(|p| p.data.get(4..36));
        let (Some(key), Some(wrapped)) = (key, property(vmk.properties, value::CCM)) else {
            continue;
        };
        let mut clear = Zeroizing::new([0u8; 32]);
        clear.copy_from_slice(key);
        let vmk = unwrap(&clear, wrapped.data).map_err(|_| Error::Damaged)?;
        return fvek(metadata, &vmk);
    }
    Err(Error::NoProtector)
}

/// The FVEK, unwrapped with the volume master key `vmk`.
fn fvek(metadata: &Metadata<'_>, vmk: &Key) -> Result<Key, Error> {
    let vmk: &[u8; 32] = vmk.bytes().try_into().map_err(|_| Error::Damaged)?;
    let entry = metadata
        .entries()
        .find(|e| e.kind == kind::FVEK && e.value == value::CCM)
        .ok_or(Error::Damaged)?;
    unwrap(vmk, entry.data).map_err(|_| Error::Damaged)
}
