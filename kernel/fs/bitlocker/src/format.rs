// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A BitLocker volume's first sector and metadata, and where its sectors
//! are.

use core::fmt;

/// The signature of a BitLocker volume's first sector, where a
/// filesystem's name is, and of its metadata blocks.
pub const SIGNATURE: &[u8; 8] = b"-FVE-FS-";
/// The name a BitLocker To Go volume's first sector has there, as a FAT
/// one does: the FAT before its encrypted volume, for Windows that do not
/// unlock it, is a discovery volume.
pub const TO_GO_SIGNATURE: &[u8; 8] = b"MSWIN4.1";
/// BitLocker's identifier, 4967d63b-2e29-4ad8-8399-f6a339e3d001, as its
/// first sector has it.
pub const BITLOCKER_ID: [u8; 16] = [
    0x3b, 0xd6, 0x67, 0x49, 0x29, 0x2e, 0xd8, 0x4a, 0x83, 0x99, 0xf6, 0xa3, 0x39, 0xe3, 0xd0, 0x01,
];
/// The identifier of a volume of which Windows encrypted only the space
/// in use, 92a84d3b-dd80-4d0e-9e4e-b1e3284eaed8.
pub const USED_SPACE_ID: [u8; 16] = [
    0x3b, 0x4d, 0xa8, 0x92, 0x80, 0xdd, 0x0e, 0x4d, 0x9e, 0x4e, 0xb1, 0xe3, 0x28, 0x4e, 0xae, 0xd8,
];

/// The bytes each metadata block's area takes on Windows 7 and later,
/// which read as zeros on the volume unlocked.
pub const METADATA_AREA: u64 = 0x10000;
/// The bytes before a metadata block's entries: its block header and
/// metadata header.
pub const ENTRIES: usize = BLOCK_HEADER + HEADER;
/// The block header's bytes.
const BLOCK_HEADER: usize = 64;
/// The metadata header's.
const HEADER: usize = 48;
/// The bytes of an entry's header: its size and three types.
const ENTRY: usize = 8;

/// Entry types: what an entry is.
pub mod kind {
    /// A property inside another entry.
    pub const PROPERTY: u16 = 0;
    /// A volume master key, wrapped for one protector.
    pub const VMK: u16 = 2;
    /// The volume's key, wrapped with the volume master key.
    pub const FVEK: u16 = 3;
    /// The volume's description: its computer's name, its own and a date.
    pub const DESCRIPTION: u16 = 7;
    /// Where the encrypted copy of the volume's first sectors is.
    pub const AREAS: u16 = 0xf;
}

/// Value types: what an entry holds.
pub mod value {
    /// A key: its method (u32), then its bytes.
    pub const KEY: u16 = 1;
    /// Text, in UTF-16 with a NUL at its end.
    pub const STRING: u16 = 2;
    /// A stretched key's method (u32) and salt (16 bytes), then an entry.
    pub const STRETCH: u16 = 3;
    /// A key wrapped with AES-CCM: its nonce (12 bytes), tag (16), then
    /// the key encrypted.
    pub const CCM: u16 = 5;
    /// A volume master key: its identifier (16 bytes), a time (8), 2
    /// bytes, its protection (u16), then its properties.
    pub const VMK: u16 = 8;
    /// An offset (u64) and size (u64).
    pub const AREAS: u16 = 0xf;
}

/// How a volume master key is protected.
pub mod protection {
    /// Not at all: BitLocker is suspended, or the volume is being decrypted.
    pub const CLEAR: u16 = 0x0000;
    /// By the TPM.
    pub const TPM: u16 = 0x0100;
    /// By a startup key, on a USB stick.
    pub const STARTUP_KEY: u16 = 0x0200;
    /// By the TPM and a PIN.
    pub const TPM_PIN: u16 = 0x0500;
    /// By a recovery key, 48 digits.
    pub const RECOVERY: u16 = 0x0800;
    /// By a password.
    pub const PASSWORD: u16 = 0x2000;
}

/// What goes wrong reading or unlocking a BitLocker volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The volume is no BitLocker volume.
    NotBitLocker,
    /// Its metadata is damaged, in each of its copies.
    Damaged,
    /// It is BitLocker that is not read: Windows Vista's, or an
    /// encryption method not known.
    NotSupported,
    /// What was given opens none of its protectors.
    WrongKey,
    /// It has no protector of the kind given.
    NoProtector,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::NotBitLocker => "it is not a BitLocker volume",
            Error::Damaged => "its BitLocker metadata is damaged",
            Error::NotSupported => "it is BitLocker of a kind not read (Windows Vista's)",
            Error::WrongKey => "the key does not open it",
            Error::NoProtector => "it cannot be unlocked that way",
        })
    }
}

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// What a volume's first sector says of BitLocker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Volume {
    /// BitLocker of Windows 7 or later, or To Go: the offsets of its
    /// three metadata blocks, and the bytes of its sectors.
    Locked {
        /// Where the metadata blocks are, from the volume's start.
        metadata: [u64; 3],
        /// Its sectors' bytes, as its first sector says: 512 or 4096.
        sector: u64,
    },
    /// Windows Vista's BitLocker, which is not read.
    Vista,
}

/// What the first sector `boot` of a volume says of BitLocker; `None` if
/// it is no BitLocker volume.
pub fn volume(boot: &[u8]) -> Option<Volume> {
    if boot.len() < 512 || boot[510..512] != [0x55, 0xaa] {
        return None;
    }
    let offsets = |at: usize| [u64_at(boot, at), u64_at(boot, at + 8), u64_at(boot, at + 16)];
    let sector = u64::from(u16_at(boot, 11));
    let sector = if matches!(sector, 512 | 1024 | 2048 | 4096) { sector } else { 512 };
    let id = &boot[160..176];
    match &boot[3..11] {
        s if s == SIGNATURE && (id == BITLOCKER_ID || id == USED_SPACE_ID) => {
            Some(Volume::Locked { metadata: offsets(176), sector })
        }
        s if s == SIGNATURE => Some(Volume::Vista),
        s if s == TO_GO_SIGNATURE && boot[424..440] == BITLOCKER_ID => {
            Some(Volume::Locked { metadata: offsets(440), sector })
        }
        _ => None,
    }
}

/// The bytes of a metadata block to read: its size, from its first
/// [`ENTRIES`] bytes `head`, as the block header and metadata header say;
/// `None` if they are not a metadata block's.
pub fn block_size(head: &[u8]) -> Option<usize> {
    if head.len() < ENTRIES || &head[..8] != SIGNATURE {
        return None;
    }
    let size = u32_at(head, BLOCK_HEADER) as usize;
    (HEADER..=METADATA_AREA as usize - BLOCK_HEADER).contains(&size).then_some(BLOCK_HEADER + size)
}

/// A metadata block, read.
#[derive(Clone, Copy, Debug)]
pub struct Metadata<'a> {
    /// The bytes of the volume encrypted, from its start: all of it, but
    /// while Windows encrypts or decrypts it.
    pub encrypted: u64,
    /// Where the encrypted copy of the volume's first sectors is.
    pub header: u64,
    /// The bytes of the copy: the first sectors' that it holds.
    pub header_size: u64,
    /// Where the three metadata blocks are.
    pub blocks: [u64; 3],
    /// The encryption method's number ([`crate::cipher::Method`]).
    pub method: u16,
    /// The volume's identifier.
    pub id: [u8; 16],
    entries: &'a [u8],
}

impl<'a> Metadata<'a> {
    /// The metadata block `block`, of a volume whose sectors are `sector`
    /// bytes: fails with [`Error::Damaged`] if it is not one, and
    /// [`Error::NotSupported`] for Windows Vista's.
    pub fn parse(block: &'a [u8], sector: u64) -> Result<Metadata<'a>, Error> {
        let size = block_size(block).ok_or(Error::Damaged)?;
        if block.len() < size {
            return Err(Error::Damaged);
        }
        match u16_at(block, 10) {
            2 => {}
            1 => return Err(Error::NotSupported),
            _ => return Err(Error::Damaged),
        }
        let head = &block[BLOCK_HEADER..];
        if u32_at(head, 4) != 1 || u32_at(head, 8) != HEADER as u32 {
            return Err(Error::Damaged);
        }
        let mut id = [0; 16];
        id.copy_from_slice(&head[16..32]);
        let mut metadata = Metadata {
            encrypted: u64_at(block, 16),
            header: u64_at(block, 56),
            header_size: u64::from(u32_at(block, 28)) * sector,
            blocks: [u64_at(block, 32), u64_at(block, 40), u64_at(block, 48)],
            method: u16_at(head, 36),
            id,
            entries: &block[ENTRIES..size],
        };
        // Where its area descriptors say, which Windows 10 may leave out.
        let areas = metadata
            .entries()
            .find(|e| e.kind == kind::AREAS && e.value == value::AREAS && e.data.len() >= 16);
        if let Some(areas) = areas {
            metadata.header = u64_at(areas.data, 0);
            metadata.header_size = u64_at(areas.data, 8);
        }
        Ok(metadata)
    }

    /// Its entries.
    pub fn entries(&self) -> Entries<'a> {
        Entries(self.entries)
    }

    /// The volume's description, in UTF-16 without its NUL: its computer's
    /// name, its own and the date it was encrypted.
    pub fn description(&self) -> Option<impl Iterator<Item = u16> + 'a> {
        let entry =
            self.entries().find(|e| e.kind == kind::DESCRIPTION && e.value == value::STRING)?;
        Some(units(entry.data).take_while(|&u| u != 0))
    }

    /// Its volume master keys: one for each protector.
    pub fn vmks(&self) -> impl Iterator<Item = Vmk<'a>> + 'a {
        self.entries()
            .filter(|e| e.kind == kind::VMK && e.value == value::VMK && e.data.len() >= 28)
            .map(|e| Vmk { protection: u16_at(e.data, 26), properties: Entries(&e.data[28..]) })
    }
}

/// UTF-16 code units, from their bytes.
pub fn units(bytes: &[u8]) -> impl Iterator<Item = u16> + '_ {
    bytes.as_chunks::<2>().0.iter().map(|u| u16::from_le_bytes(*u))
}

/// A volume master key, as wrapped for one protector.
#[derive(Clone, Copy, Debug)]
pub struct Vmk<'a> {
    /// How it is protected ([`protection`]).
    pub protection: u16,
    /// Its properties: for a password or recovery key, the stretched
    /// key's salt and the key wrapped; with none, the key that unwraps it.
    pub properties: Entries<'a>,
}

/// An entry of the metadata, or a property of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// What it is ([`kind`]).
    pub kind: u16,
    /// What it holds ([`value`]).
    pub value: u16,
    /// Its version: 1, or 3 for some.
    pub version: u16,
    /// What it holds.
    pub data: &'a [u8],
}

/// Entries, one after another, each with its size.
#[derive(Clone, Copy, Debug)]
pub struct Entries<'a>(pub &'a [u8]);

impl<'a> Iterator for Entries<'a> {
    type Item = Entry<'a>;

    fn next(&mut self) -> Option<Entry<'a>> {
        let bytes = self.0;
        let size = if bytes.len() >= ENTRY { usize::from(u16_at(bytes, 0)) } else { 0 };
        if size < ENTRY || size > bytes.len() {
            self.0 = &[];
            return None;
        }
        self.0 = &bytes[size..];
        Some(Entry {
            kind: u16_at(bytes, 2),
            value: u16_at(bytes, 4),
            version: u16_at(bytes, 6),
            data: &bytes[ENTRY..size],
        })
    }
}

/// Where a sector of a volume unlocked is ([`Layout::place`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// One of its first sectors, whose encrypted copy is at this offset.
    Moved(u64),
    /// Part of the metadata, or of the copy of its first sectors: it reads
    /// as zeros, and is not written.
    Hidden,
    /// Encrypted where it is.
    Encrypted,
    /// Not encrypted (yet, or any more), as Windows left it.
    Plain,
}

/// Where the parts of a BitLocker volume are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The volume's bytes.
    pub size: u64,
    /// The bytes of its sectors, which are encrypted one at a time.
    pub sector: u64,
    /// The bytes encrypted, from its start.
    pub encrypted: u64,
    /// Where the encrypted copy of its first sectors is.
    pub header: u64,
    /// The bytes of the copy.
    pub header_size: u64,
    /// Where the metadata blocks are, each taking [`METADATA_AREA`] bytes.
    pub blocks: [u64; 3],
}

impl Layout {
    /// The layout `metadata` gives a volume of `size` bytes in sectors of
    /// `sector`; fails with [`Error::Damaged`] if its parts are not on the
    /// volume, or not whole sectors.
    pub fn new(metadata: &Metadata<'_>, size: u64, sector: u64) -> Result<Layout, Error> {
        let whole = |n: u64| n.is_multiple_of(sector);
        let on = |at: u64, len: u64| at.checked_add(len).is_some_and(|end| end <= size);
        let Metadata { encrypted, header, header_size, blocks, .. } = *metadata;
        let fine = whole(header)
            && whole(header_size)
            && header_size > 0
            && on(header, header_size)
            && on(0, header_size)
            && whole(encrypted)
            && blocks.iter().all(|&b| whole(b) && b < size);
        if !fine {
            return Err(Error::Damaged);
        }
        Ok(Layout { size, sector, encrypted: encrypted.min(size), header, header_size, blocks })
    }

    /// Whether every sector is encrypted, as on a volume Windows is not
    /// encrypting or decrypting: only then is it written.
    pub fn whole(&self) -> bool {
        self.encrypted == self.size
    }

    /// Where the sector at `offset` of the volume unlocked is.
    pub fn place(&self, offset: u64) -> Place {
        let within = |at: u64, len: u64| offset >= at && offset - at < len;
        if offset < self.header_size {
            Place::Moved(self.header + offset)
        } else if within(self.header, self.header_size)
            || self.blocks.iter().any(|&b| within(b, METADATA_AREA))
        {
            Place::Hidden
        } else if offset < self.encrypted {
            Place::Encrypted
        } else {
            Place::Plain
        }
    }
}
