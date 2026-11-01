// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a disk or partition holds, from its first sectors, as libblkid
//! finds it: a filesystem's kind, label and UUID (FAT, exFAT, NTFS, ext2
//! to ext4, Btrfs, UDF, ISO 9660 and Linux's swap areas), or a disk's
//! partition table, and
//! a partition's UUID and label in its disk's table. For blkid(8) and
//! lsblk(8).

use std::fmt::Write;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;

/// Where the kernel lists the disks and partitions (radix-sysfs(5)).
pub const BLOCKS: &str = "/kernel/block";

/// A filesystem found on a device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filesystem {
    /// libblkid's name for its kind: `vfat`, `exfat`, `ntfs`, `ext4`,
    /// `btrfs`, `udf`, `iso9660`, `swap`, `BitLocker`.
    pub kind: &'static str,
    /// Its version, as lsblk's FSVER shows it: `FAT32`, `1.0`.
    pub version: String,
    pub label: Option<String>,
    pub uuid: Option<String>,
    /// For Btrfs, the UUID of this device of the filesystem.
    pub uuid_sub: Option<String>,
    /// The size of its sectors or blocks, as libblkid's BLOCK_SIZE.
    pub block_size: Option<u32>,
}

/// A disk's partition table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// `gpt` or `dos`.
    pub kind: &'static str,
    pub uuid: String,
}

/// A partition's UUID and label in its disk's table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionIds {
    pub uuid: Option<String>,
    pub label: Option<String>,
}

/// Reads `len` bytes at `offset`; `None` past the end or on an error.
pub type Read<'a> = dyn FnMut(u64, usize) -> Option<Vec<u8>> + 'a;

/// A reader of the device `/devices/<name>`.
pub fn reader(name: &str) -> Option<impl FnMut(u64, usize) -> Option<Vec<u8>>> {
    let file = File::open(format!("/devices/{name}")).ok()?;
    Some(move |offset: u64, len: usize| {
        let mut bytes = vec![0u8; len];
        let mut done = 0;
        while done < len {
            match file.read_at(&mut bytes[done..], offset + done as u64) {
                Ok(0) | Err(_) => return None,
                Ok(n) => done += n,
            }
        }
        Some(bytes)
    })
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Bytes in lower-case hexadecimal.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(2 * bytes.len()), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// A UUID kept as its 16 bytes in order (Btrfs, ext4, swap).
pub fn uuid(bytes: &[u8]) -> String {
    let hex = hex(bytes);
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// A GUID as GPT keeps it: its first three fields little-endian.
pub fn guid(b: &[u8]) -> String {
    let rest = hex(&b[10..16]);
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{rest}",
        u32_at(b, 0),
        u16_at(b, 4),
        u16_at(b, 6),
        b[8],
        b[9]
    )
}

/// Text padded with spaces or NULs, without them; `None` if empty.
fn text(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]).trim_end().to_string();
    (!text.is_empty()).then_some(text)
}

/// UTF-16LE text up to its first NUL.
fn utf16(bytes: &[u8]) -> Option<String> {
    let (pairs, _) = bytes.as_chunks::<2>();
    let units: Vec<u16> =
        pairs.iter().map(|&p| u16::from_le_bytes(p)).take_while(|&u| u != 0).collect();
    let text = String::from_utf16_lossy(&units);
    (!text.is_empty()).then_some(text)
}

/// A FAT or exFAT volume serial number, as `ABCD-1234`.
fn serial(value: u32) -> String {
    format!("{:04X}-{:04X}", value >> 16, value & 0xffff)
}

/// The filesystem on a device, if it holds one libblkid would name.
pub fn filesystem(read: &mut Read<'_>) -> Option<Filesystem> {
    if let Some(found) = read(64 * 1024, 4096).and_then(|s| btrfs(&s)) {
        return Some(found);
    }
    if let Some(found) = read(1024, 1024).and_then(|s| ext(&s)) {
        return Some(found);
    }
    if let Some(found) = read(0, 4096).and_then(|s| swap(&s)) {
        return Some(found);
    }
    // A disc image's first 32 KiB may hold a boot sector of its own, for
    // the image to boot from a USB stick. A DVD's ISO 9660 is a bridge to
    // its UDF, which libblkid names.
    if let Some(found) = udf(read) {
        return Some(found);
    }
    if let Some(found) = iso9660(read) {
        return Some(found);
    }
    let boot = read(0, 512)?;
    if boot[510..512] != [0x55, 0xaa] {
        return None;
    }
    // Before FAT: BitLocker To Go's first sector is a FAT's, for Windows
    // to show the files of its reader.
    if let Some(found) = bitlocker(&boot) {
        return Some(found);
    }
    if &boot[3..11] == b"EXFAT   " {
        return Some(exfat(&boot, read));
    }
    if &boot[3..11] == b"NTFS    " {
        return Some(ntfs(&boot, read));
    }
    fat(&boot)
}

/// A BitLocker volume, from its first sector (ADR 0240): libblkid's
/// version 1 for Windows Vista's, 2 for Windows 7's and later, and 3 for
/// BitLocker To Go's, and its sectors' size.
fn bitlocker(boot: &[u8]) -> Option<Filesystem> {
    use radix_bitlocker::format::{self, TO_GO_SIGNATURE, Volume};
    let (version, block_size) = match format::volume(boot)? {
        Volume::Vista => ("1", u32::from(u16_at(boot, 11))),
        Volume::Locked { sector, .. } if &boot[3..11] == TO_GO_SIGNATURE => ("3", sector as u32),
        Volume::Locked { sector, .. } => ("2", sector as u32),
    };
    Some(Filesystem {
        kind: "BitLocker",
        version: version.to_string(),
        block_size: Some(block_size),
        ..Filesystem::default()
    })
}

/// UDF: its label is the logical volume's identifier, its UUID the start
/// of the volume set's (when it is hex, as the specification asks), and
/// its version the revision its domain names, as libblkid has them.
fn udf(read: &mut Read<'_>) -> Option<Filesystem> {
    // Its recognition sequence says it is UDF.
    let nsr = (0..32u64).any(|i| {
        read(32 * 1024 + i * 2048, 8).is_some_and(|d| matches!(&d[1..6], b"NSR02" | b"NSR03"))
    });
    if !nsr {
        return None;
    }
    // Its anchor, at block 256, says where its volume descriptors are.
    let (block, mut at, count) = [512u64, 1024, 2048, 4096].into_iter().find_map(|block| {
        let avdp = read(256 * block, 512)?;
        (u16_at(&avdp, 0) == 2 && u32_at(&avdp, 12) == 256)
            .then(|| (block, u64::from(u32_at(&avdp, 20)), u64::from(u32_at(&avdp, 16)) / block))
    })?;
    let dstring = |field: &[u8]| -> Option<String> {
        let (&len, bytes) = field.split_last()?;
        let bytes = bytes.get(..usize::from(len))?;
        let (&id, rest) = bytes.split_first()?;
        let text = match id {
            8 | 254 => rest.iter().map(|&b| char::from(b)).collect(),
            16 | 255 => {
                let units: Vec<u16> =
                    rest.as_chunks::<2>().0.iter().map(|&p| u16::from_be_bytes(p)).collect();
                String::from_utf16_lossy(&units)
            }
            _ => return None,
        };
        let text = text.trim_end().to_string();
        (!text.is_empty()).then_some(text)
    };
    let (mut label, mut uuid, mut version) = (None, None, String::new());
    for _ in 0..count.min(64) {
        let Some(d) = read(at * block, 512) else { break };
        match u16_at(&d, 0) {
            // The primary volume descriptor: the volume set.
            1 => {
                let set = dstring(&d[72..200]).unwrap_or_default();
                let start: String = set.chars().take(16).collect();
                if start.len() == 16 && start.chars().all(|c| c.is_ascii_hexdigit()) {
                    uuid = Some(start.to_lowercase());
                }
            }
            // The logical volume descriptor: its name, and its domain's
            // revision, in BCD.
            6 => {
                label = dstring(&d[84..212]);
                let revision = u16_at(&d, 216 + 24);
                if revision != 0 {
                    version = format!("{:x}.{:02x}", revision >> 8, revision & 0xff);
                }
            }
            8 | 0 => break,
            _ => {}
        }
        at += 1;
    }
    Some(Filesystem {
        kind: "udf",
        version,
        label,
        uuid,
        uuid_sub: None,
        block_size: Some(block as u32),
    })
}

/// ISO 9660, from its volume descriptors at 32 KiB: its label is the
/// Joliet one if it has one, and its UUID the time it was last changed,
/// or made, as libblkid has them.
fn iso9660(read: &mut Read<'_>) -> Option<Filesystem> {
    let primary = read(32 * 1024, 2048)?;
    if &primary[1..6] != b"CD001" || primary[0] != 1 {
        return None;
    }
    let mut joliet = None;
    for n in 17..32 {
        let Some(d) = read(n * 2048, 2048) else { break };
        if &d[1..6] != b"CD001" || d[0] == 255 {
            break;
        }
        // A supplementary descriptor with Joliet's escape sequence.
        if d[0] == 2 && d[88] == b'%' && d[89] == b'/' && matches!(d[90], b'@' | b'C' | b'E') {
            joliet = Some(d);
        }
    }
    let label = match &joliet {
        Some(d) => {
            let units: Vec<u16> =
                d[40..72].as_chunks::<2>().0.iter().map(|&p| u16::from_be_bytes(p)).collect();
            let text = String::from_utf16_lossy(&units);
            let text = text.trim_end_matches([' ', '\0']);
            (!text.is_empty()).then(|| text.to_string())
        }
        None => text(&primary[40..72]),
    };
    // Sixteen digits, then the zone: YYYYMMDDHHMMSScc.
    let date = |at: usize| {
        let d = &primary[at..at + 16];
        (d.iter().all(u8::is_ascii_digit) && d.iter().any(|&b| b != b'0')).then(|| {
            let d = String::from_utf8_lossy(d);
            format!(
                "{}-{}-{}-{}-{}-{}-{}",
                &d[..4],
                &d[4..6],
                &d[6..8],
                &d[8..10],
                &d[10..12],
                &d[12..14],
                &d[14..16]
            )
        })
    };
    Some(Filesystem {
        kind: "iso9660",
        version: if joliet.is_some() { "Joliet Extension".to_string() } else { String::new() },
        label,
        uuid: date(830).or_else(|| date(813)),
        uuid_sub: None,
        block_size: Some(u32::from(u16_at(&primary, 128))),
    })
}

/// Btrfs, from its superblock at 64 KiB.
fn btrfs(s: &[u8]) -> Option<Filesystem> {
    if &s[64..72] != b"_BHRfS_M" {
        return None;
    }
    Some(Filesystem {
        kind: "btrfs",
        version: String::new(),
        label: text(&s[299..299 + 256]),
        uuid: Some(uuid(&s[32..48])),
        // The device item's UUID, in the superblock's copy of it.
        uuid_sub: Some(uuid(&s[201 + 66..201 + 82])),
        block_size: Some(u32_at(s, 144)),
    })
}

/// ext2, ext3 or ext4, from its superblock at 1 KiB: ext4 if it has
/// features ext3 lacks (extents, 64-bit, flexible groups and the rest),
/// ext3 if it has a journal.
fn ext(s: &[u8]) -> Option<Filesystem> {
    if u16_at(s, 56) != 0xef53 {
        return None;
    }
    let (compat, incompat, ro_compat) = (u32_at(s, 92), u32_at(s, 96), u32_at(s, 100));
    // FILETYPE, RECOVER and META_BG are ext3's; SPARSE_SUPER, LARGE_FILE
    // and BTREE_DIR its read-only ones.
    let ext4 = incompat & !(0x2 | 0x4 | 0x10) != 0 || ro_compat & !(0x1 | 0x2 | 0x4) != 0;
    let kind = match (ext4, compat & 0x4 != 0) {
        (true, _) => "ext4",
        (false, true) => "ext3",
        (false, false) => "ext2",
    };
    Some(Filesystem {
        kind,
        version: "1.0".into(),
        label: text(&s[120..136]),
        uuid: Some(uuid(&s[104..120])),
        uuid_sub: None,
        block_size: 1024u32.checked_shl(u32_at(s, 24)),
    })
}

/// A Linux swap area of version 1, its signature at the end of its
/// first page.
fn swap(s: &[u8]) -> Option<Filesystem> {
    if &s[4086..4096] != b"SWAPSPACE2" {
        return None;
    }
    Some(Filesystem {
        kind: "swap",
        version: "1".into(),
        label: text(&s[1052..1068]),
        uuid: Some(uuid(&s[1036..1052])),
        uuid_sub: None,
        block_size: Some(4096),
    })
}

/// NTFS: its serial number from the boot sector, as libblkid prints it,
/// and its label from the name (type 60h) of `$Volume`, the master file
/// table's fourth record.
fn ntfs(boot: &[u8], read: &mut Read<'_>) -> Filesystem {
    let sector = u32::from(u16_at(boot, 11));
    let label = (|| {
        let per_cluster = match boot[13] {
            n @ 1..=0x80 => u64::from(n),
            n @ 0xf4.. => 1 << (256 - u32::from(n)),
            _ => return None,
        };
        let cluster = u64::from(sector) * per_cluster;
        // A record's size: in clusters, or 2^n bytes for a byte of -n.
        let record = match boot[64] as i8 {
            n @ 1..=16 => cluster * n as u64,
            n @ -16..=-9 => 1 << -n,
            _ => return None,
        };
        if !(512..=64 * 1024).contains(&record) || !(512..=4096).contains(&sector) {
            return None;
        }
        let at = u64_at(boot, 48).checked_mul(cluster)?.checked_add(3 * record)?;
        let mut rec = read(at, record as usize)?;
        // The last two bytes of each 512 are in the update sequence.
        let (usa, count) = (usize::from(u16_at(&rec, 4)), usize::from(u16_at(&rec, 6)));
        if &rec[..4] != b"FILE" || count == 0 || usa + 2 * count > rec.len() {
            return None;
        }
        for i in 1..count.min(rec.len() / 512 + 1) {
            let kept = [rec[usa + 2 * i], rec[usa + 2 * i + 1]];
            rec[i * 512 - 2..i * 512].copy_from_slice(&kept);
        }
        let mut at = usize::from(u16_at(&rec, 20));
        while at + 24 <= rec.len() && u32_at(&rec, at) != 0xffff_ffff {
            let len = u32_at(&rec, at + 4) as usize;
            if len < 24 || at + len > rec.len() {
                return None;
            }
            // Resident, as a name always is.
            if u32_at(&rec, at) == 0x60 && rec[at + 8] == 0 {
                let len = u32_at(&rec, at + 16) as usize;
                let from = at + usize::from(u16_at(&rec, at + 20));
                return utf16(rec.get(from..from.checked_add(len)?)?);
            }
            at += len;
        }
        None
    })();
    Filesystem {
        kind: "ntfs",
        version: String::new(),
        label,
        uuid: Some(format!("{:016X}", u64_at(boot, 72))),
        uuid_sub: None,
        block_size: Some(sector),
    }
}

/// exFAT: its serial number from the boot sector, and its label from the
/// root directory's volume label entry (type 83h).
fn exfat(boot: &[u8], read: &mut Read<'_>) -> Filesystem {
    let sector = 1u64 << boot[108].min(12);
    let cluster = sector << boot[109].min(25 - boot[108].min(12));
    let heap = u64::from(u32_at(boot, 88)) * sector;
    let root = u64::from(u32_at(boot, 96));
    let label = root.checked_sub(2).and_then(|n| {
        let dir = read(heap + n * cluster, cluster.min(64 * 1024) as usize)?;
        let (entries, _) = dir.as_chunks::<32>();
        let entry = entries.iter().take_while(|e| e[0] != 0).find(|e| e[0] == 0x83)?;
        let count = usize::from(entry[1].min(11));
        utf16(&entry[2..2 + 2 * count])
    });
    Filesystem {
        kind: "exfat",
        version: "1.0".into(),
        label,
        uuid: Some(serial(u32_at(boot, 100))),
        uuid_sub: None,
        block_size: Some(sector as u32),
    }
}

/// FAT12, FAT16 or FAT32, from its boot sector: the extended boot
/// record's type, serial number and label.
fn fat(boot: &[u8]) -> Option<Filesystem> {
    let sector = u16_at(boot, 11);
    if !sector.is_power_of_two() || !(512..=4096).contains(&sector) {
        return None;
    }
    let (version, at) = if &boot[82..87] == b"FAT32" {
        ("FAT32", 64)
    } else if &boot[54..59] == b"FAT12" {
        ("FAT12", 36)
    } else if &boot[54..59] == b"FAT16" {
        ("FAT16", 36)
    } else {
        return None;
    };
    // The extended boot record: its signature, serial and label.
    let extended = matches!(boot[at + 2], 0x28 | 0x29);
    let label = if boot[at + 2] == 0x29 { text(&boot[at + 7..at + 18]) } else { None };
    Some(Filesystem {
        kind: "vfat",
        version: version.into(),
        label: label.filter(|l| l != "NO NAME"),
        uuid: extended.then(|| serial(u32_at(boot, at + 3))),
        uuid_sub: None,
        block_size: Some(u32::from(sector)),
    })
}

/// A disk's partition table: a GPT, its header in logical block 1, or
/// else an MBR.
pub fn table(read: &mut Read<'_>, block: u64) -> Option<Table> {
    if let Some(header) = read(block, 512)
        && &header[..8] == b"EFI PART"
    {
        return Some(Table { kind: "gpt", uuid: guid(&header[56..72]) });
    }
    let mbr = read(0, 512)?;
    if mbr[510..512] != [0x55, 0xaa] || !(0..4).any(|i| mbr[446 + 16 * i + 4] != 0) {
        return None;
    }
    Some(Table { kind: "dos", uuid: format!("{:08x}", u32_at(&mbr, 440)) })
}

/// Partition `number`'s UUID and label in its disk's table.
pub fn partition_ids(read: &mut Read<'_>, block: u64, number: u32) -> PartitionIds {
    let Some(table) = table(read, block) else { return PartitionIds::default() };
    if table.kind == "dos" {
        return PartitionIds { uuid: Some(format!("{}-{number:02x}", table.uuid)), label: None };
    }
    let found = (|| {
        let header = read(block, 512)?;
        let (first, count, size) = (u64_at(&header, 72), u32_at(&header, 80), u32_at(&header, 84));
        if number == 0 || number > count || !(128..=4096).contains(&size) {
            return None;
        }
        let at = first * block + u64::from(number - 1) * u64::from(size);
        let entry = read(at, 128)?;
        Some(PartitionIds { uuid: Some(guid(&entry[16..32])), label: utf16(&entry[56..128]) })
    })();
    found.unwrap_or_default()
}

/// A device's line in `/kernel/block/<name>/<what>`, trimmed.
pub fn block_text(name: &str, what: &str) -> Option<String> {
    fs::read_to_string(format!("{BLOCKS}/{name}/{what}")).ok().map(|t| t.trim().to_string())
}

/// The disk a partition is on: `sda` for `sda1`, `nvme0n1` for
/// `nvme0n1p1`.
pub fn disk_of(name: &str) -> &str {
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    match stem.strip_suffix('p') {
        Some(disk) if disk.ends_with(|c: char| c.is_ascii_digit()) => disk,
        _ => stem,
    }
}

/// Every disk and partition, each disk followed by its partitions in
/// order of their numbers: (name, whether it is a partition).
pub fn devices() -> std::io::Result<Vec<(String, bool)>> {
    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(BLOCKS)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    let number = |name: &str| block_text(name, "partition").and_then(|n| n.parse::<u32>().ok());
    let mut disks: Vec<&String> = names.iter().filter(|n| number(n).is_none()).collect();
    disks.sort();
    let mut out = Vec::new();
    for disk in disks {
        out.push((disk.clone(), false));
        let mut parts: Vec<(u32, &String)> = names
            .iter()
            .filter_map(|n| Some((number(n)?, n)))
            .filter(|(_, n)| disk_of(n) == disk.as_str())
            .collect();
        parts.sort();
        out.extend(parts.into_iter().map(|(_, n)| (n.clone(), true)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(image: &[u8]) -> impl FnMut(u64, usize) -> Option<Vec<u8>> + '_ {
        move |at, len| image.get(at as usize..at as usize + len).map(<[u8]>::to_vec)
    }

    #[test]
    fn finds_fat32_and_its_serial_and_label() {
        let mut image = vec![0u8; 128 * 1024];
        image[11..13].copy_from_slice(&512u16.to_le_bytes());
        image[66] = 0x29;
        image[67..71].copy_from_slice(&0x1234_abcdu32.to_le_bytes());
        image[71..82].copy_from_slice(b"RADIX32    ");
        image[82..90].copy_from_slice(b"FAT32   ");
        image[510..512].copy_from_slice(&[0x55, 0xaa]);
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!((found.kind, found.version.as_str()), ("vfat", "FAT32"));
        assert_eq!(found.label.as_deref(), Some("RADIX32"));
        assert_eq!(found.uuid.as_deref(), Some("1234-ABCD"));
        // With no filesystem, the boot sector's partition entries make an MBR.
        image[82..90].copy_from_slice(b"        ");
        image[440..444].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        image[446 + 4] = 0x0c;
        assert_eq!(filesystem(&mut reading(&image)), None);
        let found = table(&mut reading(&image), 512).unwrap();
        assert_eq!((found.kind, found.uuid.as_str()), ("dos", "deadbeef"));
        let ids = partition_ids(&mut reading(&image), 512, 2);
        assert_eq!(ids.uuid.as_deref(), Some("deadbeef-02"));
    }

    #[test]
    fn finds_bitlocker_before_the_fat_it_shows() {
        let mut image = vec![0u8; 64 * 1024];
        image[3..11].copy_from_slice(b"-FVE-FS-");
        image[11..13].copy_from_slice(&512u16.to_le_bytes());
        image[82..90].copy_from_slice(b"FAT32   ");
        image[160..176].copy_from_slice(&radix_bitlocker::format::BITLOCKER_ID);
        image[510..512].copy_from_slice(&[0x55, 0xaa]);
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!((found.kind, found.version.as_str()), ("BitLocker", "2"));
        assert_eq!((found.label, found.block_size), (None, Some(512)));
        // BitLocker To Go's.
        image[3..11].copy_from_slice(b"MSWIN4.1");
        image[160..176].fill(0);
        image[424..440].copy_from_slice(&radix_bitlocker::format::BITLOCKER_ID);
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!((found.kind, found.version.as_str()), ("BitLocker", "3"));
        // Without its identifier, it is the FAT it shows.
        image[424..440].fill(0);
        assert_eq!(filesystem(&mut reading(&image)).unwrap().kind, "vfat");
    }

    #[test]
    fn finds_udf_and_its_label_uuid_and_revision() {
        let block = 2048;
        let mut image = vec![0u8; 300 * block];
        image[32768 + 2048 + 1..32768 + 2048 + 6].copy_from_slice(b"NSR02");
        let avdp = 256 * block;
        image[avdp..avdp + 2].copy_from_slice(&2u16.to_le_bytes());
        image[avdp + 12..avdp + 16].copy_from_slice(&256u32.to_le_bytes());
        image[avdp + 16..avdp + 20].copy_from_slice(&(16 * 2048u32).to_le_bytes());
        image[avdp + 20..avdp + 24].copy_from_slice(&32u32.to_le_bytes());
        let pvd = 32 * block;
        image[pvd] = 1;
        let set = b"\x086ae66cea51fa2a06LinuxUDF";
        image[pvd + 72..pvd + 72 + set.len()].copy_from_slice(set);
        image[pvd + 199] = set.len() as u8;
        let lvd = 33 * block;
        image[lvd] = 6;
        image[lvd + 84..lvd + 93].copy_from_slice(b"\x08RadixUDF");
        image[lvd + 211] = 9;
        image[lvd + 240..lvd + 242].copy_from_slice(&0x0201u16.to_le_bytes());
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!((found.kind, found.version.as_str()), ("udf", "2.01"));
        assert_eq!(found.label.as_deref(), Some("RadixUDF"));
        assert_eq!(found.uuid.as_deref(), Some("6ae66cea51fa2a06"));
        assert_eq!(found.block_size, Some(2048));
    }

    #[test]
    fn finds_iso9660_and_its_joliet_label() {
        let mut image = vec![0u8; 40 * 2048];
        let pvd = 16 * 2048;
        image[pvd] = 1;
        image[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        image[pvd + 40..pvd + 72].copy_from_slice(b"PLAIN_LABEL                     ");
        image[pvd + 128..pvd + 130].copy_from_slice(&2048u16.to_le_bytes());
        image[pvd + 813..pvd + 829].copy_from_slice(b"2026103112000000");
        image[pvd + 830..pvd + 846].copy_from_slice(b"0000000000000000");
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!((found.kind, found.version.as_str()), ("iso9660", ""));
        assert_eq!(found.label.as_deref(), Some("PLAIN_LABEL"));
        assert_eq!(found.uuid.as_deref(), Some("2026-10-31-12-00-00-00"));
        assert_eq!(found.block_size, Some(2048));
        let svd = 17 * 2048;
        image[svd] = 2;
        image[svd + 1..svd + 6].copy_from_slice(b"CD001");
        image[svd + 88..svd + 91].copy_from_slice(b"%/E");
        let label: Vec<u8> = "Été disc".encode_utf16().flat_map(u16::to_be_bytes).collect();
        image[svd + 40..svd + 72].fill(0);
        image[svd + 40..svd + 40 + label.len()].copy_from_slice(&label);
        image[18 * 2048] = 255;
        image[18 * 2048 + 1..18 * 2048 + 6].copy_from_slice(b"CD001");
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!(found.version, "Joliet Extension");
        assert_eq!(found.label.as_deref(), Some("Été disc"));
    }

    #[test]
    fn finds_ntfs_and_its_serial_and_label() {
        let mut image = vec![0u8; 64 * 1024];
        image[3..11].copy_from_slice(b"NTFS    ");
        image[11..13].copy_from_slice(&512u16.to_le_bytes());
        // Clusters of 4 KiB, the master file table at the third, and
        // records of 1 KiB.
        image[13] = 8;
        image[48..56].copy_from_slice(&2u64.to_le_bytes());
        image[64] = 0xf6;
        image[72..80].copy_from_slice(&0x7139_9978_54d4_9d57u64.to_le_bytes());
        image[510..512].copy_from_slice(&[0x55, 0xaa]);
        let rec = &mut image[2 * 4096 + 3 * 1024..][..1024];
        rec[..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        rec[6..8].copy_from_slice(&3u16.to_le_bytes());
        rec[20..22].copy_from_slice(&0x38u16.to_le_bytes());
        // The update sequence number at each 512 bytes' end; the zeros
        // that were there are kept after it.
        rec[0x30..0x32].copy_from_slice(&[7, 0]);
        rec[510..512].copy_from_slice(&[7, 0]);
        rec[1022..1024].copy_from_slice(&[7, 0]);
        let mut name = vec![0x60, 0, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        name.extend_from_slice(&8u32.to_le_bytes());
        name.extend_from_slice(&0x18u16.to_le_bytes());
        name.extend_from_slice(&[0, 0]);
        name.extend("data".encode_utf16().flat_map(u16::to_le_bytes));
        rec[0x38..0x58].copy_from_slice(&name);
        rec[0x58..0x5c].copy_from_slice(&[0xff; 4]);
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!((found.kind, found.version.as_str()), ("ntfs", ""));
        assert_eq!((found.label.as_deref(), found.block_size), (Some("data"), Some(512)));
        assert_eq!(found.uuid.as_deref(), Some("7139997854D49D57"));
        // FAT takes it for none of its.
        assert_eq!(fat(&image), None);
    }

    #[test]
    fn finds_btrfs_ext4_and_swap() {
        let mut image = vec![0u8; 128 * 1024];
        let sb = 64 * 1024;
        image[sb + 64..sb + 72].copy_from_slice(b"_BHRfS_M");
        image[sb + 32..sb + 48].copy_from_slice(&[0x11; 16]);
        image[sb + 267..sb + 283].copy_from_slice(&[0x22; 16]);
        image[sb + 299..sb + 303].copy_from_slice(b"data");
        image[sb + 144..sb + 148].copy_from_slice(&4096u32.to_le_bytes());
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!(found.kind, "btrfs");
        assert_eq!(found.uuid.as_deref(), Some("11111111-1111-1111-1111-111111111111"));
        assert_eq!(found.uuid_sub.as_deref(), Some("22222222-2222-2222-2222-222222222222"));
        assert_eq!((found.label.as_deref(), found.block_size), (Some("data"), Some(4096)));

        let mut image = vec![0u8; 128 * 1024];
        image[1024 + 56..1024 + 58].copy_from_slice(&0xef53u16.to_le_bytes());
        image[1024 + 24] = 2;
        image[1024 + 92] = 0x4;
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!((found.kind, found.block_size), ("ext3", Some(4096)));
        image[1024 + 96] = 0x40;
        assert_eq!(filesystem(&mut reading(&image)).unwrap().kind, "ext4");

        let mut image = vec![0u8; 128 * 1024];
        image[4086..4096].copy_from_slice(b"SWAPSPACE2");
        image[1052..1056].copy_from_slice(b"swap");
        let found = filesystem(&mut reading(&image)).unwrap();
        assert_eq!((found.kind, found.label.as_deref()), ("swap", Some("swap")));
    }

    #[test]
    fn reads_a_gpt_partitions_guid_and_label() {
        let mut image = vec![0u8; 64 * 1024];
        image[512..520].copy_from_slice(b"EFI PART");
        let disk: Vec<u8> = (0..16).collect();
        image[512 + 56..512 + 72].copy_from_slice(&disk);
        image[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes());
        image[512 + 80..512 + 84].copy_from_slice(&128u32.to_le_bytes());
        image[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes());
        let entry = 1024 + 128;
        image[entry + 16..entry + 32].copy_from_slice(&disk);
        for (i, c) in "root".encode_utf16().enumerate() {
            image[entry + 56 + 2 * i..entry + 58 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        let found = table(&mut reading(&image), 512).unwrap();
        assert_eq!(found.uuid, "03020100-0504-0706-0809-0a0b0c0d0e0f");
        let ids = partition_ids(&mut reading(&image), 512, 2);
        assert_eq!(ids.uuid.as_deref(), Some("03020100-0504-0706-0809-0a0b0c0d0e0f"));
        assert_eq!(ids.label.as_deref(), Some("root"));
    }

    #[test]
    fn partitions_are_on_their_disks() {
        assert_eq!(disk_of("sda1"), "sda");
        assert_eq!(disk_of("vdb12"), "vdb");
        assert_eq!(disk_of("nvme0n1p3"), "nvme0n1");
    }
}
