//! Safe parser for the Multiboot2 boot information structure.
//!
//! The kernel turns the address it receives from the boot loader into a byte
//! slice once; everything after that happens here, in safe code that also
//! compiles and is unit-tested on the build host.
//!
//! Reference: the Multiboot2 specification, section 3.6 "Boot information".

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

/// Value a Multiboot2 boot loader leaves in EAX when it enters the kernel.
pub const MULTIBOOT2_MAGIC: u32 = 0x36d7_6289;

/// Tag type numbers this crate understands.
pub mod tag_type {
    /// Marks the end of the tag list.
    pub const END: u32 = 0;
    /// Kernel command line (NUL-terminated UTF-8).
    pub const COMMAND_LINE: u32 = 1;
    /// Boot loader name (NUL-terminated UTF-8).
    pub const BOOT_LOADER_NAME: u32 = 2;
    /// A module the boot loader loaded next to the kernel.
    pub const MODULE: u32 = 3;
    /// Memory map provided by the firmware through the boot loader.
    pub const MEMORY_MAP: u32 = 6;
    /// The framebuffer the boot loader set up.
    pub const FRAMEBUFFER: u32 = 8;
    /// EFI 32-bit system table pointer.
    pub const EFI32_SYSTEM_TABLE: u32 = 11;
    /// EFI 64-bit system table pointer.
    pub const EFI64_SYSTEM_TABLE: u32 = 12;
    /// Copy of an ACPI 1.0 RSDP.
    pub const ACPI_OLD_RSDP: u32 = 14;
    /// Copy of an ACPI 2.0 or later RSDP.
    pub const ACPI_NEW_RSDP: u32 = 15;
    /// EFI memory map.
    pub const EFI_MEMORY_MAP: u32 = 17;
}

/// Memory map entry type for RAM the kernel may use.
pub const MEMORY_AVAILABLE: u32 = 1;

/// Why the boot information could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Fewer than 8 bytes, so not even the fixed header is present.
    TooShort,
    /// The `total_size` field does not match the slice it was read from.
    SizeMismatch {
        /// Size recorded in the header.
        recorded: usize,
        /// Length of the slice handed to [`BootInfo::parse`].
        actual: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort => write!(f, "boot information shorter than its 8-byte header"),
            Error::SizeMismatch { recorded, actual } => {
                write!(f, "boot information records {recorded} bytes but {actual} were provided")
            }
        }
    }
}

/// The firmware the machine booted through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Firmware {
    /// UEFI; the boot loader passed EFI tags.
    Uefi,
    /// Legacy BIOS; no EFI tags were passed.
    Bios,
}

impl fmt::Display for Firmware {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Firmware::Uefi => "UEFI",
            Firmware::Bios => "BIOS",
        })
    }
}

/// A validated view of the boot information structure.
#[derive(Clone, Copy, Debug)]
pub struct BootInfo<'a> {
    bytes: &'a [u8],
}

impl<'a> BootInfo<'a> {
    /// Checks the fixed header and wraps the structure.
    ///
    /// `bytes` must be exactly `total_size` bytes long, starting at the
    /// structure's first byte.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() < 8 {
            return Err(Error::TooShort);
        }
        let recorded = read_u32(bytes, 0).ok_or(Error::TooShort)? as usize;
        if recorded != bytes.len() {
            return Err(Error::SizeMismatch { recorded, actual: bytes.len() });
        }
        Ok(Self { bytes })
    }

    /// Iterates over the tags, stopping at the end tag or at the first
    /// malformed tag.
    pub fn tags(&self) -> Tags<'a> {
        Tags { bytes: self.bytes, offset: 8 }
    }

    /// Returns the first tag of type `typ`.
    pub fn tag(&self, typ: u32) -> Option<Tag<'a>> {
        self.tags().find(|tag| tag.typ == typ)
    }

    /// The kernel command line, if the boot loader passed one.
    pub fn command_line(&self) -> Option<&'a str> {
        self.tag(tag_type::COMMAND_LINE).and_then(|tag| c_str(tag.data))
    }

    /// The boot loader's name, for example `GRUB 2.12`.
    pub fn boot_loader_name(&self) -> Option<&'a str> {
        self.tag(tag_type::BOOT_LOADER_NAME).and_then(|tag| c_str(tag.data))
    }

    /// UEFI if any EFI tag is present, otherwise BIOS.
    pub fn firmware(&self) -> Firmware {
        let efi = self.tags().any(|tag| {
            matches!(
                tag.typ,
                tag_type::EFI32_SYSTEM_TABLE
                    | tag_type::EFI64_SYSTEM_TABLE
                    | tag_type::EFI_MEMORY_MAP
            )
        });
        if efi { Firmware::Uefi } else { Firmware::Bios }
    }

    /// The memory map, if present and well formed.
    pub fn memory_map(&self) -> Option<MemoryMap<'a>> {
        let tag = self.tag(tag_type::MEMORY_MAP)?;
        let entry_size = read_u32(tag.data, 0)? as usize;
        if entry_size < MemoryRegion::ENCODED_SIZE {
            return None;
        }
        Some(MemoryMap { entries: tag.data.get(8..)?, entry_size })
    }

    /// The framebuffer, if the boot loader set one up.
    pub fn framebuffer(&self) -> Option<FramebufferInfo> {
        let data = self.tag(tag_type::FRAMEBUFFER)?.data;
        let kind = *data.get(21)?;
        let rgb = if kind == framebuffer_type::RGB {
            let fields = data.get(24..30)?;
            Some([(fields[0], fields[1]), (fields[2], fields[3]), (fields[4], fields[5])])
        } else {
            None
        };
        Some(FramebufferInfo {
            address: read_u64(data, 0)?,
            pitch: read_u32(data, 8)?,
            width: read_u32(data, 12)?,
            height: read_u32(data, 16)?,
            bpp: *data.get(20)?,
            kind,
            rgb,
        })
    }

    /// The copy of the ACPI RSDP the boot loader passed, preferring the
    /// ACPI 2.0 one.
    pub fn acpi_rsdp(&self) -> Option<&'a [u8]> {
        self.tag(tag_type::ACPI_NEW_RSDP)
            .or_else(|| self.tag(tag_type::ACPI_OLD_RSDP))
            .map(|t| t.data)
    }

    /// The modules the boot loader loaded.
    pub fn modules(&self) -> impl Iterator<Item = Module<'a>> + 'a {
        self.tags().filter(|tag| tag.typ == tag_type::MODULE).filter_map(|tag| {
            Some(Module {
                start: read_u32(tag.data, 0)?,
                end: read_u32(tag.data, 4)?,
                command_line: c_str(tag.data.get(8..)?)?,
            })
        })
    }
}

/// Framebuffer types in the framebuffer tag.
pub mod framebuffer_type {
    /// Indexed colour with a palette.
    pub const INDEXED: u8 = 0;
    /// Direct RGB colour.
    pub const RGB: u8 = 1;
    /// EGA-style text mode: the address is the VGA text buffer.
    pub const EGA_TEXT: u8 = 2;
}

/// The framebuffer tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramebufferInfo {
    /// Physical address of the first pixel (or character cell).
    pub address: u64,
    /// Bytes per row.
    pub pitch: u32,
    /// Width in pixels (or characters).
    pub width: u32,
    /// Height in pixels (or characters).
    pub height: u32,
    /// Bits per pixel.
    pub bpp: u8,
    /// One of [`framebuffer_type`].
    pub kind: u8,
    /// For RGB framebuffers, (position, size) in bits of red, green and
    /// blue.
    pub rgb: Option<[(u8, u8); 3]>,
}

/// A module loaded by the boot loader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Module<'a> {
    /// Physical address of the first byte.
    pub start: u32,
    /// Physical address one past the last byte.
    pub end: u32,
    /// The rest of the module's GRUB line, such as `microcode`.
    pub command_line: &'a str,
}

/// One tag: its type number and the bytes after its 8-byte header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag<'a> {
    /// Tag type number (see [`tag_type`]).
    pub typ: u32,
    /// Tag payload, excluding the type and size fields.
    pub data: &'a [u8],
}

/// Iterator over the tags of a [`BootInfo`].
#[derive(Clone, Debug)]
pub struct Tags<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for Tags<'a> {
    type Item = Tag<'a>;

    fn next(&mut self) -> Option<Tag<'a>> {
        let typ = read_u32(self.bytes, self.offset)?;
        let size = read_u32(self.bytes, self.offset + 4)? as usize;
        if typ == tag_type::END || size < 8 {
            return None;
        }
        let data = self.bytes.get(self.offset + 8..self.offset.checked_add(size)?)?;
        // Tags start on 8-byte boundaries.
        self.offset = self.offset.checked_add(size)?.checked_next_multiple_of(8)?;
        Some(Tag { typ, data })
    }
}

/// The Multiboot2 memory map.
#[derive(Clone, Copy, Debug)]
pub struct MemoryMap<'a> {
    entries: &'a [u8],
    entry_size: usize,
}

impl<'a> MemoryMap<'a> {
    /// Iterates over the regions in the order the firmware reported them.
    pub fn regions(&self) -> impl Iterator<Item = MemoryRegion> + 'a {
        self.entries.chunks_exact(self.entry_size).filter_map(|entry| {
            Some(MemoryRegion {
                base: read_u64(entry, 0)?,
                length: read_u64(entry, 8)?,
                kind: read_u32(entry, 16)?,
            })
        })
    }

    /// Total bytes and number of regions of usable RAM.
    pub fn usable_summary(&self) -> (u64, usize) {
        self.regions()
            .filter(|region| region.kind == MEMORY_AVAILABLE)
            .fold((0, 0), |(bytes, count), region| (bytes.saturating_add(region.length), count + 1))
    }
}

/// One region of the memory map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    /// Physical start address.
    pub base: u64,
    /// Length in bytes.
    pub length: u64,
    /// Region type; [`MEMORY_AVAILABLE`] is usable RAM.
    pub kind: u32,
}

impl MemoryRegion {
    /// Bytes one entry occupies in the Multiboot2 encoding.
    pub const ENCODED_SIZE: usize = 24;
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(raw.try_into().ok()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let raw = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(raw.try_into().ok()?))
}

/// Reads a NUL-terminated UTF-8 string from the start of `bytes`.
fn c_str(bytes: &[u8]) -> Option<&str> {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..len]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a boot information structure from (type, payload) pairs.
    fn build(tags: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![0u8; 8];
        for (typ, payload) in tags {
            out.extend_from_slice(&typ.to_le_bytes());
            out.extend_from_slice(&((payload.len() + 8) as u32).to_le_bytes());
            out.extend_from_slice(payload);
            while !out.len().is_multiple_of(8) {
                out.push(0);
            }
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        let total = out.len() as u32;
        out[0..4].copy_from_slice(&total.to_le_bytes());
        out
    }

    fn c_string(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    fn memory_map(regions: &[(u64, u64, u32)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&24u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        for &(base, length, kind) in regions {
            v.extend_from_slice(&base.to_le_bytes());
            v.extend_from_slice(&length.to_le_bytes());
            v.extend_from_slice(&kind.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes());
        }
        v
    }

    #[test]
    fn reads_strings_and_firmware() {
        let bytes = build(&[
            (tag_type::COMMAND_LINE, c_string("qemu-exit verbose")),
            (tag_type::BOOT_LOADER_NAME, c_string("GRUB 2.12")),
            (tag_type::EFI64_SYSTEM_TABLE, vec![0; 8]),
        ]);
        let info = BootInfo::parse(&bytes).unwrap();
        assert_eq!(info.command_line(), Some("qemu-exit verbose"));
        assert_eq!(info.boot_loader_name(), Some("GRUB 2.12"));
        assert_eq!(info.firmware(), Firmware::Uefi);
    }

    #[test]
    fn no_efi_tags_means_bios() {
        let bytes = build(&[(tag_type::COMMAND_LINE, c_string(""))]);
        let info = BootInfo::parse(&bytes).unwrap();
        assert_eq!(info.firmware(), Firmware::Bios);
        assert_eq!(info.command_line(), Some(""));
        assert_eq!(info.boot_loader_name(), None);
    }

    #[test]
    fn sums_usable_memory() {
        let bytes = build(&[(
            tag_type::MEMORY_MAP,
            memory_map(&[(0, 0x9f000, 1), (0x9f000, 0x1000, 2), (0x100000, 0x7ef0000, 1)]),
        )]);
        let info = BootInfo::parse(&bytes).unwrap();
        let map = info.memory_map().unwrap();
        assert_eq!(map.regions().count(), 3);
        assert_eq!(map.usable_summary(), (0x9f000 + 0x7ef0000, 2));
    }

    #[test]
    fn reads_framebuffer_rsdp_and_modules() {
        let mut fb = Vec::new();
        fb.extend_from_slice(&0xfd00_0000u64.to_le_bytes());
        for value in [5120u32, 1280, 800] {
            fb.extend_from_slice(&value.to_le_bytes());
        }
        fb.extend_from_slice(&[32, framebuffer_type::RGB, 0, 0, 16, 8, 8, 8, 0, 8]);
        let mut module = Vec::new();
        module.extend_from_slice(&0x0010_0000u32.to_le_bytes());
        module.extend_from_slice(&0x0010_2000u32.to_le_bytes());
        module.extend_from_slice(&c_string("microcode"));
        let bytes = build(&[
            (tag_type::FRAMEBUFFER, fb),
            (tag_type::ACPI_OLD_RSDP, b"RSD PTR v1".to_vec()),
            (tag_type::ACPI_NEW_RSDP, b"RSD PTR v2".to_vec()),
            (tag_type::MODULE, module),
        ]);
        let info = BootInfo::parse(&bytes).unwrap();
        let fb = info.framebuffer().unwrap();
        assert_eq!(
            (fb.address, fb.pitch, fb.width, fb.height, fb.bpp),
            (0xfd00_0000, 5120, 1280, 800, 32)
        );
        assert_eq!(fb.rgb, Some([(16, 8), (8, 8), (0, 8)]));
        assert_eq!(info.acpi_rsdp(), Some(&b"RSD PTR v2"[..]));
        let modules: Vec<_> = info.modules().collect();
        assert_eq!(
            modules,
            [Module { start: 0x0010_0000, end: 0x0010_2000, command_line: "microcode" }]
        );
    }

    #[test]
    fn rejects_bad_headers() {
        assert_eq!(BootInfo::parse(&[0; 4]).unwrap_err(), Error::TooShort);
        let mut bytes = build(&[]);
        bytes.push(0);
        assert_eq!(
            BootInfo::parse(&bytes).unwrap_err(),
            Error::SizeMismatch { recorded: 16, actual: 17 }
        );
    }

    #[test]
    fn stops_at_truncated_tag() {
        let mut bytes = build(&[(tag_type::COMMAND_LINE, c_string("x"))]);
        // Claim the first tag is far larger than the structure.
        bytes[12..16].copy_from_slice(&4096u32.to_le_bytes());
        let info = BootInfo::parse(&bytes).unwrap();
        assert_eq!(info.tags().count(), 0);
    }

    #[test]
    fn ignores_undersized_memory_map_entries() {
        let mut payload = memory_map(&[(0, 4096, 1)]);
        payload[0..4].copy_from_slice(&16u32.to_le_bytes());
        let bytes = build(&[(tag_type::MEMORY_MAP, payload)]);
        let info = BootInfo::parse(&bytes).unwrap();
        assert!(info.memory_map().is_none());
    }
}
