// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Sizes as people write them, `512MiB`, `+1G`, `2048s`, and as the tools
//! print them, `512 MiB`.
//!
//! As util-linux reads them: a letter alone or with `iB` is a power of
//! 1024 (`K`, `KiB`), and with `B` a power of 1000 (`KB`); `s` counts
//! the disk's logical sectors, and a number alone is bytes.

/// An amount of a disk: bytes, or its logical sectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Amount {
    /// Bytes.
    Bytes(u64),
    /// The disk's logical sectors.
    Sectors(u64),
}

impl Amount {
    /// The amount in logical sectors of `sector` bytes, if it is a whole
    /// number of them.
    pub fn sectors(self, sector: u32) -> Option<u64> {
        match self {
            Amount::Sectors(n) => Some(n),
            Amount::Bytes(n) => n.is_multiple_of(u64::from(sector)).then(|| n / u64::from(sector)),
        }
    }

    /// The amount in logical sectors of `sector` bytes, a part of one
    /// counting as one: for a size, where a little more is no harm.
    pub fn sectors_up(self, sector: u32) -> u64 {
        match self {
            Amount::Sectors(n) => n,
            Amount::Bytes(n) => n.div_ceil(u64::from(sector)),
        }
    }
}

/// The units, by their letter: how many times 1024 each is.
const LETTERS: [(char, u32); 6] = [('K', 1), ('M', 2), ('G', 3), ('T', 4), ('P', 5), ('E', 6)];

/// The amount written `text`: `512MiB`, `1G`, `1.5 GiB`, `100MB`,
/// `2048s`, `4096`; `None` if it is not one, or too much.
pub fn parse(text: &str) -> Option<Amount> {
    let text = text.trim();
    let end = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let (number, unit) = (&text[..end], text[end..].trim());
    if number.is_empty() || number.starts_with('.') || number.matches('.').count() > 1 {
        return None;
    }
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let whole: u64 = whole.parse().ok()?;
    if unit == "s" || unit == "S" {
        return fraction.is_empty().then_some(Amount::Sectors(whole));
    }
    let scale: u64 = match unit {
        "" | "B" | "b" => 1,
        _ => {
            let mut chars = unit.chars();
            let letter = chars.next()?.to_ascii_uppercase();
            let (_, power) = LETTERS.iter().find(|(l, _)| *l == letter)?;
            match chars.as_str() {
                "" | "iB" | "ib" => 1u64.checked_shl(10 * power)?,
                "B" | "b" => 1000u64.checked_pow(*power)?,
                _ => return None,
            }
        }
    };
    let mut bytes = whole.checked_mul(scale)?;
    if !fraction.is_empty() {
        // A tenth or a hundredth of a unit, rounded down to a byte.
        let digits = fraction.len().min(6) as u32;
        let part: u64 = fraction[..digits as usize].parse().ok()?;
        let extra = u128::from(part) * u128::from(scale) / 10u128.pow(digits);
        bytes = bytes.checked_add(u64::try_from(extra).ok()?)?;
    }
    Some(Amount::Bytes(bytes))
}

/// `bytes` for people: in the largest unit of 1024 it reaches, with a
/// tenth when it is not a whole number of them, `512 MiB`, `7.5 GiB`,
/// `1.4 MiB`, as the kernel's log and fdisk write sizes.
pub fn human(bytes: u64) -> String {
    let units = [("EiB", 60), ("PiB", 50), ("TiB", 40), ("GiB", 30), ("MiB", 20), ("KiB", 10)];
    let Some(&(unit, shift)) = units.iter().find(|&&(_, shift)| bytes >> shift != 0) else {
        return format!("{bytes} B");
    };
    let whole = bytes >> shift;
    let tenths = (((bytes - (whole << shift)) as u128 * 10) >> shift) as u64;
    match tenths {
        0 => format!("{whole} {unit}"),
        _ => format!("{whole}.{tenths} {unit}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_as_util_linux_reads_them() {
        assert_eq!(parse("512MiB"), Some(Amount::Bytes(512 << 20)));
        assert_eq!(parse("512M"), Some(Amount::Bytes(512 << 20)));
        assert_eq!(parse("512 m"), Some(Amount::Bytes(512 << 20)));
        assert_eq!(parse("100MB"), Some(Amount::Bytes(100_000_000)));
        assert_eq!(parse("1.5GiB"), Some(Amount::Bytes(3 << 29)));
        assert_eq!(parse("2048s"), Some(Amount::Sectors(2048)));
        assert_eq!(parse("4096"), Some(Amount::Bytes(4096)));
        assert_eq!(parse("8E"), Some(Amount::Bytes(8 << 60)));
        for bad in ["", "M", "1.5s", "1..5G", "16E", "1Q", "1MiBs", ".5G", "-1G"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(Amount::Bytes(1 << 20).sectors(512), Some(2048));
        assert_eq!(Amount::Bytes(1000).sectors(512), None);
        assert_eq!(Amount::Bytes(1000).sectors_up(512), 2);
    }

    #[test]
    fn sizes_print_with_a_tenth() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(512 << 20), "512 MiB");
        assert_eq!(human(1_474_560), "1.4 MiB");
        assert_eq!(human(15 << 29), "7.5 GiB");
        assert_eq!(human(u64::MAX), "15.9 EiB");
    }
}
