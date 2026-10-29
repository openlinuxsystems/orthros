// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! SCSI as USB sticks, card readers and USB disks speak it (Phase 11b,
//! ADR 0161): the commands of *SCSI Primary Commands* (SPC-6) and *SCSI
//! Block Commands* (SBC-4) that the kernel sends a block device, and what
//! the device answers.
//!
//! - [`Cdb`]: a command descriptor block, and the commands the kernel
//!   makes.
//! - [`Inquiry`], [`Capacity`] and [`write_protected`]: what a device
//!   says it is.
//! - [`Sense`]: why a command failed, in fixed or descriptor format, and
//!   what that means for the request ([`Meaning`]).
//! - [`Transport`]: what carries the commands to the device (USB's
//!   Bulk-Only Transport, `radix_usb::storage`), and [`probe`], which finds
//!   out what the disk behind one is, as Linux's `sd` does when it scans a
//!   USB stick.
//!
//! The crate knows nothing of the kernel or of USB, so its tests run on
//! the host. Nothing a device sends is trusted without checking its
//! length.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

mod probe;
#[cfg(test)]
mod tests;

pub use probe::{Absent, Found, probe};

/// Operation codes.
pub mod op {
    /// TEST UNIT READY: whether the device can take commands for its
    /// medium.
    pub const TEST_UNIT_READY: u8 = 0x00;
    /// REQUEST SENSE: why the last command failed.
    pub const REQUEST_SENSE: u8 = 0x03;
    /// INQUIRY: what the device is.
    pub const INQUIRY: u8 = 0x12;
    /// MODE SENSE(6): the device's parameters.
    pub const MODE_SENSE_6: u8 = 0x1a;
    /// START STOP UNIT: spins the medium up or down, or ejects it.
    pub const START_STOP_UNIT: u8 = 0x1b;
    /// READ CAPACITY(10): the last block and the block length.
    pub const READ_CAPACITY_10: u8 = 0x25;
    /// READ(10).
    pub const READ_10: u8 = 0x28;
    /// WRITE(10).
    pub const WRITE_10: u8 = 0x2a;
    /// SYNCHRONIZE CACHE(10): makes every completed write durable.
    pub const SYNCHRONIZE_CACHE_10: u8 = 0x35;
    /// READ(16).
    pub const READ_16: u8 = 0x88;
    /// WRITE(16).
    pub const WRITE_16: u8 = 0x8a;
    /// SERVICE ACTION IN(16), whose action [`READ_CAPACITY_16`] is.
    pub const SERVICE_ACTION_IN_16: u8 = 0x9e;
    /// READ CAPACITY(16)'s service action.
    pub const READ_CAPACITY_16: u8 = 0x10;
    /// REPORT LUNS: the logical units a target has.
    pub const REPORT_LUNS: u8 = 0xa0;
}

/// Peripheral device types, of an [`Inquiry`].
pub mod device_type {
    /// A direct-access block device: a disk, a stick, a card.
    pub const DISK: u8 = 0x00;
    /// A CD or DVD drive (MMC).
    pub const CD: u8 = 0x05;
    /// An optical memory device, read and written as a disk.
    pub const OPTICAL_MEMORY: u8 = 0x07;
    /// A simplified direct-access device (RBC), as some cards are.
    pub const RBC: u8 = 0x0e;
}

/// Sense keys.
pub mod key {
    /// Nothing to report.
    pub const NO_SENSE: u8 = 0x0;
    /// The command worked after the device recovered from an error.
    pub const RECOVERED_ERROR: u8 = 0x1;
    /// The device cannot take commands for its medium now.
    pub const NOT_READY: u8 = 0x2;
    /// The medium failed.
    pub const MEDIUM_ERROR: u8 = 0x3;
    /// The device failed.
    pub const HARDWARE_ERROR: u8 = 0x4;
    /// The command, or a field of it, is not one the device takes.
    pub const ILLEGAL_REQUEST: u8 = 0x5;
    /// Something changed: a reset, a new medium, new parameters.
    pub const UNIT_ATTENTION: u8 = 0x6;
    /// The medium is protected from the command (write-protected).
    pub const DATA_PROTECT: u8 = 0x7;
    /// A write-once medium's blank or written area was met.
    pub const BLANK_CHECK: u8 = 0x8;
    /// The device ended the command.
    pub const ABORTED_COMMAND: u8 = 0xb;
    /// A write reached the end of the medium.
    pub const VOLUME_OVERFLOW: u8 = 0xd;
    /// A verify found other data.
    pub const MISCOMPARE: u8 = 0xe;
}

/// The longest command descriptor block, in bytes.
pub const CDB_MAX: usize = 16;
/// How many bytes of INQUIRY data the kernel asks for: the standard
/// header and its strings. Linux's usb-storage asks for no more, since
/// some devices fail a longer request.
pub const INQUIRY_LENGTH: u8 = 36;
/// How many bytes of sense data the kernel asks for: fixed-format sense
/// with its additional sense code.
pub const SENSE_LENGTH: u8 = 18;
/// READ CAPACITY(10)'s data.
pub const CAPACITY_10_LENGTH: usize = 8;
/// How many bytes of READ CAPACITY(16) data the kernel asks for.
pub const CAPACITY_16_LENGTH: u8 = 32;
/// How many bytes MODE SENSE(6) of every page asks for: what usb-storage
/// asks for, since many USB devices take only that length.
pub const MODE_SENSE_ALL_LENGTH: u8 = 192;
/// Every mode page, for MODE SENSE.
pub const ALL_PAGES: u8 = 0x3f;
/// The most blocks a READ(10) or WRITE(10) moves.
pub const MAX_BLOCKS_10: u32 = 0xffff;

/// A command descriptor block: the command, as the transport carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cdb {
    bytes: [u8; CDB_MAX],
    len: u8,
}

impl Cdb {
    /// The block of `bytes`, at most [`CDB_MAX`].
    pub fn new(bytes: &[u8]) -> Cdb {
        let len = bytes.len().min(CDB_MAX);
        let mut all = [0; CDB_MAX];
        all[..len].copy_from_slice(&bytes[..len]);
        Cdb { bytes: all, len: len as u8 }
    }

    /// Its bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Its operation code.
    pub fn opcode(&self) -> u8 {
        self.bytes[0]
    }

    /// TEST UNIT READY.
    pub fn test_unit_ready() -> Cdb {
        Cdb::new(&[op::TEST_UNIT_READY, 0, 0, 0, 0, 0])
    }

    /// REQUEST SENSE of `length` bytes, in fixed format.
    pub fn request_sense(length: u8) -> Cdb {
        Cdb::new(&[op::REQUEST_SENSE, 0, 0, 0, length, 0])
    }

    /// INQUIRY of `length` bytes of standard data.
    pub fn inquiry(length: u8) -> Cdb {
        Cdb::new(&[op::INQUIRY, 0, 0, 0, length, 0])
    }

    /// MODE SENSE(6) of mode page `page` (its current values, with block
    /// descriptors), `length` bytes.
    pub fn mode_sense_6(page: u8, length: u8) -> Cdb {
        Cdb::new(&[op::MODE_SENSE_6, 0, page & 0x3f, 0, length, 0])
    }

    /// START STOP UNIT: starts the medium, or stops it; with `eject`,
    /// loads it (`start`) or ejects it. Answers once it is done.
    pub fn start_stop_unit(start: bool, eject: bool) -> Cdb {
        let bits = u8::from(eject) << 1 | u8::from(start);
        Cdb::new(&[op::START_STOP_UNIT, 0, 0, 0, bits, 0])
    }

    /// READ CAPACITY(10).
    pub fn read_capacity_10() -> Cdb {
        Cdb::new(&[op::READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    }

    /// READ CAPACITY(16), `length` bytes.
    pub fn read_capacity_16(length: u8) -> Cdb {
        let mut bytes = [0; 16];
        bytes[0] = op::SERVICE_ACTION_IN_16;
        bytes[1] = op::READ_CAPACITY_16;
        bytes[13] = length;
        Cdb::new(&bytes)
    }

    /// READ of `blocks` blocks from block `lba`: READ(10) unless `sixteen`
    /// (a disk with more blocks than 32-bit addresses reach), or the
    /// request needs READ(16). `None` for no blocks, or more than a
    /// READ(16) moves.
    pub fn read(lba: u64, blocks: u32, sixteen: bool) -> Option<Cdb> {
        Cdb::transfer(op::READ_10, op::READ_16, lba, blocks, sixteen)
    }

    /// WRITE of `blocks` blocks to block `lba`, as [`Cdb::read`].
    pub fn write(lba: u64, blocks: u32, sixteen: bool) -> Option<Cdb> {
        Cdb::transfer(op::WRITE_10, op::WRITE_16, lba, blocks, sixteen)
    }

    fn transfer(ten: u8, sixteen_op: u8, lba: u64, blocks: u32, sixteen: bool) -> Option<Cdb> {
        if blocks == 0 {
            return None;
        }
        if !sixteen
            && blocks <= MAX_BLOCKS_10
            && let Ok(lba) = u32::try_from(lba)
        {
            let [a, b, c, d] = lba.to_be_bytes();
            let [h, l] = (blocks as u16).to_be_bytes();
            return Some(Cdb::new(&[ten, 0, a, b, c, d, 0, h, l, 0]));
        }
        let mut bytes = [0; 16];
        bytes[0] = sixteen_op;
        bytes[2..10].copy_from_slice(&lba.to_be_bytes());
        bytes[10..14].copy_from_slice(&blocks.to_be_bytes());
        Some(Cdb::new(&bytes))
    }

    /// SYNCHRONIZE CACHE(10) of every block.
    pub fn synchronize_cache() -> Cdb {
        Cdb::new(&[op::SYNCHRONIZE_CACHE_10, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    }

    /// REPORT LUNS of every logical unit, `length` bytes (SPC-6, 6.35).
    pub fn report_luns(length: u32) -> Cdb {
        let mut bytes = [0; 12];
        bytes[0] = op::REPORT_LUNS;
        bytes[6..10].copy_from_slice(&length.to_be_bytes());
        Cdb::new(&bytes)
    }
}

/// The logical units a REPORT LUNS answer `bytes` lists that are numbered
/// below 256 in SAM's single level format (peripheral device addressing,
/// bus 0), as Linux's `scsilun_to_int` reads them: those a USB device's
/// IUs can name. Others are left out.
pub fn reported_luns(bytes: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let listed = bytes.get(0..4).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let end = bytes.len().min(8usize.saturating_add(listed as usize));
    bytes
        .get(8..end)
        .unwrap_or(&[])
        .as_chunks::<8>()
        .0
        .iter()
        .filter_map(|lun| (lun[0] == 0 && lun[2..].iter().all(|&b| b == 0)).then_some(lun[1]))
}

/// Text a device gave: ASCII padded with spaces, without the padding;
/// empty if it is not printable ASCII.
pub fn text(bytes: &[u8]) -> &str {
    let text = core::str::from_utf8(bytes).unwrap_or("");
    let text = text.trim_matches(|c: char| c == ' ' || c == '\0');
    if text.bytes().all(|b| (0x20..0x7f).contains(&b)) { text } else { "" }
}

/// What a device says it is: standard INQUIRY data (SPC-6, 6.7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inquiry {
    /// Its peripheral qualifier: 0 if a device is there.
    pub qualifier: u8,
    /// Its peripheral device type ([`device_type`]).
    pub device_type: u8,
    /// Whether its medium can be taken out (RMB).
    pub removable: bool,
    /// The version of SPC it follows.
    pub version: u8,
    /// Its vendor, padded.
    pub vendor: [u8; 8],
    /// Its product, padded.
    pub product: [u8; 16],
    /// Its product's revision, padded.
    pub revision: [u8; 4],
}

impl Inquiry {
    /// Reads INQUIRY data, which may be short: its strings are then
    /// empty. `None` with less than the first two bytes.
    pub fn parse(bytes: &[u8]) -> Option<Inquiry> {
        if bytes.len() < 2 {
            return None;
        }
        let mut inquiry = Inquiry {
            qualifier: bytes[0] >> 5,
            device_type: bytes[0] & 0x1f,
            removable: bytes[1] & 0x80 != 0,
            version: bytes.get(2).copied().unwrap_or(0),
            vendor: [b' '; 8],
            product: [b' '; 16],
            revision: [b' '; 4],
        };
        if bytes.len() >= 36 {
            inquiry.vendor.copy_from_slice(&bytes[8..16]);
            inquiry.product.copy_from_slice(&bytes[16..32]);
            inquiry.revision.copy_from_slice(&bytes[32..36]);
        }
        Some(inquiry)
    }

    /// Whether a device is there: the qualifier says one is connected.
    pub fn present(&self) -> bool {
        self.qualifier == 0
    }

    /// Whether it is read and written as a disk.
    pub fn is_disk(&self) -> bool {
        matches!(
            self.device_type,
            device_type::DISK | device_type::OPTICAL_MEMORY | device_type::RBC
        )
    }

    /// Its vendor, without the padding.
    pub fn vendor(&self) -> &str {
        text(&self.vendor)
    }

    /// Its product, without the padding.
    pub fn product(&self) -> &str {
        text(&self.product)
    }

    /// Its revision, without the padding.
    pub fn revision(&self) -> &str {
        text(&self.revision)
    }
}

/// How big a medium is: READ CAPACITY's data (SBC-4, 5.20 and 5.21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// How many logical blocks it has.
    pub blocks: u64,
    /// A logical block's length, in bytes.
    pub block_length: u32,
    /// Logical blocks in a physical block, as a power of two.
    pub physical_exponent: u8,
}

impl Capacity {
    /// Reads READ CAPACITY(10) data. Its last block is `u32::MAX` when the
    /// medium has more blocks than it can say, which [`Capacity::parse_16`]
    /// then says.
    pub fn parse_10(bytes: &[u8]) -> Option<Capacity> {
        let last = u32::from_be_bytes(bytes.get(0..4)?.try_into().ok()?);
        let block_length = u32::from_be_bytes(bytes.get(4..8)?.try_into().ok()?);
        Some(Capacity { blocks: u64::from(last) + 1, block_length, physical_exponent: 0 })
    }

    /// Whether READ CAPACITY(10) said its last block is past what it can
    /// say.
    pub fn needs_16(&self) -> bool {
        self.blocks > u64::from(u32::MAX)
    }

    /// Reads READ CAPACITY(16) data; the physical block exponent is 0 if
    /// it is short.
    pub fn parse_16(bytes: &[u8]) -> Option<Capacity> {
        let last = u64::from_be_bytes(bytes.get(0..8)?.try_into().ok()?);
        let block_length = u32::from_be_bytes(bytes.get(8..12)?.try_into().ok()?);
        let physical_exponent = bytes.get(13).map_or(0, |b| b & 0xf);
        Some(Capacity { blocks: last.checked_add(1)?, block_length, physical_exponent })
    }

    /// Whether the kernel can use it: a block length of 512, 1024, 2048 or
    /// 4096 bytes, as Linux takes, and at least one block.
    pub fn usable(&self) -> bool {
        matches!(self.block_length, 512 | 1024 | 2048 | 4096) && self.blocks > 0
    }

    /// A physical block's length, in bytes, at most 64 KiB.
    pub fn physical_block(&self) -> u32 {
        self.block_length
            << self.physical_exponent.min(16 - self.block_length.trailing_zeros() as u8)
    }

    /// Its size in bytes.
    pub fn bytes(&self) -> u64 {
        self.blocks.saturating_mul(u64::from(self.block_length))
    }
}

/// Whether a medium is write-protected, from MODE SENSE(6)'s header: its
/// device-specific parameter's WP bit. `None` if the header is short.
pub fn write_protected(header: &[u8]) -> Option<bool> {
    header.get(2).map(|parameter| parameter & 0x80 != 0)
}

/// The caching mode page's code.
pub const CACHING_PAGE: u8 = 0x08;

/// Whether the device keeps writes in a cache that SYNCHRONIZE CACHE
/// empties, from MODE SENSE(6) data of every page: the caching page's
/// WCE bit (SBC-4, 6.5.5). `None` if the data has no caching page, as
/// Linux finds it in the same answer for a USB disk.
pub fn write_cache(data: &[u8]) -> Option<bool> {
    // The mode data length counts the bytes after itself.
    let end = data.len().min(usize::from(*data.first()?) + 1);
    let mut at = 4 + usize::from(*data.get(3)?);
    while at + 2 <= end {
        let (code, length) = (data[at] & 0x3f, usize::from(data[at + 1]));
        if code == CACHING_PAGE {
            return data.get(at + 2).filter(|_| at + 2 < end).map(|flags| flags & 0x04 != 0);
        }
        // A page of code 0 ends the list: vendor data follows.
        if code == 0 {
            return None;
        }
        at += 2 + length;
    }
    None
}

/// Why a command failed: the sense key and the additional sense code and
/// qualifier (SPC-6, 4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense {
    /// The sense key ([`key`]).
    pub key: u8,
    /// The additional sense code.
    pub asc: u8,
    /// Its qualifier.
    pub ascq: u8,
}

/// What a failed command's [`Sense`] means for the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Meaning {
    /// Nothing went wrong, or the device recovered from it.
    Fine,
    /// The device is getting ready; it takes the command a little later.
    BecomingReady,
    /// It is not ready until a START STOP UNIT starts it.
    NeedsStart,
    /// It has no medium in it.
    NoMedium,
    /// Something changed (a reset, a new medium); the command goes again.
    Attention,
    /// The device does not take the command, or a field of it.
    Unsupported,
    /// The medium is write-protected.
    WriteProtected,
    /// The device ended the command; it may go again.
    Aborted,
    /// The medium or the device failed.
    Error,
}

/// Words for the additional sense codes the log meets most, by code and
/// qualifier.
const CODES: [((u8, u8), &str); 18] = [
    ((0x00, 0x00), "no additional sense information"),
    ((0x04, 0x00), "not ready, cause not reportable"),
    ((0x04, 0x01), "becoming ready"),
    ((0x04, 0x02), "an initializing command is required"),
    ((0x04, 0x03), "manual intervention is required"),
    ((0x0c, 0x00), "write error"),
    ((0x11, 0x00), "unrecovered read error"),
    ((0x20, 0x00), "invalid command operation code"),
    ((0x21, 0x00), "logical block address out of range"),
    ((0x24, 0x00), "invalid field in the command"),
    ((0x25, 0x00), "logical unit not supported"),
    ((0x27, 0x00), "write protected"),
    ((0x28, 0x00), "the medium may have changed"),
    ((0x29, 0x00), "power on or reset"),
    ((0x3a, 0x00), "medium not present"),
    ((0x3a, 0x01), "medium not present, tray closed"),
    ((0x3a, 0x02), "medium not present, tray open"),
    ((0x44, 0x00), "internal target failure"),
];

impl Sense {
    /// Reads sense data in fixed (70h, 71h) or descriptor (72h, 73h)
    /// format. `None` for another format, or data too short to hold the
    /// sense key.
    pub fn parse(bytes: &[u8]) -> Option<Sense> {
        match bytes.first()? & 0x7f {
            0x70 | 0x71 => Some(Sense {
                key: bytes.get(2)? & 0xf,
                asc: bytes.get(12).copied().unwrap_or(0),
                ascq: bytes.get(13).copied().unwrap_or(0),
            }),
            0x72 | 0x73 => Some(Sense {
                key: bytes.get(1)? & 0xf,
                asc: bytes.get(2).copied().unwrap_or(0),
                ascq: bytes.get(3).copied().unwrap_or(0),
            }),
            _ => None,
        }
    }

    /// What it means for the request, as Linux's SCSI layer reads it.
    pub fn meaning(&self) -> Meaning {
        match (self.key, self.asc, self.ascq) {
            (key::NO_SENSE | key::RECOVERED_ERROR, _, _) => Meaning::Fine,
            (key::NOT_READY, 0x3a, _) => Meaning::NoMedium,
            (key::NOT_READY, 0x04, 0x01) => Meaning::BecomingReady,
            // Manual intervention, standby, unavailable, sanitize: no
            // waiting makes it ready.
            (key::NOT_READY, 0x04, 0x03 | 0x0b | 0x0c | 0x1b) => Meaning::Error,
            (key::NOT_READY, _, _) => Meaning::NeedsStart,
            (key::UNIT_ATTENTION, _, _) => Meaning::Attention,
            (key::ILLEGAL_REQUEST, _, _) => Meaning::Unsupported,
            (key::DATA_PROTECT, _, _) => Meaning::WriteProtected,
            (key::ABORTED_COMMAND, _, _) => Meaning::Aborted,
            _ => Meaning::Error,
        }
    }

    /// The sense key's name.
    pub fn key_name(&self) -> &'static str {
        match self.key {
            key::NO_SENSE => "no sense",
            key::RECOVERED_ERROR => "recovered error",
            key::NOT_READY => "not ready",
            key::MEDIUM_ERROR => "medium error",
            key::HARDWARE_ERROR => "hardware error",
            key::ILLEGAL_REQUEST => "illegal request",
            key::UNIT_ATTENTION => "unit attention",
            key::DATA_PROTECT => "data protect",
            key::BLANK_CHECK => "blank check",
            key::ABORTED_COMMAND => "aborted command",
            key::VOLUME_OVERFLOW => "volume overflow",
            key::MISCOMPARE => "miscompare",
            _ => "sense key",
        }
    }
}

impl fmt::Display for Sense {
    /// `not ready, medium not present (3a/00)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key_name())?;
        if let Some((_, words)) = CODES.iter().find(|(code, _)| *code == (self.asc, self.ascq)) {
            write!(f, ", {words}")?;
        }
        write!(f, " ({:02x}/{:02x})", self.asc, self.ascq)
    }
}

/// What a command moves, and where.
#[derive(Debug)]
pub enum Data<'a> {
    /// Nothing.
    None,
    /// Up to the slice's length from the device, into it.
    In(&'a mut [u8]),
    /// The slice's bytes, to the device.
    Out(&'a [u8]),
    /// Into memory (`into_host`) or from it, which the transport reaches
    /// by DMA: buffers by physical address and length, each within a page.
    Frames {
        /// Whether the data comes from the device.
        into_host: bool,
        /// The buffers.
        frames: &'a [(u64, u32)],
    },
}

impl Data<'_> {
    /// How many bytes it moves.
    pub fn len(&self) -> usize {
        match self {
            Data::None => 0,
            Data::In(bytes) => bytes.len(),
            Data::Out(bytes) => bytes.len(),
            Data::Frames { frames, .. } => frames.iter().map(|&(_, len)| len as usize).sum(),
        }
    }

    /// Whether it moves nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether it comes from the device.
    pub fn into_host(&self) -> bool {
        match self {
            Data::In(_) => true,
            Data::Frames { into_host, .. } => *into_host,
            Data::None | Data::Out(_) => false,
        }
    }
}

/// How a command the transport carried ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// It worked; `residue` bytes of its data did not move.
    Good {
        /// The bytes that did not move.
        residue: u32,
    },
    /// It failed, with why, if the device said.
    Failed(Option<Sense>),
}

/// Why the transport could not carry a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The device is gone: unplugged.
    Gone,
    /// The transport failed, and was reset: the command may go again.
    Failed(&'static str),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Gone => f.write_str("it was unplugged"),
            TransportError::Failed(why) => f.write_str(why),
        }
    }
}

/// What carries commands to a device's logical units.
pub trait Transport {
    /// Sends `cdb` to logical unit `lun` with `data`, waiting at most
    /// `timeout_ms` for it, and gets the sense data of a command that
    /// failed.
    fn command(
        &mut self,
        lun: u8,
        cdb: &Cdb,
        data: Data<'_>,
        timeout_ms: u32,
    ) -> Result<Reply, TransportError>;
}
