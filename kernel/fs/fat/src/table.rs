// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The file allocation table's entries (Microsoft's FAT specification,
//! "FAT Data Structure").
//!
//! Entry `n` says what follows cluster `n` in its file's chain: 0 for a
//! free cluster, an end-of-chain mark, a bad-cluster mark, or the next
//! cluster. FAT16 entries are 2 bytes and FAT32's 4 (whose top 4 bits are
//! reserved and kept as found). FAT12 packs two 12-bit entries into 3
//! bytes, so an entry shares a byte with its neighbour: an even entry is
//! the low 12 bits of the 16 at `n * 3 / 2`, an odd one the high 12, and
//! writing one must keep the other's half-byte. The functions here work
//! on the bytes alone; Kani proves the FAT12 packing loses nothing.

/// How wide the entries are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// 12 bits.
    Fat12,
    /// 16 bits.
    Fat16,
    /// 32 bits, 28 of them used.
    Fat32,
}

impl Width {
    /// The width that a volume of `clusters` data clusters has: what
    /// decides it, by the specification, is the count alone.
    pub fn of(clusters: u32) -> Width {
        if clusters < 4085 {
            Width::Fat12
        } else if clusters < 65525 {
            Width::Fat16
        } else {
            Width::Fat32
        }
    }

    /// The bits of an entry.
    pub fn mask(self) -> u32 {
        match self {
            Width::Fat12 => 0xfff,
            Width::Fat16 => 0xffff,
            Width::Fat32 => 0x0fff_ffff,
        }
    }

    /// The end-of-chain mark this driver writes.
    pub fn end(self) -> u32 {
        self.mask()
    }

    /// The bad-cluster mark.
    pub fn bad(self) -> u32 {
        self.mask() - 8
    }

    /// Whether `value` ends a chain: one of the end marks, 0xff8 to 0xfff
    /// for FAT12 and their wider forms, the eight values above the bad
    /// mark.
    pub fn is_end(self, value: u32) -> bool {
        value >= self.mask() - 7
    }

    /// The name, as `/processes/mounts` shows it.
    pub fn name(self) -> &'static str {
        match self {
            Width::Fat12 => "FAT12",
            Width::Fat16 => "FAT16",
            Width::Fat32 => "FAT32",
        }
    }
}

/// Where entry `n` starts in the table, in bytes.
pub fn offset(width: Width, n: u32) -> u64 {
    let n = u64::from(n);
    match width {
        Width::Fat12 => n + n / 2,
        Width::Fat16 => n * 2,
        Width::Fat32 => n * 4,
    }
}

/// How many bytes from [`offset`] an entry touches.
pub fn span(width: Width) -> usize {
    match width {
        Width::Fat12 | Width::Fat16 => 2,
        Width::Fat32 => 4,
    }
}

/// Entry `n`, from the [`span`] bytes at its [`offset`].
pub fn decode(width: Width, n: u32, bytes: [u8; 4]) -> u32 {
    match width {
        Width::Fat12 => {
            let pair = u32::from(u16::from_le_bytes([bytes[0], bytes[1]]));
            if n.is_multiple_of(2) { pair & 0xfff } else { pair >> 4 }
        }
        Width::Fat16 => u32::from(u16::from_le_bytes([bytes[0], bytes[1]])),
        Width::Fat32 => u32::from_le_bytes(bytes) & 0x0fff_ffff,
    }
}

/// The [`span`] bytes at entry `n`'s [`offset`], which were `old`, with
/// the entry set to `value`: the neighbour's half-byte (FAT12) and the
/// reserved bits (FAT32) are kept.
pub fn encode(width: Width, n: u32, old: [u8; 4], value: u32) -> [u8; 4] {
    match width {
        Width::Fat12 => {
            let pair = u16::from_le_bytes([old[0], old[1]]);
            let value = (value & 0xfff) as u16;
            let pair = if n.is_multiple_of(2) {
                (pair & 0xf000) | value
            } else {
                (pair & 0x000f) | (value << 4)
            };
            let [a, b] = pair.to_le_bytes();
            [a, b, old[2], old[3]]
        }
        Width::Fat16 => {
            let [a, b] = ((value & 0xffff) as u16).to_le_bytes();
            [a, b, old[2], old[3]]
        }
        Width::Fat32 => {
            let kept = u32::from_le_bytes(old) & 0xf000_0000;
            (kept | (value & 0x0fff_ffff)).to_le_bytes()
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Writing one FAT12 entry into a table reads back what was written
    /// and leaves every other entry as it was.
    #[kani::proof]
    fn fat12_packing_keeps_the_neighbours() {
        let mut table: [u8; 8] = kani::any();
        let before = table;
        let n: u32 = kani::any_where(|&n: &u32| n < 4);
        let value: u32 = kani::any_where(|&v: &u32| v <= 0xfff);
        let read = |table: &[u8; 8], k: u32| {
            let at = offset(Width::Fat12, k) as usize;
            decode(Width::Fat12, k, [table[at], table[at + 1], 0, 0])
        };
        let at = offset(Width::Fat12, n) as usize;
        let old = [table[at], table[at + 1], 0, 0];
        let new = encode(Width::Fat12, n, old, value);
        table[at] = new[0];
        table[at + 1] = new[1];
        assert_eq!(read(&table, n), value);
        for k in 0..4 {
            if k != n {
                assert_eq!(read(&table, k), read(&before, k));
            }
        }
    }

    /// FAT32 entries keep their reserved top bits.
    #[kani::proof]
    fn fat32_keeps_the_reserved_bits() {
        let old: [u8; 4] = kani::any();
        let value: u32 = kani::any();
        let new = encode(Width::Fat32, 7, old, value);
        assert_eq!(u32::from_le_bytes(new) >> 28, u32::from_le_bytes(old) >> 28);
        assert_eq!(decode(Width::Fat32, 7, new), value & 0x0fff_ffff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_and_marks() {
        assert_eq!(Width::of(4084), Width::Fat12);
        assert_eq!(Width::of(4085), Width::Fat16);
        assert_eq!(Width::of(65525), Width::Fat32);
        assert!(Width::Fat12.is_end(0xff8) && !Width::Fat12.is_end(0xff6));
        assert_eq!(Width::Fat16.bad(), 0xfff7);
        // FAT12: entries 0x123 and 0x456 are bytes 23 61 45.
        let even = encode(Width::Fat12, 0, [0; 4], 0x123);
        let odd = encode(Width::Fat12, 1, [even[1], 0, 0, 0], 0x456);
        let bytes = [even[0], odd[0], odd[1]];
        assert_eq!(bytes, [0x23, 0x61, 0x45]);
        assert_eq!(decode(Width::Fat12, 0, [bytes[0], bytes[1], 0, 0]), 0x123);
        assert_eq!(decode(Width::Fat12, 1, [bytes[1], bytes[2], 0, 0]), 0x456);
        assert_eq!(offset(Width::Fat12, 3), 4);
    }
}
