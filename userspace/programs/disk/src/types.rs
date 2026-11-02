// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Partition types: what an entry says its partition holds, a GUID in a
//! GPT and a byte in an MBR, and the short aliases rdisk(8) and layout
//! files take for the ones Dukovo uses (`efi`, `bios-boot`, `swap`,
//! `dukovo-root`, `linux`, `windows-data`).

use std::fmt;

use crate::guid::Guid;
use crate::table::Scheme;

/// What kind an entry says its partition is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A GPT entry's type GUID.
    Gpt(Guid),
    /// An MBR entry's type byte.
    Mbr(u8),
}

impl Kind {
    /// The kind's name, `EFI System`, or `unknown`.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Gpt(guid) => TYPES.iter().find(|t| t.gpt == Some(guid)).map(|t| t.name),
            Kind::Mbr(byte) => MBR_NAMES
                .iter()
                .find(|(b, _)| *b == byte)
                .map(|(_, n)| *n)
                .or_else(|| TYPES.iter().find(|t| t.mbr == Some(byte)).map(|t| t.name)),
        }
        .unwrap_or("unknown")
    }

    /// The alias rdisk takes for it, if it has one.
    pub fn alias(self) -> Option<&'static str> {
        TYPES
            .iter()
            .find(|t| match self {
                Kind::Gpt(guid) => t.gpt == Some(guid),
                Kind::Mbr(byte) => t.mbr == Some(byte),
            })
            .map(|t| t.alias)
    }

    /// Whether this kind is that of an extended MBR partition, which holds
    /// logical ones.
    pub fn is_extended(self) -> bool {
        matches!(self, Kind::Mbr(0x05 | 0x0f | 0x85))
    }
}

impl fmt::Display for Kind {
    /// As blkid's PART_ENTRY_TYPE: the GUID in lowercase, or the byte
    /// in hexadecimal (`0x83`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Gpt(guid) => f.write_str(&guid.lower()),
            Kind::Mbr(byte) => write!(f, "{byte:#04x}"),
        }
    }
}

/// A partition type rdisk knows by name.
#[derive(Clone, Copy, Debug)]
pub struct Type {
    /// What rdisk and layout files call it.
    pub alias: &'static str,
    /// Its name, as fdisk's list gives it.
    pub name: &'static str,
    /// Its GPT type GUID, if GPT has one.
    pub gpt: Option<Guid>,
    /// Its MBR type byte, if MBR has one.
    pub mbr: Option<u8>,
    /// sgdisk's four-digit code for it, which rdisk takes too.
    pub code: Option<u16>,
}

const fn gpt(
    alias: &'static str,
    name: &'static str,
    guid: &str,
    mbr: Option<u8>,
    code: u16,
) -> Type {
    Type { alias, name, gpt: Some(Guid::known(guid)), mbr, code: Some(code) }
}

/// The types rdisk knows by name, in the order `l` lists them.
pub const TYPES: &[Type] = &[
    gpt("efi", "EFI System", "C12A7328-F81F-11D2-BA4B-00A0C93EC93B", Some(0xef), 0xef00),
    gpt("bios-boot", "BIOS boot", "21686148-6449-6E6F-744E-656564454649", None, 0xef02),
    gpt("swap", "Linux swap", "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F", Some(0x82), 0x8200),
    // Minted by ADR 0137. On an MBR, Dukovo's root is a Linux partition.
    Type {
        alias: "dukovo-root",
        name: "Dukovo root",
        gpt: Some(Guid::known("90C9A303-3954-4174-8F8B-EE75A92A9EB0")),
        mbr: None,
        code: None,
    },
    gpt("linux", "Linux filesystem", "0FC63DAF-8483-4772-8E79-3D69D8477DE4", Some(0x83), 0x8300),
    gpt("linux-root", "Linux root (x86-64)", "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709", None, 0x8304),
    gpt("linux-home", "Linux home", "933AC7E1-2EB4-4F13-B844-0E14E2AEF915", None, 0x8302),
    gpt("linux-raid", "Linux RAID", "A19D880F-05FC-4D3B-A006-743F0F84911E", Some(0xfd), 0xfd00),
    gpt("linux-lvm", "Linux LVM", "E6D6D379-F507-44C2-A23C-238F2A3DF928", Some(0x8e), 0x8e00),
    gpt(
        "windows-data",
        "Microsoft basic data",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7",
        Some(0x07),
        0x0700,
    ),
    gpt(
        "windows-reserved",
        "Microsoft reserved",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE",
        None,
        0x0c01,
    ),
    gpt(
        "windows-recovery",
        "Windows recovery environment",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC",
        Some(0x27),
        0x2700,
    ),
    // FAT on an MBR has types of its own; on a GPT it is basic data.
    Type { alias: "fat32", name: "W95 FAT32 (LBA)", gpt: None, mbr: Some(0x0c), code: None },
    Type { alias: "fat16", name: "W95 FAT16 (LBA)", gpt: None, mbr: Some(0x0e), code: None },
    Type { alias: "fat12", name: "FAT12", gpt: None, mbr: Some(0x01), code: None },
];

/// The names of MBR types rdisk shows but has no alias for, and those
/// whose name differs from their alias's (0x07 holds NTFS and exFAT).
const MBR_NAMES: &[(u8, &str)] = &[
    (0x00, "empty"),
    (0x04, "FAT16 <32M"),
    (0x05, "Extended"),
    (0x06, "FAT16"),
    (0x07, "HPFS/NTFS/exFAT"),
    (0x0b, "W95 FAT32"),
    (0x0f, "W95 Ext'd (LBA)"),
    (0x83, "Linux"),
    (0x85, "Linux extended"),
    (0xee, "GPT"),
];

/// The aliases that mean basic data on a GPT, for FAT.
const GPT_DATA: [&str; 3] = ["fat32", "fat16", "fat12"];

/// The kind `text` names in a table of `scheme`: an alias, sgdisk's code
/// (`ef00`), a GUID for a GPT or a byte for an MBR (`0x83`, `83`).
pub fn parse(text: &str, scheme: Scheme) -> Option<Kind> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    if scheme == Scheme::Gpt && GPT_DATA.contains(&lower.as_str()) {
        return parse("windows-data", scheme);
    }
    if let Some(t) = TYPES.iter().find(|t| t.alias == lower) {
        return match scheme {
            Scheme::Gpt => t.gpt.map(Kind::Gpt),
            Scheme::Mbr => t.mbr.or((t.alias == "dukovo-root").then_some(0x83)).map(Kind::Mbr),
        };
    }
    match scheme {
        Scheme::Gpt => {
            if let Some(guid) = Guid::from_text(text) {
                return Some(Kind::Gpt(guid));
            }
            let code = u16::from_str_radix(&lower, 16).ok().filter(|_| lower.len() == 4)?;
            TYPES.iter().find(|t| t.code == Some(code)).and_then(|t| t.gpt).map(Kind::Gpt)
        }
        Scheme::Mbr => {
            let hex = lower.strip_prefix("0x").unwrap_or(&lower);
            if hex.is_empty() || hex.len() > 2 {
                return None;
            }
            u8::from_str_radix(hex, 16).ok().filter(|&b| b != 0).map(Kind::Mbr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_codes_guids_and_bytes() {
        let efi = Guid::known("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(parse("efi", Scheme::Gpt), Some(Kind::Gpt(efi)));
        assert_eq!(parse("EF00", Scheme::Gpt), Some(Kind::Gpt(efi)));
        assert_eq!(
            parse("c12a7328-f81f-11d2-ba4b-00a0c93ec93b", Scheme::Gpt),
            Some(Kind::Gpt(efi))
        );
        assert_eq!(parse("efi", Scheme::Mbr), Some(Kind::Mbr(0xef)));
        assert_eq!(parse("0x83", Scheme::Mbr), Some(Kind::Mbr(0x83)));
        assert_eq!(parse("dukovo-root", Scheme::Mbr), Some(Kind::Mbr(0x83)));
        assert_eq!(parse("fat32", Scheme::Mbr), Some(Kind::Mbr(0x0c)));
        assert_eq!(parse("fat32", Scheme::Gpt), parse("windows-data", Scheme::Gpt));
        assert_eq!(parse("bios-boot", Scheme::Mbr), None);
        assert_eq!(parse("0", Scheme::Mbr), None);
        assert_eq!(parse("nonsense", Scheme::Gpt), None);
        assert_eq!(Kind::Gpt(efi).name(), "EFI System");
        assert_eq!(Kind::Mbr(0x07).name(), "HPFS/NTFS/exFAT");
        assert_eq!(Kind::Mbr(0x0c).name(), "W95 FAT32 (LBA)");
        assert_eq!(Kind::Gpt(efi).to_string(), "c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
        assert_eq!(Kind::Mbr(0xef).to_string(), "0xef");
    }
}
