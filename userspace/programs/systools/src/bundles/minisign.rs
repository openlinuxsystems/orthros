// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! minisign's keys and signatures (ADR 0222): Ed25519, over the BLAKE2b-512
//! hash of what is signed, in the files minisign itself reads and writes,
//! so that `minisign -V` checks a bundle's signature as `bundle verify`
//! does.
//!
//! - A public key is `Ed`, an 8-byte key ID and the 32-byte key, in
//!   base64 (`RW...`), under an untrusted comment.
//! - A secret key is `Ed`, the KDF's name, `B2`, the KDF's salt and
//!   limits, then the key ID, the 64-byte secret key and a BLAKE2b-256
//!   checksum of the three. Only keys without a password (`minisign -GW`,
//!   KDF `\0\0`) are read: the developer key `bundle` makes has none.
//! - A signature is `ED`, the key ID and the signature of the hash; then a
//!   trusted comment, and a second signature over the first and the
//!   comment, so that the comment cannot be changed either.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Blake2b512, Digest};
use ed25519_compact::{KeyPair, Seed};

use super::base64;

/// The prefix of the untrusted first line of each file.
const UNTRUSTED: &str = "untrusted comment: ";
/// The prefix of a signature's trusted comment.
const TRUSTED: &str = "trusted comment: ";
/// The longest comment minisign writes or reads.
const COMMENT_MAX: usize = 1024;
/// The longest trusted comment minisign reads.
const TRUSTED_MAX: usize = 8192;
/// scrypt's limits, as minisign writes them; not used without a password.
const OPSLIMIT: u64 = 1_048_576;
const MEMLIMIT: u64 = 33_554_432;

/// A key's 8-byte ID, which a signature names its key by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyId(pub [u8; 8]);

impl std::fmt::Display for KeyId {
    /// As minisign shows it: the ID as a little-endian number, in hex.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016X}", u64::from_le_bytes(self.0))
    }
}

/// A public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicKey {
    pub id: KeyId,
    pub key: [u8; 32],
}

impl PublicKey {
    /// The key as minisign writes it on one line, `RW` and 54 more
    /// characters.
    pub fn encoded(&self) -> String {
        let mut bytes = Vec::with_capacity(42);
        bytes.extend_from_slice(b"Ed");
        bytes.extend_from_slice(&self.id.0);
        bytes.extend_from_slice(&self.key);
        base64::encode(&bytes)
    }

    /// The key a line such as `RWQf6LRC...` holds.
    pub fn decode(line: &str) -> Result<PublicKey, String> {
        let bytes = base64::decode(line.trim())
            .filter(|b| b.len() == 42 && b.starts_with(b"Ed"))
            .ok_or("not a minisign public key, which is RW and 54 more characters")?;
        Ok(PublicKey {
            id: KeyId(bytes[2..10].try_into().unwrap_or_default()),
            key: bytes[10..42].try_into().unwrap_or_default(),
        })
    }

    /// The key in a public key file, or on a line of its own.
    pub fn read(text: &str) -> Result<PublicKey, String> {
        let mut lines = text.lines();
        match lines.next() {
            Some(first) if first.starts_with(UNTRUSTED) => {
                PublicKey::decode(lines.next().unwrap_or_default())
            }
            Some(first) => PublicKey::decode(first),
            None => Err("an empty file, not a minisign public key".into()),
        }
    }

    /// The public key file minisign writes.
    pub fn file(&self) -> String {
        format!("{UNTRUSTED}minisign public key {}\n{}\n", self.id, self.encoded())
    }

    /// Checks `signature` over `message` with this key: the signature, then
    /// the trusted comment. Returns the trusted comment.
    pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<String, String> {
        if signature.key != self.id {
            return Err(format!(
                "it was signed by key {}, and the key given is {}",
                signature.key, self.id
            ));
        }
        let key = ed25519_compact::PublicKey::new(self.key);
        let first = ed25519_compact::Signature::new(signature.signature);
        let checked = if signature.hashed {
            key.verify(Blake2b512::digest(message), &first)
        } else {
            key.verify(message, &first)
        };
        checked.map_err(|_| "the signature does not match what it signs".to_string())?;
        let mut global = signature.signature.to_vec();
        global.extend_from_slice(signature.trusted.as_bytes());
        key.verify(&global, &ed25519_compact::Signature::new(signature.global))
            .map_err(|_| "the signature's trusted comment was changed".to_string())?;
        Ok(signature.trusted.clone())
    }
}

/// A secret key, with its public half.
#[derive(Clone)]
pub struct SecretKey {
    pub id: KeyId,
    pair: KeyPair,
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretKey({})", self.id)
    }
}

/// The checksum minisign keeps of a secret key: BLAKE2b-256 of its
/// algorithm, ID and key.
fn checksum(id: KeyId, secret: &[u8]) -> [u8; 32] {
    let mut hash = Blake2b::<U32>::new();
    hash.update(b"Ed");
    hash.update(id.0);
    hash.update(secret);
    hash.finalize().into()
}

impl SecretKey {
    /// A new key from 40 random bytes: 8 for its ID and 32 for its seed.
    pub fn generate(random: &[u8; 40]) -> Result<SecretKey, String> {
        let id = KeyId(random[..8].try_into().unwrap_or_default());
        let seed: [u8; 32] = random[8..].try_into().unwrap_or_default();
        let pair = KeyPair::try_from_seed(Seed::new(seed))
            .map_err(|_| "the random source gave only zeros".to_string())?;
        Ok(SecretKey { id, pair })
    }

    pub fn public(&self) -> PublicKey {
        PublicKey { id: self.id, key: *self.pair.pk }
    }

    /// The key in a minisign secret key file, if it has no password.
    pub fn read(text: &str) -> Result<SecretKey, String> {
        let mut lines = text.lines();
        let line = match lines.next() {
            Some(first) if first.starts_with(UNTRUSTED) => lines.next().unwrap_or_default(),
            _ => return Err("not a minisign secret key file".into()),
        };
        let bytes = base64::decode(line.trim())
            .filter(|b| b.len() == 158 && b.starts_with(b"Ed") && &b[4..6] == b"B2")
            .ok_or("not a minisign secret key file")?;
        match &bytes[2..4] {
            [0, 0] => {}
            b"Sc" => {
                return Err("this key has a password, which bundle does not read: make a key \
                            without one with `minisign -G -W`, or sign with the developer key \
                            bundle makes"
                    .into());
            }
            _ => return Err("not a minisign secret key file".into()),
        }
        let id = KeyId(bytes[54..62].try_into().unwrap_or_default());
        let secret = &bytes[62..126];
        // minisign 0.11 leaves the checksum of a key without a password
        // as zeros; one that is there must match.
        let kept = &bytes[126..158];
        if kept.iter().any(|&b| b != 0) && checksum(id, secret) != kept {
            return Err("the key file is damaged: its checksum does not match".into());
        }
        let pair = KeyPair::from_slice(secret).map_err(|_| "the key file is damaged")?;
        // The public half is in the file too: it must be the seed's.
        let seed: [u8; 32] = secret[..32].try_into().unwrap_or_default();
        let made =
            KeyPair::try_from_seed(Seed::new(seed)).map_err(|_| "the key file is damaged")?;
        if made.pk != pair.pk {
            return Err("the key file is damaged: its two halves do not match".into());
        }
        Ok(SecretKey { id, pair })
    }

    /// The secret key file, without a password, as `minisign -GW` writes
    /// it; `salt` is random, as minisign's, though nothing uses it.
    pub fn file(&self, comment: &str, salt: &[u8; 32]) -> String {
        let mut bytes = Vec::with_capacity(158);
        bytes.extend_from_slice(b"Ed\0\0B2");
        bytes.extend_from_slice(salt);
        bytes.extend_from_slice(&OPSLIMIT.to_le_bytes());
        bytes.extend_from_slice(&MEMLIMIT.to_le_bytes());
        bytes.extend_from_slice(&self.id.0);
        bytes.extend_from_slice(&self.pair.sk[..]);
        bytes.extend_from_slice(&checksum(self.id, &self.pair.sk[..]));
        format!("{UNTRUSTED}{}\n{}\n", one_line(comment, COMMENT_MAX), base64::encode(&bytes))
    }

    /// Signs `message` as `minisign -S` does, with the two comments.
    pub fn sign(&self, message: &[u8], untrusted: &str, trusted: &str) -> Signature {
        let trusted = one_line(trusted, TRUSTED_MAX);
        let first = self.pair.sk.sign(Blake2b512::digest(message), None);
        let mut global = first.to_vec();
        global.extend_from_slice(trusted.as_bytes());
        let second = self.pair.sk.sign(&global, None);
        Signature {
            untrusted: one_line(untrusted, COMMENT_MAX),
            hashed: true,
            key: self.id,
            signature: *first,
            trusted,
            global: *second,
        }
    }
}

/// `text` without line breaks or other control characters but tabs, cut
/// to `max` bytes: a comment is one line, and minisign's trusted comments
/// have tabs between their fields.
fn one_line(text: &str, max: usize) -> String {
    let mut line: String =
        text.chars().map(|c| if c.is_control() && c != '\t' { ' ' } else { c }).collect();
    while line.len() > max {
        line.pop();
    }
    line
}

/// A minisign signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// The first line's comment, which nothing signs.
    pub untrusted: String,
    /// Whether it signs the BLAKE2b-512 hash (`ED`, minisign's way since
    /// 0.8) or the message itself (`Ed`).
    pub hashed: bool,
    /// The ID of the key that made it.
    pub key: KeyId,
    pub signature: [u8; 64],
    /// The trusted comment, which the second signature covers.
    pub trusted: String,
    pub global: [u8; 64],
}

impl Signature {
    /// The signature file minisign writes, four lines.
    pub fn file(&self) -> String {
        let mut first = Vec::with_capacity(74);
        first.extend_from_slice(if self.hashed { b"ED" } else { b"Ed" });
        first.extend_from_slice(&self.key.0);
        first.extend_from_slice(&self.signature);
        format!(
            "{UNTRUSTED}{}\n{}\n{TRUSTED}{}\n{}\n",
            self.untrusted,
            base64::encode(&first),
            self.trusted,
            base64::encode(&self.global)
        )
    }

    /// The signature in a minisign signature file.
    pub fn read(text: &str) -> Result<Signature, String> {
        let bad = || "not a minisign signature file".to_string();
        let mut lines = text.lines();
        let untrusted = lines.next().and_then(|l| l.strip_prefix(UNTRUSTED)).ok_or_else(bad)?;
        let first = lines.next().and_then(base64::decode).filter(|b| b.len() == 74);
        let first = first.ok_or_else(bad)?;
        let hashed = match &first[..2] {
            b"ED" => true,
            b"Ed" => false,
            _ => return Err("a signature of an algorithm minisign does not have".into()),
        };
        let trusted = lines.next().and_then(|l| l.strip_prefix(TRUSTED)).ok_or_else(bad)?;
        let global = lines.next().and_then(base64::decode).filter(|b| b.len() == 64);
        let global = global.ok_or_else(bad)?;
        if lines.any(|line| !line.is_empty())
            || untrusted.len() > COMMENT_MAX
            || trusted.len() > TRUSTED_MAX
        {
            return Err(bad());
        }
        Ok(Signature {
            untrusted: untrusted.into(),
            hashed,
            key: KeyId(first[2..10].try_into().unwrap_or_default()),
            signature: first[10..74].try_into().unwrap_or([0; 64]),
            trusted: trusted.into(),
            global: global.try_into().unwrap_or([0; 64]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test key `minisign -G -W` (0.11) made, and what it signed with
    /// it: `minisign -Sm hello.txt -t 'timestamp:1761868800<TAB>file:hello.txt<TAB>hashed'`
    /// and, the legacy way, `minisign -S -l -t legacy`.
    const MINISIGN_PUBLIC: &str = "untrusted comment: minisign public key 1541115D5CDE228C\n\
        RWSMIt5cXRFBFS8RVW7FVftienboF5JP7k2c+ms0yzLzk/ZH+6UwB5II\n";
    const MINISIGN_SECRET: &str = "untrusted comment: minisign encrypted secret key\n\
        RWQAAEIyAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAjCLeXF0R\
        QRWfo0TD2AZTq5Gldx0W3dNajH/JOXWR2LSAsSA9Y/z6ki8RVW7FVftienboF5JP7k2c+ms0yzLzk/ZH\
        +6UwB5IIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n";
    const MINISIGN_SIGNATURE: &str = "untrusted comment: signature from minisign secret key\n\
        RUSMIt5cXRFBFcY7muPcOQvmVzNd9TVSorzteqwbc9AlpG7x+SMQ5EKooUXOeHAQX/r+hXNFSIw82+vHAwSu4HhRwqdBreZ1mgM=\n\
        trusted comment: timestamp:1761868800\tfile:hello.txt\thashed\n\
        IQFx7qhaFozuP5Ub4thJHZp1qMY0kDDTg8gzCMEb1dxe0OFvy540XF0r5CY5PZl9OMn+rhWkP0ZQBHneN4orAQ==\n";
    const MINISIGN_LEGACY: &str = "untrusted comment: signature from minisign secret key\n\
        RWSMIt5cXRFBFY6gBY5hsNUuz0XDTANIBqqlk6NY9iQcCYsXY++nDaizseC5UUlFqE2tVsOWAJdeN7qrQvLpn09jh2z1yODkwwA=\n\
        trusted comment: legacy\n\
        AeKyWLPrZ8hUTDzVNoLhk294mk1w7u27huvfYYnQaKZdpf0+l1HRHc8pw66xYPfSPTXfnakMstn4+TYh9No7CQ==\n";

    fn key() -> SecretKey {
        let mut random = [0u8; 40];
        for (i, b) in random.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(1);
        }
        SecretKey::generate(&random).unwrap()
    }

    #[test]
    fn keys_are_written_and_read_as_minisign_writes_them() {
        let secret = key();
        let public = secret.public();
        let line = public.encoded();
        assert!(line.starts_with("RW") && line.len() == 56, "{line}");
        assert_eq!(PublicKey::decode(&line).unwrap(), public);
        assert_eq!(PublicKey::read(&public.file()).unwrap(), public);
        assert!(public.file().starts_with("untrusted comment: minisign public key "));
        assert_eq!(public.id.to_string().len(), 16);
        let file = secret.file("Dukovo developer key", &[9; 32]);
        let read = SecretKey::read(&file).unwrap();
        assert_eq!(read.public(), public);
        assert_eq!(read.id, secret.id);
        // A damaged byte fails the checksum.
        let lines: Vec<&str> = file.lines().collect();
        let mut bytes = base64::decode(lines[1]).unwrap();
        bytes[100] ^= 1;
        let damaged = format!("{}\n{}\n", lines[0], base64::encode(&bytes));
        assert!(SecretKey::read(&damaged).unwrap_err().contains("checksum"));
        // A key with a password is not read.
        let mut bytes = base64::decode(lines[1]).unwrap();
        bytes[2..4].copy_from_slice(b"Sc");
        let locked = format!("{}\n{}\n", lines[0], base64::encode(&bytes));
        assert!(SecretKey::read(&locked).unwrap_err().contains("password"));
        assert!(PublicKey::decode("RWQ").is_err());
    }

    #[test]
    fn signatures_check_what_they_sign_and_their_comment() {
        let secret = key();
        let public = secret.public();
        let message = b"0123  manifest.json\n";
        let signature = secret.sign(message, "key RW...", "timestamp:0\tfile:files.sha256\thashed");
        let file = signature.file();
        let read = Signature::read(&file).unwrap();
        assert_eq!(read, signature);
        assert_eq!(
            public.verify(message, &read).unwrap(),
            "timestamp:0\tfile:files.sha256\thashed"
        );
        assert!(public.verify(b"another", &read).unwrap_err().contains("does not match"));
        let mut changed = read.clone();
        changed.trusted.push('!');
        assert!(public.verify(message, &changed).unwrap_err().contains("trusted comment"));
        let mut random = [3u8; 40];
        random[0] = 99;
        let other = SecretKey::generate(&random).unwrap().public();
        assert!(other.verify(message, &read).unwrap_err().contains("signed by key"));
        // A comment is one line, whatever it is given.
        let odd = secret.sign(message, "two\nlines", "a\nb");
        assert_eq!(odd.untrusted, "two lines");
        assert_eq!(public.verify(message, &Signature::read(&odd.file()).unwrap()).unwrap(), "a b");
        for bad in ["", "untrusted comment: x\n", "trusted comment: x\nAAAA\n"] {
            assert!(Signature::read(bad).is_err(), "{bad}");
        }
    }

    /// minisign 0.11's files read here, and what is signed here with its
    /// key is what it signed: Ed25519 is deterministic.
    #[test]
    fn reads_and_writes_what_minisign_does() {
        let public = PublicKey::read(MINISIGN_PUBLIC).unwrap();
        assert_eq!(public.id.to_string(), "1541115D5CDE228C");
        assert_eq!(public.file(), MINISIGN_PUBLIC);
        let signature = Signature::read(MINISIGN_SIGNATURE).unwrap();
        assert!(signature.hashed);
        let trusted = "timestamp:1761868800\tfile:hello.txt\thashed";
        assert_eq!(public.verify(b"hello\n", &signature).unwrap(), trusted);
        let legacy = Signature::read(MINISIGN_LEGACY).unwrap();
        assert!(!legacy.hashed);
        assert_eq!(public.verify(b"hello\n", &legacy).unwrap(), "legacy");
        let secret = SecretKey::read(MINISIGN_SECRET).unwrap();
        assert_eq!(secret.public(), public);
        let ours = secret.sign(b"hello\n", "signature from minisign secret key", trusted);
        assert_eq!(ours.file(), MINISIGN_SIGNATURE);
    }
}
