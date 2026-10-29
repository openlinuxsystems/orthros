// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ATA commands as an AHCI port sends them (ATA Command Set, ACS-4; Serial
//! ATA 3.x for the frames): the register frame a command travels in, and
//! what a disk says of itself in answer to IDENTIFY DEVICE. A disk's own
//! record of its health, SMART, is read with the commands of ATA/ATAPI-8
//! (ACS-4 keeps SMART READ DATA, and the thresholds are in ATA/ATAPI-8);
//! what its bytes mean is left to the program that asks.

/// ATA command codes.
pub mod command {
    /// DATA SET MANAGEMENT: with feature bit 0, TRIM (discard) ranges.
    pub const DATA_SET_MANAGEMENT: u8 = 0x06;
    /// READ DMA EXT: reads sectors, with 48-bit addresses.
    pub const READ_DMA_EXT: u8 = 0x25;
    /// READ LOG EXT: reads a page of a log.
    pub const READ_LOG_EXT: u8 = 0x2f;
    /// WRITE DMA EXT: writes sectors, with 48-bit addresses.
    pub const WRITE_DMA_EXT: u8 = 0x35;
    /// WRITE DMA FUA EXT: writes sectors through the disk's write cache.
    pub const WRITE_DMA_FUA_EXT: u8 = 0x3d;
    /// READ FPDMA QUEUED: a native command queuing (NCQ) read.
    pub const READ_FPDMA_QUEUED: u8 = 0x60;
    /// WRITE FPDMA QUEUED: an NCQ write.
    pub const WRITE_FPDMA_QUEUED: u8 = 0x61;
    /// FLUSH CACHE EXT: makes every completed write durable.
    pub const FLUSH_CACHE_EXT: u8 = 0xea;
    /// SMART: with a feature code ([`super::smart`]), the disk's own
    /// record of its health.
    pub const SMART: u8 = 0xb0;
    /// IDENTIFY DEVICE: 512 bytes of what the disk is.
    pub const IDENTIFY_DEVICE: u8 = 0xec;
}

/// SMART's feature codes, for the features register.
pub mod smart {
    /// SMART READ DATA: 512 bytes, the disk's attributes and their values.
    pub const READ_DATA: u8 = 0xd0;
    /// SMART READ ATTRIBUTE THRESHOLDS: 512 bytes, the value at or below
    /// which each attribute says the disk is failing. Obsolete since
    /// ACS-1, yet disks still answer it, and smartctl reads it.
    pub const READ_THRESHOLDS: u8 = 0xd1;
    /// What every SMART command carries in the LBA mid and high
    /// registers, 4Fh and C2h, as the block address's bytes 1 and 2.
    pub const SIGNATURE: u64 = 0xc2_4f00;
}

/// The log page that names the command an NCQ error was for (NCQ Command
/// Error, log address 10h). After an NCQ error a disk aborts every command
/// it holds and takes no other until the page is read.
pub const LOG_NCQ_ERROR: u8 = 0x10;

/// The device register's LBA bit: the address is a block number.
pub const DEVICE_LBA: u8 = 1 << 6;
/// The device register's FUA bit, in an NCQ write: write through the
/// disk's cache.
pub const DEVICE_FUA: u8 = 1 << 7;
/// Feature bit 0 of DATA SET MANAGEMENT: the ranges are to be trimmed.
pub const DSM_TRIM: u16 = 1;

/// The status register's error bit.
pub const STATUS_ERR: u8 = 1 << 0;
/// The status register's data request bit: the disk waits for a data
/// transfer.
pub const STATUS_DRQ: u8 = 1 << 3;
/// The status register's busy bit.
pub const STATUS_BSY: u8 = 1 << 7;

/// The type of a register frame from host to device.
pub const FIS_H2D: u8 = 0x27;
/// The length of one, in bytes.
pub const FIS_H2D_LEN: usize = 20;
/// Its second byte's command bit: the frame carries a command (not a
/// device control register change).
const FIS_COMMAND: u8 = 1 << 7;

/// The most sectors one TRIM range covers.
pub const TRIM_RANGE_MAX: u64 = 0xffff;
/// The bytes of one TRIM range entry.
pub const TRIM_ENTRY: usize = 8;

/// The registers of an ATA command, which a register frame carries: the
/// 48-bit address, the 16-bit count and features, and the device register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Taskfile {
    /// The command code.
    pub command: u8,
    /// The features register.
    pub features: u16,
    /// The block address, 48 bits of it.
    pub lba: u64,
    /// The count register.
    pub count: u16,
    /// The device register.
    pub device: u8,
}

impl Taskfile {
    /// The register frame from host to device that carries the command
    /// (SATA 3.x, 10.5.5).
    pub fn fis(&self) -> [u8; FIS_H2D_LEN] {
        let lba = self.lba.to_le_bytes();
        let features = self.features.to_le_bytes();
        let count = self.count.to_le_bytes();
        [
            FIS_H2D,
            FIS_COMMAND,
            self.command,
            features[0],
            lba[0],
            lba[1],
            lba[2],
            self.device,
            lba[3],
            lba[4],
            lba[5],
            features[1],
            count[0],
            count[1],
            0,
            0,
            0,
            0,
            0,
            0,
        ]
    }

    /// READ DMA EXT or WRITE DMA EXT (or WRITE DMA FUA EXT, with `fua`)
    /// of `count` sectors from `lba`: one command at a time. A count of 0
    /// means 65,536 sectors.
    pub fn dma(write: bool, fua: bool, lba: u64, count: u16) -> Taskfile {
        let command = match (write, fua) {
            (false, _) => command::READ_DMA_EXT,
            (true, false) => command::WRITE_DMA_EXT,
            (true, true) => command::WRITE_DMA_FUA_EXT,
        };
        Taskfile { command, features: 0, lba, count, device: DEVICE_LBA }
    }

    /// READ FPDMA QUEUED or WRITE FPDMA QUEUED of `count` sectors from
    /// `lba` as NCQ tag `tag` (below 32): the count goes in the features
    /// register and the tag in the count register's bits 7 to 3. A count
    /// of 0 means 65,536 sectors.
    pub fn queued(write: bool, fua: bool, lba: u64, count: u16, tag: u8) -> Taskfile {
        let command = if write { command::WRITE_FPDMA_QUEUED } else { command::READ_FPDMA_QUEUED };
        let device = DEVICE_LBA | if write && fua { DEVICE_FUA } else { 0 };
        Taskfile { command, features: count, lba, count: u16::from(tag & 0x1f) << 3, device }
    }

    /// FLUSH CACHE EXT.
    pub fn flush() -> Taskfile {
        Taskfile { command: command::FLUSH_CACHE_EXT, ..Taskfile::default() }
    }

    /// DATA SET MANAGEMENT with TRIM, its ranges in `blocks` 512-byte
    /// blocks of data.
    pub fn trim(blocks: u16) -> Taskfile {
        Taskfile {
            command: command::DATA_SET_MANAGEMENT,
            features: DSM_TRIM,
            lba: 0,
            count: blocks,
            device: DEVICE_LBA,
        }
    }

    /// READ LOG EXT of one 512-byte page of log `log`.
    pub fn read_log(log: u8) -> Taskfile {
        Taskfile {
            command: command::READ_LOG_EXT,
            features: 0,
            lba: u64::from(log),
            count: 1,
            device: DEVICE_LBA,
        }
    }

    /// IDENTIFY DEVICE.
    pub fn identify() -> Taskfile {
        Taskfile { command: command::IDENTIFY_DEVICE, ..Taskfile::default() }
    }

    /// SMART with feature `feature`, which reads one 512-byte page.
    pub fn smart(feature: u8) -> Taskfile {
        Taskfile {
            command: command::SMART,
            features: u16::from(feature),
            lba: smart::SIGNATURE,
            count: 1,
            device: 0,
        }
    }
}

/// One TRIM range entry: `sectors` sectors (1 to [`TRIM_RANGE_MAX`]) from
/// `lba`, which is below 2^48.
pub fn trim_entry(lba: u64, sectors: u64) -> [u8; TRIM_ENTRY] {
    ((lba & ((1 << 48) - 1)) | (sectors.min(TRIM_RANGE_MAX) << 48)).to_le_bytes()
}

/// The error register's bits that say the disk could not read or find
/// sectors: uncorrectable data (UNC), ID not found (IDNF) and address
/// mark not found (AMNF). A command that failed with one of them fails,
/// as it would fail again; one that failed otherwise (aborted, or with a
/// CRC error on the link) goes again.
pub const ERROR_MEDIA: u8 = (1 << 6) | (1 << 4) | (1 << 0);

/// The command an NCQ Command Error log page names, if it names one: its
/// tag, in the low five bits of the page's first byte, whose top bit (NQ)
/// says that the error was a non-queued command's; and its error
/// register, the page's fourth byte.
pub fn ncq_error(page: &[u8; 512]) -> Option<(u8, u8)> {
    (page[0] & 0x80 == 0).then_some((page[0] & 0x1f, page[3]))
}

/// Why IDENTIFY DEVICE data are not those of a disk this driver uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsupported {
    /// The data's checksum is wrong.
    Checksum,
    /// It is not an ATA device (word 0's bit 15).
    NotAta,
    /// It has no 48-bit addresses, which ATA-6 brought in 2002.
    No48Bit,
    /// Its logical sectors are not 512 or 4096 bytes.
    SectorSize(u32),
    /// It reports no sectors.
    Empty,
}

/// What a disk says of itself in IDENTIFY DEVICE (ACS-4, 7.13.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Its capacity in its logical sectors.
    pub sectors: u64,
    /// Its logical sector size in bytes, which commands count in: 512, or
    /// 4096 on a disk with 4 KiB sectors (4Kn).
    pub logical_sector: u32,
    /// Its physical sector size in bytes: the smallest unit it writes
    /// without reading first, no smaller than a logical sector.
    pub physical_sector: u32,
    /// How many commands it queues (native command queuing), or 0.
    pub queue_depth: u8,
    /// Whether it has a write cache that is on.
    pub write_cache: bool,
    /// Whether it has FLUSH CACHE EXT.
    pub flush: bool,
    /// Whether it writes through its cache on request (FUA).
    pub fua: bool,
    /// How many 512-byte blocks of TRIM ranges one DATA SET MANAGEMENT
    /// command takes, or 0 for no TRIM.
    pub trim_blocks: u16,
    /// Whether it spins: `Some(false)` for solid state, `None` if it
    /// does not say.
    pub rotational: Option<bool>,
    /// Whether it keeps SMART: `Some(true)` if it does and SMART is on,
    /// `Some(false)` if it is off, `None` if the disk has none.
    pub smart: Option<bool>,
    /// Its model, serial number and firmware revision, in ASCII, padded
    /// with spaces.
    pub model: [u8; 40],
    /// Its serial number.
    pub serial: [u8; 20],
    /// Its firmware revision.
    pub firmware: [u8; 8],
}

/// Whether word `w` of `id` has bit `bit` set.
fn bit(id: &[u16; 256], w: usize, bit: u32) -> bool {
    id[w] & (1 << bit) != 0
}

/// Whether a word that marks its group of words as valid does (bits 15
/// and 14 read 01).
fn valid(word: u16) -> bool {
    word & 0xc000 == 0x4000
}

/// The ASCII string in words `from..from + N/2`: each word holds two
/// characters, the first in its high byte.
fn string<const N: usize>(id: &[u16; 256], from: usize) -> [u8; N] {
    let mut out = [b' '; N];
    let printable = |c: u8| if c.is_ascii_graphic() || c == b' ' { c } else { b' ' };
    for (i, pair) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        *pair = id[from + i].to_be_bytes().map(printable);
    }
    out
}

/// Reads IDENTIFY DEVICE data.
pub fn identify(id: &[u16; 256]) -> Result<Identity, Unsupported> {
    // Word 255 holds A5h and a checksum that makes the 512 bytes add up
    // to 0, or no signature, when the disk does not keep one.
    if id[255] & 0xff == 0xa5 {
        let sum = id.iter().fold(0u8, |sum, word| {
            let [low, high] = word.to_le_bytes();
            sum.wrapping_add(low).wrapping_add(high)
        });
        if sum != 0 {
            return Err(Unsupported::Checksum);
        }
    }
    if bit(id, 0, 15) {
        return Err(Unsupported::NotAta);
    }
    // Words 82 to 84 are valid when 83 says so, and 85 to 87 when 87
    // does. 48-bit addresses must be there and on.
    let supported = valid(id[83]);
    let enabled = valid(id[87]);
    let extended = valid(id[84]);
    if !(supported && enabled && bit(id, 83, 10) && bit(id, 86, 10)) {
        return Err(Unsupported::No48Bit);
    }
    let sectors = u64::from(id[100])
        | u64::from(id[101]) << 16
        | u64::from(id[102]) << 32
        | u64::from(id[103]) << 48;
    if sectors == 0 {
        return Err(Unsupported::Empty);
    }
    // Word 106: whether logical sectors are longer than 256 words (their
    // length in words 117 and 118), and how many make a physical one.
    let geometry = id[106];
    let mut logical = 512u32;
    let mut physical = 512u32;
    if valid(geometry) {
        if geometry & (1 << 12) != 0 {
            let words = u32::from(id[117]) | u32::from(id[118]) << 16;
            logical = words.saturating_mul(2);
        }
        if geometry & (1 << 13) != 0 {
            physical = logical.checked_shl(u32::from(geometry & 0xf)).unwrap_or(logical);
        } else {
            physical = logical;
        }
    }
    if logical != 512 && logical != 4096 {
        return Err(Unsupported::SectorSize(logical));
    }
    if !physical.is_power_of_two() || !(logical..=65_536).contains(&physical) {
        physical = logical;
    }
    // Word 76 is all ones or zero on devices that are not Serial ATA.
    let sata = id[76] != 0 && id[76] != 0xffff;
    let queue_depth = if sata && bit(id, 76, 8) { (id[75] & 0x1f) as u8 + 1 } else { 0 };
    let rotational = match id[217] {
        1 => Some(false),
        0x0401..=0xfffe => Some(true),
        _ => None,
    };
    Ok(Identity {
        sectors,
        logical_sector: logical,
        physical_sector: physical,
        queue_depth,
        write_cache: bit(id, 82, 5) && enabled && bit(id, 85, 5),
        flush: bit(id, 83, 13) && bit(id, 86, 13),
        fua: extended && bit(id, 84, 6),
        trim_blocks: if bit(id, 169, 0) { id[105].max(1) } else { 0 },
        rotational,
        smart: (supported && bit(id, 82, 0)).then(|| enabled && bit(id, 85, 0)),
        model: string(id, 27),
        serial: string(id, 10),
        firmware: string(id, 23),
    })
}

/// The words of IDENTIFY DEVICE data from its 512 bytes, little-endian.
pub fn words(bytes: &[u8; 512]) -> [u16; 256] {
    let mut words = [0u16; 256];
    for (word, pair) in words.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *word = u16::from_le_bytes(*pair);
    }
    words
}

/// A string from IDENTIFY DEVICE data with its padding cut off.
pub fn trimmed(text: &[u8]) -> &str {
    let text = core::str::from_utf8(text).unwrap_or("");
    text.trim_matches(' ')
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Whatever a disk answers to IDENTIFY DEVICE, reading it does not
    /// panic, and a disk it accepts has sectors, logical sectors of 512 or
    /// 4096 bytes, a physical sector size the block layer can take, at
    /// most 32 commands queued, and TRIM ranges only where it says it
    /// trims.
    #[kani::proof]
    #[kani::unwind(257)]
    fn identify_reads_any_answer() {
        let id: [u16; 256] = kani::any();
        if let Ok(found) = identify(&id) {
            assert!(found.sectors > 0);
            assert!(found.logical_sector == 512 || found.logical_sector == 4096);
            assert!(found.physical_sector.is_power_of_two());
            assert!((found.logical_sector..=65_536).contains(&found.physical_sector));
            assert!(found.queue_depth <= 32);
            assert!((found.trim_blocks > 0) == (id[169] & 1 != 0));
        }
    }

    /// A TRIM entry holds the range it was made from, for every range a
    /// command takes.
    #[kani::proof]
    fn trim_entries_hold_their_range() {
        let lba: u64 = kani::any_where(|&lba: &u64| lba < 1 << 48);
        let sectors: u64 = kani::any_where(|&s: &u64| (1..=TRIM_RANGE_MAX).contains(&s));
        let entry = u64::from_le_bytes(trim_entry(lba, sectors));
        assert_eq!(entry & ((1 << 48) - 1), lba);
        assert_eq!(entry >> 48, sectors);
    }
}
