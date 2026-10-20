//! What the architecture hands the kernel at boot.

use core::fmt;

/// Everything the kernel learns from the boot loader, in a form that does
/// not depend on the boot protocol.
pub struct BootParams {
    /// Kernel command line; see orthros-boot(7) and [`crate::cmdline`].
    pub command_line: &'static str,
    /// Name the boot loader reported, such as `GRUB 2.12`.
    pub boot_loader: &'static str,
    /// The firmware the machine booted through.
    pub firmware: Firmware,
    /// Physical memory as the firmware reported it, sorted by address.
    pub memory_map: &'static [MemoryRegion],
    /// Ranges of RAM the kernel itself occupies or has set aside so far.
    pub reservations: &'static [Reservation],
    /// The linear framebuffer, mapped and ready to draw into, if the boot
    /// loader set up a graphics mode.
    pub framebuffer: Option<Framebuffer>,
    /// Whether the architecture already shows the console on a text-mode
    /// screen (the VGA text buffer on a PC booted through BIOS).
    pub text_screen: bool,
}

/// The firmware a machine boots through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Firmware {
    /// UEFI.
    Uefi,
    /// Legacy PC BIOS.
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

/// What a range of physical memory holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    /// RAM free for the kernel to use.
    Usable,
    /// Reserved by the firmware or hardware.
    Reserved,
    /// RAM holding ACPI tables, usable once they have been read.
    AcpiReclaimable,
    /// RAM the firmware needs preserved, including across sleep.
    AcpiNvs,
    /// RAM the firmware found to be faulty.
    Defective,
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MemoryKind::Usable => "usable",
            MemoryKind::Reserved => "reserved",
            MemoryKind::AcpiReclaimable => "ACPI tables",
            MemoryKind::AcpiNvs => "ACPI NVS",
            MemoryKind::Defective => "defective",
        })
    }
}

/// One range of the physical memory map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    /// First byte.
    pub start: u64,
    /// One past the last byte.
    pub end: u64,
    /// What the range holds.
    pub kind: MemoryKind,
}

impl MemoryRegion {
    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the region is empty.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// A range of physical memory the kernel has claimed, with a name for the
/// boot report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    /// What the range is used for.
    pub name: &'static str,
    /// First byte.
    pub start: u64,
    /// One past the last byte.
    pub end: u64,
}

/// Bit layout of one pixel channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channel {
    /// Position of the lowest bit.
    pub shift: u8,
    /// Number of bits.
    pub bits: u8,
}

/// How a pixel's colour is laid out in memory (little-endian).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    /// Red channel.
    pub red: Channel,
    /// Green channel.
    pub green: Channel,
    /// Blue channel.
    pub blue: Channel,
    /// Bytes per pixel: 2, 3 or 4.
    pub bytes_per_pixel: u8,
}

impl PixelFormat {
    /// The usual 32-bit format: blue in the lowest byte, then green, red.
    pub const XRGB8888: PixelFormat = PixelFormat {
        red: Channel { shift: 16, bits: 8 },
        green: Channel { shift: 8, bits: 8 },
        blue: Channel { shift: 0, bits: 8 },
        bytes_per_pixel: 4,
    };

    /// Encodes an 8-bit-per-channel colour as a pixel value.
    pub fn encode(&self, r: u8, g: u8, b: u8) -> u32 {
        fn channel(value: u8, channel: Channel) -> u32 {
            if channel.bits == 0 || channel.shift >= 32 {
                return 0;
            }
            let bits = channel.bits.min(8);
            (u32::from(value) >> (8 - bits)) << channel.shift
        }
        channel(r, self.red) | channel(g, self.green) | channel(b, self.blue)
    }
}

/// A linear framebuffer.
pub struct Framebuffer {
    /// The pixels, `pitch * height` bytes, already mapped for writing.
    pub memory: &'static mut [u8],
    /// Physical address, for the boot report.
    pub phys_addr: u64,
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Bytes from the start of one row to the next.
    pub pitch: usize,
    /// Pixel layout.
    pub format: PixelFormat,
}

impl fmt::Debug for Framebuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Framebuffer")
            .field("phys_addr", &self.phys_addr)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pitch", &self.pitch)
            .field("format", &self.format)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_pixels() {
        assert_eq!(PixelFormat::XRGB8888.encode(0x12, 0x34, 0x56), 0x0012_3456);
        let rgb565 = PixelFormat {
            red: Channel { shift: 11, bits: 5 },
            green: Channel { shift: 5, bits: 6 },
            blue: Channel { shift: 0, bits: 5 },
            bytes_per_pixel: 2,
        };
        assert_eq!(rgb565.encode(0xff, 0xff, 0xff), 0xffff);
        assert_eq!(rgb565.encode(0xff, 0, 0), 0xf800);
    }

    #[test]
    fn region_lengths() {
        let region = MemoryRegion { start: 0x1000, end: 0x3000, kind: MemoryKind::Usable };
        assert_eq!(region.len(), 0x2000);
        assert!(!region.is_empty());
        assert_eq!(MemoryKind::AcpiReclaimable.to_string(), "ACPI tables");
    }
}
