// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Argon2id password hashes (RFC 9106), written as PHC strings:
//! `$argon2id$v=19$m=65536,t=3,p=4$<salt>$<hash>`, the salt and the hash
//! in base64 without padding, as libxcrypt, PHP and Python's argon2-cffi
//! write them, so that a hash moved from Linux reads here and back.

use argon2::{Algorithm, Argon2, Params, Version};
use base64ct::{Base64Unpadded, Encoding};

/// What a hash costs to make, and so to guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cost {
    /// Memory, in KiB.
    pub memory_kib: u32,
    /// Passes over it.
    pub passes: u32,
    /// Lanes.
    pub lanes: u32,
}

impl Cost {
    /// RFC 9106's second recommendation, for machines without much
    /// memory to spare: 64 MiB, 3 passes, 4 lanes; a few tenths of a
    /// second on a laptop.
    pub const DEFAULT: Cost = Cost { memory_kib: 64 * 1024, passes: 3, lanes: 4 };
    /// The boot tests' accounts' (`ob image --user` with `--test-cost`):
    /// 8 MiB and a pass, so that signing in under emulation stays quick.
    /// A hash says its cost, so authd checks either.
    pub const TEST: Cost = Cost { memory_kib: 8 * 1024, passes: 1, lanes: 1 };
}

/// The most a hash read may cost: 1 GiB, 16 passes and 16 lanes, so that
/// a hash edited by hand cannot have authd take all the memory there is.
const MOST: Cost = Cost { memory_kib: 1024 * 1024, passes: 16, lanes: 16 };

/// The salt's length, in bytes, as RFC 9106 recommends.
pub const SALT: usize = 16;
/// The hash's length, in bytes.
const LENGTH: usize = 32;

/// The hasher for `cost`, making `length` bytes.
fn argon2(cost: Cost, length: usize) -> Result<Argon2<'static>, String> {
    let params = Params::new(cost.memory_kib, cost.passes, cost.lanes, Some(length))
        .map_err(|err| format!("Argon2's parameters: {err}"))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// The hash of `password` with `salt`, at `cost`, as a PHC string.
pub fn hash(password: &[u8], salt: &[u8; SALT], cost: Cost) -> Result<String, String> {
    let mut out = [0u8; LENGTH];
    argon2(cost, LENGTH)?
        .hash_password_into(password, salt, &mut out)
        .map_err(|err| format!("Argon2: {err}"))?;
    Ok(format!(
        "$argon2id$v=19$m={},t={},p={}${}${}",
        cost.memory_kib,
        cost.passes,
        cost.lanes,
        Base64Unpadded::encode_string(salt),
        Base64Unpadded::encode_string(&out)
    ))
}

/// A hash read: its cost, salt and hash.
struct Phc {
    cost: Cost,
    salt: Vec<u8>,
    hash: Vec<u8>,
}

/// Reads the PHC string `phc`, of Argon2id version 19 and at most
/// [`MOST`]'s cost.
fn parse(phc: &str) -> Result<Phc, String> {
    let bad = || "not an Argon2id hash".to_string();
    let fields: Vec<&str> = phc.split('$').collect();
    let ["", "argon2id", "v=19", params, salt, hash] = fields[..] else { return Err(bad()) };
    let mut cost = Cost { memory_kib: 0, passes: 0, lanes: 0 };
    for param in params.split(',') {
        let (key, value) = param.split_once('=').ok_or_else(bad)?;
        let value: u32 = value.parse().map_err(|_| bad())?;
        match key {
            "m" => cost.memory_kib = value,
            "t" => cost.passes = value,
            "p" => cost.lanes = value,
            _ => return Err(bad()),
        }
    }
    if cost.memory_kib > MOST.memory_kib || cost.passes > MOST.passes || cost.lanes > MOST.lanes {
        return Err("an Argon2id hash that would cost more than authd spends".into());
    }
    let salt = Base64Unpadded::decode_vec(salt).map_err(|_| bad())?;
    let hash = Base64Unpadded::decode_vec(hash).map_err(|_| bad())?;
    if !(8..=64).contains(&salt.len()) || !(16..=64).contains(&hash.len()) {
        return Err(bad());
    }
    Ok(Phc { cost, salt, hash })
}

/// Whether `password` is the one `phc` is the hash of. A hash it cannot
/// read is an error, so that an account whose line is broken is told
/// from a wrong password.
pub fn verify(password: &[u8], phc: &str) -> Result<bool, String> {
    let Phc { cost, salt, hash } = parse(phc)?;
    let mut out = vec![0u8; hash.len()];
    argon2(cost, hash.len())?
        .hash_password_into(password, &salt, &mut out)
        .map_err(|err| format!("Argon2: {err}"))?;
    // Every byte is compared, so the time taken says nothing of where
    // the two differ.
    let differ = out.iter().zip(&hash).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    Ok(std::hint::black_box(differ) == 0)
}

/// Whether `phc` is a hash this reads.
pub fn readable(phc: &str) -> bool {
    parse(phc).is_ok()
}
