// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Resource templates: what a device's `_CRS` method returns, a buffer of
//! resource descriptors that say which registers, ports, interrupts, GPIO
//! pins and serial bus connections the device uses (ACPI Specification
//! 6.5, section 6.4 "Resource Data Types for ACPI").
//!
//! A descriptor is small, its first byte holding its type in bits 6:3
//! and its length in bits 2:0, or large, bit 7 set with its type in bits
//! 6:0 and a 16-bit length after it; an End Tag ends the template.
//! [`Resources`] walks them. Descriptors the kernel does not use, such
//! as DMA channels, vendor data and dependent functions, are
//! [`Resource::Other`]; a descriptor shorter than its kind's fixed part,
//! or longer than the bytes there are, is an error and ends the walk.

use crate::{u16_at, u32_at, u64_at};

/// One resource of a template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource<'a> {
    /// A range of memory: registers, or memory a bridge decodes.
    Memory(Memory),
    /// A range of I/O ports.
    Io(Io),
    /// Interrupts the device raises.
    Interrupts(Interrupts<'a>),
    /// GPIO pins it uses: as an interrupt, or for input and output.
    Gpio(Gpio<'a>),
    /// A connection on an I2C bus.
    I2c(I2c<'a>),
    /// A descriptor of another kind: its small type, or a large one's
    /// type with bit 7 set.
    Other(u8),
}

/// A range of memory (32-bit Fixed Memory Range, 32-bit Memory Range,
/// or a memory Address Space descriptor of 16, 32 or 64 bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Memory {
    /// Its first address.
    pub base: u64,
    /// How many bytes it has.
    pub length: u64,
    /// Whether it may be written.
    pub writable: bool,
}

/// A range of I/O ports (I/O Port or Fixed Location I/O Port, or an I/O
/// Address Space descriptor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Io {
    /// Its first port.
    pub base: u16,
    /// How many ports it has.
    pub length: u16,
}

/// How an interrupt or a GPIO pin signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    /// Edge-triggered, rather than level-triggered.
    pub edge: bool,
    /// Which level, or which edge, is the signal's.
    pub polarity: Polarity,
    /// Whether other devices share it.
    pub shared: bool,
    /// Whether it can wake the machine.
    pub wake: bool,
}

/// An interrupt's or a GPIO pin's active level, or edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    /// High, or the rising edge.
    High,
    /// Low, or the falling edge.
    Low,
    /// Both edges (GPIO pins only).
    Both,
}

/// Interrupts a device raises (IRQ or Extended Interrupt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupts<'a> {
    /// How they signal.
    pub signal: Signal,
    /// Whether the device raises them, rather than passing them on.
    pub consumer: bool,
    numbers: Numbers<'a>,
    /// The device that has them, when not the interrupt controller.
    pub source: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Numbers<'a> {
    /// An IRQ descriptor's: a bit for each ISA interrupt.
    Mask(u16),
    /// An Extended Interrupt descriptor's: 32-bit global system interrupts.
    List(&'a [u8]),
}

impl Interrupts<'_> {
    /// The interrupts, global system interrupts unless a `source` has them.
    pub fn numbers(&self) -> impl Iterator<Item = u32> + '_ {
        let (mask, list): (u16, &[u8]) = match self.numbers {
            Numbers::Mask(mask) => (mask, &[]),
            Numbers::List(list) => (0, list),
        };
        (0..16)
            .filter(move |bit| mask & (1 << bit) != 0)
            .chain(list.as_chunks::<4>().0.iter().map(|&number| u32::from_le_bytes(number)))
    }
}

/// GPIO pins a device uses (GPIO Connection descriptor, `GpioInt` and
/// `GpioIo` in ASL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gpio<'a> {
    /// What the pins are for.
    pub usage: GpioUsage,
    /// The pull the pins need.
    pub pull: Pull,
    /// The drive strength of output pins, in hundredths of a milliampere;
    /// 0 for the controller's default.
    pub drive_strength: u16,
    /// How long a signal must hold before it counts, in hundredths of a
    /// millisecond; 0 for none.
    pub debounce: u16,
    pins: &'a [u8],
    /// The GPIO controller's path in the namespace.
    pub controller: &'a str,
    /// Bytes for the controller's driver.
    pub vendor: &'a [u8],
}

/// What a GPIO connection's pins are for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpioUsage {
    /// An interrupt, signalled as it says.
    Interrupt(Signal),
    /// Input and output: 0 either, 1 input only, 2 output only, 3 as the
    /// pin is set up already.
    Io {
        /// The restriction.
        restriction: u8,
        /// Whether other devices share the pins.
        shared: bool,
    },
}

/// The pull a GPIO pin needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pull {
    /// The controller's default.
    Default,
    /// Pulled up.
    Up,
    /// Pulled down.
    Down,
    /// Neither.
    None,
    /// A vendor's value, 0x80 to 0xff.
    Vendor(u8),
}

impl Gpio<'_> {
    /// The pins, by the controller's numbers.
    pub fn pins(&self) -> impl Iterator<Item = u16> + '_ {
        self.pins.as_chunks::<2>().0.iter().map(|&pin| u16::from_le_bytes(pin))
    }
}

/// A connection on an I2C bus (I2C Serial Bus Connection descriptor,
/// `I2cSerialBusV2` in ASL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2c<'a> {
    /// The device's address on the bus.
    pub address: u16,
    /// Whether the address has 10 bits, rather than 7.
    pub ten_bit: bool,
    /// The fastest the bus may run for it, in hertz.
    pub speed: u32,
    /// Whether the device starts transactions (it is a controller of the
    /// bus), rather than the bus's controller.
    pub device_initiated: bool,
    /// Whether other drivers share the connection.
    pub shared: bool,
    /// The I2C controller's path in the namespace.
    pub controller: &'a str,
    /// Bytes for the device's driver.
    pub vendor: &'a [u8],
}

/// A descriptor that cannot be read: its offset in the template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub usize);

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the resource descriptor at byte {} cannot be read", self.0)
    }
}

/// The descriptors of a resource template, up to its End Tag.
#[derive(Clone, Debug)]
pub struct Resources<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Resources<'a> {
    /// A template's descriptors.
    pub fn new(bytes: &'a [u8]) -> Resources<'a> {
        Resources { bytes, offset: 0 }
    }
}

/// Small descriptor types (section 6.4.2).
const IRQ: u8 = 0x04;
const IO: u8 = 0x08;
const FIXED_IO: u8 = 0x09;
const END: u8 = 0x0f;
/// Large descriptor types (section 6.4.3).
const MEMORY32: u8 = 0x05;
const FIXED_MEMORY32: u8 = 0x06;
const DWORD_SPACE: u8 = 0x07;
const WORD_SPACE: u8 = 0x08;
const EXTENDED_INTERRUPT: u8 = 0x09;
const QWORD_SPACE: u8 = 0x0a;
const GPIO: u8 = 0x0c;
const SERIAL_BUS: u8 = 0x0e;
/// The serial bus type of I2C.
const I2C: u8 = 1;

impl<'a> Iterator for Resources<'a> {
    type Item = Result<Resource<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.offset;
        let &first = self.bytes.get(start)?;
        // An End Tag, even one without its checksum byte.
        if first & 0x80 == 0 && first >> 3 == END {
            self.offset = self.bytes.len();
            return None;
        }
        let (kind, body) = if first & 0x80 == 0 {
            let length = usize::from(first & 7);
            (first >> 3, self.bytes.get(start + 1..start + 1 + length))
        } else {
            let length = u16_at(self.bytes, start + 1).map(usize::from);
            (first, length.and_then(|length| self.bytes.get(start + 3..start + 3 + length)))
        };
        let Some(body) = body else {
            self.offset = self.bytes.len();
            return Some(Err(Error(start)));
        };
        self.offset = start + 1 + body.len() + if first & 0x80 != 0 { 2 } else { 0 };
        let whole = &self.bytes[start..self.offset];
        let resource =
            if first & 0x80 == 0 { small(kind, body) } else { large(kind & 0x7f, whole) };
        if resource.is_none() {
            self.offset = self.bytes.len();
        }
        Some(resource.ok_or(Error(start)))
    }
}

/// A small descriptor of type `kind` with the bytes after its first.
fn small(kind: u8, body: &[u8]) -> Option<Resource<'_>> {
    Some(match kind {
        IRQ => {
            let mask = u16_at(body, 0)?;
            // No information byte: edge-triggered, active high, exclusive.
            let info = body.get(2).copied().unwrap_or(1);
            Resource::Interrupts(Interrupts {
                signal: Signal {
                    edge: info & 1 != 0,
                    polarity: if info & 8 != 0 { Polarity::Low } else { Polarity::High },
                    shared: info & 0x10 != 0,
                    wake: info & 0x20 != 0,
                },
                consumer: true,
                numbers: Numbers::Mask(mask),
                source: None,
            })
        }
        IO => Resource::Io(Io { base: u16_at(body, 1)?, length: body.get(6).copied()?.into() }),
        FIXED_IO => Resource::Io(Io {
            base: u16_at(body, 0)? & 0x3ff,
            length: body.get(2).copied()?.into(),
        }),
        other => Resource::Other(other),
    })
}

/// A large descriptor of type `kind`, all its bytes.
fn large(kind: u8, bytes: &[u8]) -> Option<Resource<'_>> {
    Some(match kind {
        MEMORY32 => Resource::Memory(Memory {
            base: u32_at(bytes, 4)?.into(),
            length: u32_at(bytes, 16)?.into(),
            writable: *bytes.get(3)? & 1 != 0,
        }),
        FIXED_MEMORY32 => Resource::Memory(Memory {
            base: u32_at(bytes, 4)?.into(),
            length: u32_at(bytes, 8)?.into(),
            writable: *bytes.get(3)? & 1 != 0,
        }),
        WORD_SPACE | DWORD_SPACE | QWORD_SPACE => return space(kind, bytes),
        EXTENDED_INTERRUPT => {
            let flags = *bytes.get(3)?;
            let count = usize::from(*bytes.get(4)?);
            let numbers = bytes.get(5..5 + 4 * count)?;
            Resource::Interrupts(Interrupts {
                signal: Signal {
                    edge: flags & 2 != 0,
                    polarity: if flags & 4 != 0 { Polarity::Low } else { Polarity::High },
                    shared: flags & 8 != 0,
                    wake: flags & 0x10 != 0,
                },
                consumer: flags & 1 != 0,
                numbers: Numbers::List(numbers),
                // An optional source index byte, then the source's name.
                source: bytes.get(6 + 4 * count..).and_then(name),
            })
        }
        GPIO => gpio(bytes)?,
        SERIAL_BUS if *bytes.get(5)? == I2C => i2c(bytes)?,
        other => Resource::Other(other | 0x80),
    })
}

/// A Word, DWord or QWord Address Space descriptor: memory and I/O ranges
/// as resources, bus numbers as others.
fn space(kind: u8, bytes: &[u8]) -> Option<Resource<'_>> {
    let size = match kind {
        WORD_SPACE => 2,
        DWORD_SPACE => 4,
        _ => 8,
    };
    let field = |index: usize| -> Option<u64> {
        let at = 6 + index * size;
        match size {
            2 => u16_at(bytes, at).map(u64::from),
            4 => u32_at(bytes, at).map(u64::from),
            _ => u64_at(bytes, at),
        }
    };
    // Granularity, minimum, maximum, translation offset, length.
    let (base, length) = (field(1)?, field(4)?);
    let (space, specific) = (*bytes.get(3)?, *bytes.get(5)?);
    Some(match space {
        0 => Resource::Memory(Memory { base, length, writable: specific & 1 != 0 }),
        1 => Resource::Io(Io { base: u16::try_from(base).ok()?, length: length as u16 }),
        _ => Resource::Other(kind | 0x80),
    })
}

/// A GPIO Connection descriptor, all its bytes.
fn gpio(bytes: &[u8]) -> Option<Resource<'_>> {
    let connection = *bytes.get(4)?;
    let flags = u16_at(bytes, 7)?;
    let usage = match connection {
        0 => GpioUsage::Interrupt(Signal {
            edge: flags & 1 != 0,
            polarity: match (flags >> 1) & 3 {
                0 => Polarity::High,
                1 => Polarity::Low,
                _ => Polarity::Both,
            },
            shared: flags & 8 != 0,
            wake: flags & 0x10 != 0,
        }),
        1 => GpioUsage::Io { restriction: (flags & 3) as u8, shared: flags & 8 != 0 },
        _ => return Some(Resource::Other(GPIO | 0x80)),
    };
    let pull = match *bytes.get(9)? {
        0 => Pull::Default,
        1 => Pull::Up,
        2 => Pull::Down,
        3 => Pull::None,
        vendor => Pull::Vendor(vendor),
    };
    let pins_at = usize::from(u16_at(bytes, 14)?);
    let name_at = usize::from(u16_at(bytes, 17)?);
    let vendor_at = usize::from(u16_at(bytes, 19)?);
    let vendor_length = usize::from(u16_at(bytes, 21)?);
    let pins = bytes.get(pins_at..name_at)?;
    let controller = bytes.get(name_at..vendor_at).and_then(name)?;
    Some(Resource::Gpio(Gpio {
        usage,
        pull,
        drive_strength: u16_at(bytes, 10)?,
        debounce: u16_at(bytes, 12)?,
        pins,
        controller,
        vendor: bytes.get(vendor_at..vendor_at + vendor_length)?,
    }))
}

/// An I2C Serial Bus Connection descriptor, all its bytes.
fn i2c(bytes: &[u8]) -> Option<Resource<'_>> {
    let flags = *bytes.get(6)?;
    let data = usize::from(u16_at(bytes, 10)?);
    // Speed and address, then the vendor's bytes.
    let vendor = bytes.get(18..12 + data)?;
    Some(Resource::I2c(I2c {
        address: u16_at(bytes, 16)?,
        ten_bit: u16_at(bytes, 7)? & 1 != 0,
        speed: u32_at(bytes, 12)?,
        device_initiated: flags & 1 != 0,
        shared: flags & 4 != 0,
        controller: bytes.get(12 + data..).and_then(name)?,
        vendor,
    }))
}

/// A resource source's name: the bytes up to a NUL, if they are text.
fn name(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).ok().filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Memory32Fixed` three times and an `Interrupt`, as an Intel
    /// Sunrise Point GPIO controller's `_CRS` has them; compiled by iasl.
    const INTEL_GPIO: &[u8] = &[
        0x86, 0x09, 0x00, 0x01, 0x00, 0x00, 0x6e, 0xfd, 0x00, 0x00, 0x01, 0x00, //
        0x86, 0x09, 0x00, 0x01, 0x00, 0x00, 0x6d, 0xfd, 0x00, 0x00, 0x01, 0x00, //
        0x86, 0x09, 0x00, 0x01, 0x00, 0x00, 0x6a, 0xfd, 0x00, 0x00, 0x01, 0x00, //
        0x89, 0x06, 0x00, 0x0d, 0x01, 0x0e, 0x00, 0x00, 0x00, 0x79, 0x00,
    ];

    /// A touchpad's: `I2cSerialBusV2 (0x2C, ControllerInitiated, 400000,
    /// AddressingMode7Bit, "\\_SB.PCI0.I2C1", ...)` and `GpioInt (Level,
    /// ActiveLow, ExclusiveAndWake, PullUp, 0, "\\_SB.PCI0.GPI0") {0x35}`.
    const TOUCHPAD: &[u8] = &[
        0x8e, 0x1e, 0x00, 0x02, 0x00, 0x01, 0x02, 0x00, 0x00, 0x01, 0x06, 0x00, //
        0x80, 0x1a, 0x06, 0x00, 0x2c, 0x00, 0x5c, 0x5f, 0x53, 0x42, 0x2e, 0x50, //
        0x43, 0x49, 0x30, 0x2e, 0x49, 0x32, 0x43, 0x31, 0x00, 0x8c, 0x25, 0x00, //
        0x01, 0x00, 0x01, 0x00, 0x12, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x17, //
        0x00, 0x00, 0x19, 0x00, 0x28, 0x00, 0x00, 0x00, 0x35, 0x00, 0x5c, 0x5f, //
        0x53, 0x42, 0x2e, 0x50, 0x43, 0x49, 0x30, 0x2e, 0x47, 0x50, 0x49, 0x30, //
        0x00, 0x79, 0x00,
    ];

    /// `IO`, `FixedIO`, `IRQNoFlags () {1}`, `IRQ (Edge, ActiveLow,
    /// Shared) {3, 4}`, `GpioIo (Exclusive, PullDown, 0, 0,
    /// IoRestrictionOutputOnly, "\\_SB.GPIO") {2, 5}`, a `QWordMemory` and
    /// a read-only `DWordMemory`, `Interrupt (ResourceConsumer, Edge,
    /// ActiveHigh, Exclusive) {0x33, 0x34}` and `I2cSerialBusV2 (0x350,
    /// DeviceInitiated, 100000, AddressingMode10Bit, "^I2C0", 0,
    /// ResourceConsumer, , Shared, RawDataBuffer () {1, 2})`.
    const MIXED: &[u8] = &[
        0x47, 0x01, 0x60, 0x00, 0x60, 0x00, 0x01, 0x01, 0x4b, 0x64, 0x00, 0x01, //
        0x22, 0x02, 0x00, 0x23, 0x18, 0x00, 0x19, 0x8c, 0x22, 0x00, 0x01, 0x01, //
        0x01, 0x00, 0x02, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x17, 0x00, 0x00, //
        0x1b, 0x00, 0x25, 0x00, 0x00, 0x00, 0x02, 0x00, 0x05, 0x00, 0x5c, 0x5f, //
        0x53, 0x42, 0x2e, 0x47, 0x50, 0x49, 0x4f, 0x00, 0x8a, 0x2b, 0x00, 0x00, //
        0x0c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x08, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x87, 0x17, 0x00, 0x00, 0x0c, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0xff, 0xff, 0xbf, 0xfe, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x3e, 0x89, 0x0a, 0x00, 0x03, //
        0x02, 0x33, 0x00, 0x00, 0x00, 0x34, 0x00, 0x00, 0x00, 0x8e, 0x17, 0x00, //
        0x02, 0x00, 0x01, 0x07, 0x01, 0x00, 0x01, 0x08, 0x00, 0xa0, 0x86, 0x01, //
        0x00, 0x50, 0x03, 0x01, 0x02, 0x5e, 0x49, 0x32, 0x43, 0x30, 0x00, 0x79, //
        0x00,
    ];

    fn all(bytes: &[u8]) -> Vec<Resource<'_>> {
        Resources::new(bytes).collect::<Result<_, _>>().unwrap()
    }

    fn interrupts(resource: &Resource<'_>) -> (Signal, Vec<u32>) {
        let Resource::Interrupts(interrupts) = resource else { panic!("{resource:?}") };
        (interrupts.signal, interrupts.numbers().collect())
    }

    const LEVEL_LOW_SHARED: Signal =
        Signal { edge: false, polarity: Polarity::Low, shared: true, wake: false };

    #[test]
    fn reads_a_gpio_controllers_registers_and_interrupt() {
        let resources = all(INTEL_GPIO);
        let memory: Vec<_> = resources[..3]
            .iter()
            .map(|resource| match resource {
                Resource::Memory(memory) => (memory.base, memory.length, memory.writable),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            memory,
            [
                (0xfd6e_0000, 0x1_0000, true),
                (0xfd6d_0000, 0x1_0000, true),
                (0xfd6a_0000, 0x1_0000, true)
            ]
        );
        assert_eq!(interrupts(&resources[3]), (LEVEL_LOW_SHARED, vec![14]));
        assert_eq!(resources.len(), 4);
    }

    #[test]
    fn reads_a_touchpads_i2c_connection_and_gpio_interrupt() {
        let resources = all(TOUCHPAD);
        assert_eq!(
            resources[0],
            Resource::I2c(I2c {
                address: 0x2c,
                ten_bit: false,
                speed: 400_000,
                device_initiated: false,
                shared: false,
                controller: "\\_SB.PCI0.I2C1",
                vendor: &[],
            })
        );
        let Resource::Gpio(gpio) = resources[1] else { panic!("{:?}", resources[1]) };
        assert_eq!(
            gpio.usage,
            GpioUsage::Interrupt(Signal {
                edge: false,
                polarity: Polarity::Low,
                shared: false,
                wake: true
            })
        );
        assert_eq!((gpio.pull, gpio.drive_strength, gpio.debounce), (Pull::Up, 0, 0));
        assert_eq!(gpio.pins().collect::<Vec<_>>(), [0x35]);
        assert_eq!((gpio.controller, gpio.vendor), ("\\_SB.PCI0.GPI0", &[][..]));
        assert_eq!(resources.len(), 2);
    }

    #[test]
    fn reads_ports_irqs_address_spaces_and_their_flags() {
        let resources = all(MIXED);
        assert_eq!(resources[0], Resource::Io(Io { base: 0x60, length: 1 }));
        assert_eq!(resources[1], Resource::Io(Io { base: 0x64, length: 1 }));
        let edge_high = Signal { edge: true, polarity: Polarity::High, shared: false, wake: false };
        assert_eq!(interrupts(&resources[2]), (edge_high, vec![1]));
        let edge_low_shared = Signal { edge: true, polarity: Polarity::Low, ..LEVEL_LOW_SHARED };
        assert_eq!(interrupts(&resources[3]), (edge_low_shared, vec![3, 4]));
        let Resource::Gpio(gpio) = resources[4] else { panic!("{:?}", resources[4]) };
        assert_eq!(gpio.usage, GpioUsage::Io { restriction: 2, shared: false });
        assert_eq!((gpio.pull, gpio.controller), (Pull::Down, "\\_SB.GPIO"));
        assert_eq!(gpio.pins().collect::<Vec<_>>(), [2, 5]);
        assert_eq!(
            resources[5],
            Resource::Memory(Memory { base: 0x8_0000_0000, length: 0x1_0000_0000, writable: true })
        );
        assert_eq!(
            resources[6],
            Resource::Memory(Memory { base: 0xc000_0000, length: 0x3ec0_0000, writable: false })
        );
        assert_eq!(interrupts(&resources[7]), (edge_high, vec![0x33, 0x34]));
        assert_eq!(
            resources[8],
            Resource::I2c(I2c {
                address: 0x350,
                ten_bit: true,
                speed: 100_000,
                device_initiated: true,
                shared: true,
                controller: "^I2C0",
                vendor: &[1, 2],
            })
        );
        assert_eq!(resources.len(), 9);
    }

    #[test]
    fn a_descriptor_cut_short_ends_the_walk_with_its_offset() {
        // The touchpad's GPIO descriptor, at byte 33, loses its last bytes.
        let cut = &TOUCHPAD[..60];
        let walked: Vec<_> = Resources::new(cut).collect();
        assert!(matches!(walked[0], Ok(Resource::I2c(_))));
        assert_eq!(walked[1], Err(Error(33)));
        assert_eq!(walked.len(), 2);
        // A GPIO descriptor whose pin table runs past its end.
        let mut bad = TOUCHPAD[33..].to_vec();
        bad[14] = 0x30;
        let walked: Vec<_> = Resources::new(&bad).collect();
        assert_eq!(walked, [Err(Error(0))]);
        // A fixed memory range two bytes short.
        let walked: Vec<_> =
            Resources::new(&[0x86, 0x07, 0, 1, 0, 0, 0, 0, 0, 0, 0x79, 0]).collect();
        assert_eq!(walked, [Err(Error(0))]);
    }

    #[test]
    fn other_descriptors_are_named_and_skipped() {
        // DMA (small type 5), a vendor-defined large one (type 4), then an IRQ.
        let bytes = [0x2a, 0x04, 0x00, 0x84, 0x02, 0x00, 0xaa, 0xbb, 0x22, 0x01, 0x00, 0x79, 0x00];
        let resources = all(&bytes);
        assert_eq!(resources[..2], [Resource::Other(0x05), Resource::Other(0x84)]);
        assert_eq!(interrupts(&resources[2]).1, [0]);
        // Without an End Tag, the walk ends with the bytes, and one without
        // its checksum ends it too.
        assert_eq!(all(&bytes[..11]).len(), 3);
        assert_eq!(all(&bytes[..12]).len(), 3);
    }
}
