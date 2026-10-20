//! NUMA topology: the System Resource Affinity Table (`SRAT`) and the
//! System Locality Information Table (`SLIT`).
//!
//! The SRAT says which *proximity domain* each CPU and each range of
//! memory belongs to; the SLIT gives the relative distance between domains
//! (10 for a domain to itself, larger for farther ones). The kernel numbers
//! the domains it finds 0, 1, ... in table order and calls them nodes.
//!
//! Reference: ACPI Specification 6.5, sections 5.2.16 and 5.2.17.

use crate::{Error, Sdt, u32_at, u64_at};

/// One SRAT entry the kernel understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SratEntry {
    /// A CPU, by APIC ID (xAPIC or x2APIC entries).
    Cpu {
        /// Its proximity domain.
        domain: u32,
        /// Its APIC ID.
        apic_id: u32,
    },
    /// A range of memory.
    Memory {
        /// Its proximity domain.
        domain: u32,
        /// First byte.
        base: u64,
        /// Length in bytes.
        length: u64,
        /// Memory that may be added later (hot-pluggable).
        hot_pluggable: bool,
        /// Non-volatile memory.
        non_volatile: bool,
    },
}

/// The SRAT; [`Srat::entries`] walks its enabled entries.
#[derive(Clone, Copy, Debug)]
pub struct Srat<'a> {
    entries: &'a [u8],
}

const ENABLED: u32 = 1;

impl<'a> Srat<'a> {
    /// Parses an `SRAT` table.
    pub fn parse(sdt: &Sdt<'a>) -> Result<Srat<'a>, Error> {
        if &sdt.signature != b"SRAT" {
            return Err(Error::BadSignature);
        }
        // A reserved word and a reserved quadword precede the entries.
        let entries = sdt.body().get(12..).ok_or(Error::TooShort)?;
        Ok(Srat { entries })
    }

    /// The enabled entries the kernel understands, in table order.
    pub fn entries(&self) -> impl Iterator<Item = SratEntry> + 'a {
        let mut rest = self.entries;
        core::iter::from_fn(move || {
            loop {
                let kind = *rest.first()?;
                let length = usize::from(*rest.get(1)?);
                if length < 2 || length > rest.len() {
                    rest = &[];
                    return None;
                }
                let (entry, tail) = rest.split_at(length);
                rest = tail;
                if let Some(parsed) = parse_entry(kind, entry) {
                    return Some(parsed);
                }
            }
        })
    }
}

fn parse_entry(kind: u8, e: &[u8]) -> Option<SratEntry> {
    match kind {
        // Processor local APIC affinity: the domain is split into a low
        // byte and three high bytes.
        0 if e.len() >= 16 => {
            let flags = u32_at(e, 4)?;
            let domain = u32::from(e[2]) | u32::from_le_bytes([0, e[9], e[10], e[11]]);
            (flags & ENABLED != 0).then_some(SratEntry::Cpu { domain, apic_id: u32::from(e[3]) })
        }
        1 if e.len() >= 40 => {
            let flags = u32_at(e, 28)?;
            let base = u64::from(u32_at(e, 8)?) | u64::from(u32_at(e, 12)?) << 32;
            let length = u64::from(u32_at(e, 16)?) | u64::from(u32_at(e, 20)?) << 32;
            (flags & ENABLED != 0 && length > 0).then_some(SratEntry::Memory {
                domain: u32_at(e, 2)?,
                base,
                length,
                hot_pluggable: flags & 2 != 0,
                non_volatile: flags & 4 != 0,
            })
        }
        2 if e.len() >= 24 => {
            let flags = u32_at(e, 12)?;
            (flags & ENABLED != 0)
                .then_some(SratEntry::Cpu { domain: u32_at(e, 4)?, apic_id: u32_at(e, 8)? })
        }
        _ => None,
    }
}

/// The SLIT: distances between proximity domains.
#[derive(Clone, Copy, Debug)]
pub struct Slit<'a> {
    /// Number of domains (localities).
    pub localities: usize,
    matrix: &'a [u8],
}

impl<'a> Slit<'a> {
    /// Parses a `SLIT` table.
    pub fn parse(sdt: &Sdt<'a>) -> Result<Slit<'a>, Error> {
        if &sdt.signature != b"SLIT" {
            return Err(Error::BadSignature);
        }
        let body = sdt.body();
        let localities = usize::try_from(u64_at(body, 0).ok_or(Error::TooShort)?)
            .ok()
            .filter(|&n| n <= 256)
            .ok_or(Error::TooShort)?;
        let matrix = body.get(8..8 + localities * localities).ok_or(Error::TooShort)?;
        Ok(Slit { localities, matrix })
    }

    /// Distance from domain `from` to domain `to`, or `None` if either is
    /// out of range or the table says they cannot reach each other (255).
    pub fn distance(&self, from: usize, to: usize) -> Option<u8> {
        if from >= self.localities || to >= self.localities {
            return None;
        }
        self.matrix.get(from * self.localities + to).copied().filter(|&d| d != 255)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::table;

    fn memory_entry(domain: u32, base: u64, length: u64, flags: u32) -> Vec<u8> {
        let mut e = vec![1, 40];
        e.extend_from_slice(&domain.to_le_bytes());
        e.extend_from_slice(&[0, 0]);
        e.extend_from_slice(&(base as u32).to_le_bytes());
        e.extend_from_slice(&((base >> 32) as u32).to_le_bytes());
        e.extend_from_slice(&(length as u32).to_le_bytes());
        e.extend_from_slice(&((length >> 32) as u32).to_le_bytes());
        e.extend_from_slice(&[0; 4]);
        e.extend_from_slice(&flags.to_le_bytes());
        e.extend_from_slice(&[0; 8]);
        e
    }

    #[test]
    fn parses_cpus_and_memory() {
        let mut body = vec![1, 0, 0, 0];
        body.extend_from_slice(&[0; 8]);
        // xAPIC CPU 3 in domain 0x01_0002, enabled.
        body.extend_from_slice(&[0, 16, 0x02, 3, 1, 0, 0, 0, 0, 0, 0x01, 0, 0, 0, 0, 0]);
        // A disabled CPU is skipped.
        body.extend_from_slice(&[0, 16, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // x2APIC CPU 300 in domain 1.
        let mut x2 = vec![2, 24, 0, 0];
        x2.extend_from_slice(&1u32.to_le_bytes());
        x2.extend_from_slice(&300u32.to_le_bytes());
        x2.extend_from_slice(&1u32.to_le_bytes());
        x2.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&x2);
        body.extend_from_slice(&memory_entry(1, 0x1_0000_0000, 0x4000_0000, 1 | 2));
        body.extend_from_slice(&memory_entry(0, 0, 0xa_0000, 0));
        // An unknown entry type is skipped.
        body.extend_from_slice(&[3, 4, 0, 0]);
        let bytes = table(b"SRAT", 3, &body);
        let sdt = Sdt::parse(0x1000, &bytes).unwrap();
        let srat = Srat::parse(&sdt).unwrap();
        let entries: Vec<_> = srat.entries().collect();
        assert_eq!(
            entries,
            [
                SratEntry::Cpu { domain: 0x01_0002, apic_id: 3 },
                SratEntry::Cpu { domain: 1, apic_id: 300 },
                SratEntry::Memory {
                    domain: 1,
                    base: 0x1_0000_0000,
                    length: 0x4000_0000,
                    hot_pluggable: true,
                    non_volatile: false,
                },
            ]
        );
    }

    #[test]
    fn parses_distances() {
        let mut body = 2u64.to_le_bytes().to_vec();
        body.extend_from_slice(&[10, 21, 21, 255]);
        let bytes = table(b"SLIT", 1, &body);
        let slit = Slit::parse(&Sdt::parse(0, &bytes).unwrap()).unwrap();
        assert_eq!(slit.localities, 2);
        assert_eq!(slit.distance(0, 1), Some(21));
        assert_eq!(slit.distance(1, 1), None);
        assert_eq!(slit.distance(2, 0), None);
        let short = table(b"SLIT", 1, &3u64.to_le_bytes());
        assert!(Slit::parse(&Sdt::parse(0, &short).unwrap()).is_err());
    }
}
