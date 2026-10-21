// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel CPU microcode updates: parsing, checking and matching.
//!
//! Processors ship with microcode that later updates fix; firmware applies
//! some, and the operating system loads newer ones early in boot. Updates
//! are closed firmware, so OrthrOS never includes them in the base system
//! (ADR 0007); `ob image --microcode` adds a file of them to an image, GRUB
//! loads it as a module, and the kernel looks for an update for each CPU
//! here before writing it to the processor.
//!
//! A file is any number of updates back to back, as in the `intel-ucode`
//! directory of Intel's microcode releases. Each update is a 48-byte
//! header, the update data, and optionally a table of further processor
//! signatures it applies to. Reference: Intel 64 and IA-32 Architectures
//! Software Developer's Manual, volume 3, section 10.11 "Microcode Update
//! Facilities". AMD's format differs and is not supported yet.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

/// Bytes in an update header.
pub const HEADER: usize = 48;
/// Data size of an update whose header says 0.
const DEFAULT_DATA: usize = 2000;
/// Total size of an update whose header says 0.
const DEFAULT_TOTAL: usize = 2048;
/// Bytes before the entries of the extended signature table.
const EXT_HEADER: usize = 20;
/// Bytes per extended signature.
const EXT_ENTRY: usize = 12;

/// Why an update was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Header or loader version other than 1.
    UnknownVersion,
    /// The sizes in the header are inconsistent or exceed the file.
    BadSize,
    /// A checksum does not add up.
    BadChecksum,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::UnknownVersion => "unknown microcode header version",
            Error::BadSize => "inconsistent microcode update size",
            Error::BadChecksum => "microcode checksum mismatch",
        })
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

/// Sum of the little-endian 32-bit words of `bytes`.
fn word_sum(bytes: &[u8]) -> u32 {
    bytes.as_chunks::<4>().0.iter().fold(0u32, |sum, w| sum.wrapping_add(u32::from_le_bytes(*w)))
}

/// The CPU identity an update is matched against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuId {
    /// CPUID leaf 1, EAX: family, model and stepping.
    pub signature: u32,
    /// `1 << platform ID`, where the platform ID is bits 50 to 52 of the
    /// `IA32_PLATFORM_ID` MSR.
    pub platform: u32,
}

/// One checked microcode update.
#[derive(Clone, Copy)]
pub struct Update<'a> {
    bytes: &'a [u8],
    data_size: usize,
}

impl fmt::Debug for Update<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Update")
            .field("revision", &format_args!("{:#x}", self.revision()))
            .field("signature", &format_args!("{:#x}", self.signature()))
            .field("platforms", &format_args!("{:#x}", self.platforms()))
            .field("len", &self.bytes.len())
            .finish()
    }
}

impl<'a> Update<'a> {
    /// Parses and checks the update at the start of `bytes`.
    pub fn parse(bytes: &'a [u8]) -> Result<Update<'a>, Error> {
        if bytes.len() < HEADER {
            return Err(Error::BadSize);
        }
        if u32_at(bytes, 0) != 1 || u32_at(bytes, 20) != 1 {
            return Err(Error::UnknownVersion);
        }
        let data_size = match u32_at(bytes, 28) as usize {
            0 => DEFAULT_DATA,
            size => size,
        };
        let total_size = match u32_at(bytes, 32) as usize {
            0 => DEFAULT_TOTAL,
            size => size,
        };
        if !data_size.is_multiple_of(4)
            || !total_size.is_multiple_of(4)
            || HEADER + data_size > total_size
            || total_size > bytes.len()
        {
            return Err(Error::BadSize);
        }
        let bytes = &bytes[..total_size];
        if word_sum(&bytes[..HEADER + data_size]) != 0 {
            return Err(Error::BadChecksum);
        }
        let update = Update { bytes, data_size };
        if let Some(table) = update.extended_table() {
            let count = u32_at(table, 0) as usize;
            let len = count.checked_mul(EXT_ENTRY).and_then(|n| n.checked_add(EXT_HEADER));
            let table = len.and_then(|len| table.get(..len)).ok_or(Error::BadSize)?;
            if word_sum(table) != 0 {
                return Err(Error::BadChecksum);
            }
            // Each extra signature's checksum stands in for the main one.
            let main =
                update.signature().wrapping_add(update.platforms()).wrapping_add(update.checksum());
            for entry in table[EXT_HEADER..].as_chunks::<EXT_ENTRY>().0 {
                let sum = word_sum(entry);
                if sum != main {
                    return Err(Error::BadChecksum);
                }
            }
        }
        Ok(update)
    }

    fn extended_table(&self) -> Option<&'a [u8]> {
        let rest = &self.bytes[HEADER + self.data_size..];
        (rest.len() >= EXT_HEADER).then_some(rest)
    }

    /// The revision the processor reports once the update is loaded.
    pub fn revision(&self) -> u32 {
        u32_at(self.bytes, 4)
    }

    /// Release date as (year, month, day).
    pub fn date(&self) -> (u16, u8, u8) {
        let date = u32_at(self.bytes, 8);
        let bcd = |value: u32| {
            (0..4).rev().fold(0u32, |acc, digit| acc * 10 + ((value >> (digit * 4)) & 0xf))
        };
        (bcd(date & 0xffff) as u16, bcd(date >> 24) as u8, bcd((date >> 16) & 0xff) as u8)
    }

    /// The main processor signature.
    pub fn signature(&self) -> u32 {
        u32_at(self.bytes, 12)
    }

    fn checksum(&self) -> u32 {
        u32_at(self.bytes, 16)
    }

    /// The platform IDs of the main signature, as a bit mask.
    pub fn platforms(&self) -> u32 {
        u32_at(self.bytes, 24)
    }

    /// The whole update, header included.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The update data, which is what the processor is given. Its address
    /// must be 16-byte aligned when it is loaded.
    pub fn data(&self) -> &'a [u8] {
        &self.bytes[HEADER..HEADER + self.data_size]
    }

    /// Every (signature, platform mask) the update applies to.
    pub fn targets(&self) -> impl Iterator<Item = (u32, u32)> + 'a {
        let main = core::iter::once((self.signature(), self.platforms()));
        let extended = self.extended_table().map_or(&[][..], |table| {
            let count = u32_at(table, 0) as usize;
            &table[EXT_HEADER..EXT_HEADER + count * EXT_ENTRY]
        });
        main.chain(extended.as_chunks::<EXT_ENTRY>().0.iter().map(|e| (u32_at(e, 0), u32_at(e, 4))))
    }

    /// Whether the update is for `cpu`.
    pub fn matches(&self, cpu: CpuId) -> bool {
        self.targets().any(|(signature, platforms)| {
            signature == cpu.signature
                && ((platforms == 0 && cpu.platform == 0) || platforms & cpu.platform != 0)
        })
    }
}

/// The updates in a file, in order. Iteration stops after the first
/// update that fails its checks, which is returned as an error.
pub fn updates(file: &[u8]) -> impl Iterator<Item = Result<Update<'_>, Error>> {
    let mut rest = file;
    core::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match Update::parse(rest) {
            Ok(update) => {
                rest = &rest[update.bytes.len()..];
                Some(Ok(update))
            }
            Err(err) => {
                rest = &[];
                Some(Err(err))
            }
        }
    })
}

/// The newest update in `file` for `cpu`.
pub fn find(file: &[u8], cpu: CpuId) -> Option<Update<'_>> {
    updates(file).filter_map(Result::ok).filter(|u| u.matches(cpu)).max_by_key(|u| u.revision())
}

/// Builds update files for tests, here and in `ob`'s boot tests.
pub mod build {
    /// Encodes an update with the given header fields, `data_words` words of
    /// data and the extra signatures, with correct checksums, into `out`.
    /// Returns its length.
    pub fn update(
        revision: u32,
        signature: u32,
        platforms: u32,
        data_words: usize,
        extended: &[(u32, u32)],
        out: &mut [u8],
    ) -> usize {
        let data_size = data_words * 4;
        let ext_len = if extended.is_empty() { 0 } else { 20 + 12 * extended.len() };
        // Real updates are padded to a multiple of 1 KiB, which keeps the
        // data of every update in a file 16-byte aligned.
        let total = (super::HEADER + data_size + ext_len).next_multiple_of(1024);
        let out = &mut out[..total];
        out.fill(0);
        let mut put = |at: usize, value: u32| out[at..at + 4].copy_from_slice(&value.to_le_bytes());
        put(0, 1);
        put(4, revision);
        put(8, 0x0715_2019);
        put(12, signature);
        put(20, 1);
        put(24, platforms);
        put(28, data_size as u32);
        put(32, total as u32);
        for word in 0..data_words {
            put(super::HEADER + word * 4, 0x0101_0101u32.wrapping_mul(word as u32 + 1));
        }
        let sum = super::word_sum(&out[..super::HEADER + data_size]);
        let checksum = 0u32.wrapping_sub(sum);
        out[16..20].copy_from_slice(&checksum.to_le_bytes());
        if !extended.is_empty() {
            let main = signature.wrapping_add(platforms).wrapping_add(checksum);
            let table = super::HEADER + data_size;
            out[table..table + 4].copy_from_slice(&(extended.len() as u32).to_le_bytes());
            for (i, &(sig, pf)) in extended.iter().enumerate() {
                let at = table + 20 + i * 12;
                out[at..at + 4].copy_from_slice(&sig.to_le_bytes());
                out[at + 4..at + 8].copy_from_slice(&pf.to_le_bytes());
                let entry_sum = main.wrapping_sub(sig).wrapping_sub(pf);
                out[at + 8..at + 12].copy_from_slice(&entry_sum.to_le_bytes());
            }
            let ext_sum = super::word_sum(&out[table..table + ext_len]);
            out[table + 4..table + 8].copy_from_slice(&0u32.wrapping_sub(ext_sum).to_le_bytes());
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Revision, signature, platforms and extra signatures of an update.
    type Spec<'a> = (u32, u32, u32, &'a [(u32, u32)]);

    fn file(updates: &[Spec<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(revision, signature, platforms, extended) in updates {
            let mut buf = vec![0; 8192];
            let len = build::update(revision, signature, platforms, 16, extended, &mut buf);
            out.extend_from_slice(&buf[..len]);
        }
        out
    }

    const KABY_LAKE_R: CpuId = CpuId { signature: 0x806ea, platform: 1 << 7 };

    #[test]
    fn finds_the_newest_matching_update() {
        let bytes = file(&[
            (0x96, 0x806ea, 0xc0, &[]),
            (0xf4, 0x806ea, 0xc0, &[]),
            (0xff, 0x806ea, 0x01, &[]),
            (0x100, 0x906e9, 0x2a, &[]),
        ]);
        assert_eq!(updates(&bytes).count(), 4);
        let update = find(&bytes, KABY_LAKE_R).unwrap();
        assert_eq!(update.revision(), 0xf4);
        assert_eq!(update.date(), (2019, 7, 15));
        assert_eq!(update.data().len(), 64);
        assert!(find(&bytes, CpuId { signature: 0x806ec, platform: 0x80 }).is_none());
    }

    #[test]
    fn extended_signatures_match_too() {
        let bytes = file(&[(0x10, 0x506e3, 0x36, &[(0x806ea, 0x80), (0x806eb, 0xd0)])]);
        let update = find(&bytes, KABY_LAKE_R).unwrap();
        assert_eq!(update.targets().count(), 3);
        assert_eq!(bytes.len() % 1024, 0);
    }

    #[test]
    fn rejects_corruption() {
        let mut bytes = file(&[(1, 0x806ea, 0x80, &[])]);
        bytes[HEADER + 3] ^= 1;
        assert_eq!(updates(&bytes).next().unwrap().unwrap_err(), Error::BadChecksum);

        let mut bytes = file(&[(1, 0x806ea, 0x80, &[(0x806eb, 0x80)])]);
        let last = bytes.len() - 1024 + HEADER + 64 + 20 + 8;
        bytes[last] ^= 1;
        assert_eq!(Update::parse(&bytes).unwrap_err(), Error::BadChecksum);

        let mut bytes = file(&[(1, 0x806ea, 0x80, &[])]);
        bytes[0] = 2;
        assert_eq!(Update::parse(&bytes).unwrap_err(), Error::UnknownVersion);
        let bytes = file(&[(1, 0x806ea, 0x80, &[])]);
        assert_eq!(Update::parse(&bytes[..100]).unwrap_err(), Error::BadSize);
    }

    #[test]
    fn stops_at_the_first_bad_update() {
        let mut bytes = file(&[(1, 0x806ea, 0x80, &[]), (2, 0x806ea, 0x80, &[])]);
        bytes.extend_from_slice(&[0xff; 64]);
        let results: Vec<_> = updates(&bytes).collect();
        assert_eq!(results.len(), 3);
        assert!(results[2].is_err());
        assert_eq!(find(&bytes, KABY_LAKE_R).unwrap().revision(), 2);
    }
}
