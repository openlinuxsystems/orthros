// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! PCI configuration space, through the PCI Express enhanced configuration
//! access mechanism (ECAM).
//!
//! The ACPI MCFG table gives, for each PCI segment, the physical address
//! of a window in which every function's 4 KiB of configuration space sits
//! at `bus << 20 | device << 15 | function << 12`. [`scan`] walks the
//! buses a segment decodes from its first one, following PCI-to-PCI
//! bridges to the buses behind them, and reports every function present
//! with its base address registers (BARs) already sized. A function's
//! capability list leads to its MSI and MSI-X capabilities, which
//! [`Msi`] and [`MsiX`] program; drivers use only message-signalled
//! interrupts, which need no firmware routing tables (ADR 0052).
//!
//! Everything here is safe code over [`Mmio`] windows: a function's
//! registers are only reached at fixed offsets, or at offsets read from
//! the device and checked first. The PCI Local Bus Specification 3.0 and
//! the PCI Express Base Specification describe the registers.

use core::fmt;

use crate::{Mmio, MsiMessage};

/// The size of one function's configuration space.
pub const CONFIG_SIZE: usize = 4096;
/// The bytes of ECAM window one bus takes.
pub const BUS_SIZE: usize = 32 * 8 * CONFIG_SIZE;

const VENDOR_ID: usize = 0x00;
const DEVICE_ID: usize = 0x02;
const COMMAND: usize = 0x04;
const STATUS: usize = 0x06;
const REVISION: usize = 0x08;
const PROG_IF: usize = 0x09;
const SUBCLASS: usize = 0x0a;
const CLASS: usize = 0x0b;
const HEADER_TYPE: usize = 0x0e;
const BAR0: usize = 0x10;
const SECONDARY_BUS: usize = 0x19;
const SUBSYSTEM_VENDOR_ID: usize = 0x2c;
const SUBSYSTEM_ID: usize = 0x2e;
const CAPABILITIES: usize = 0x34;

/// The status register's bit that says a capability list exists.
const STATUS_CAPABILITIES: u16 = 1 << 4;
/// The most capabilities followed: a malformed list cannot loop.
const MAX_CAPABILITIES: usize = 48;

/// Capability IDs.
pub mod capability {
    /// Message Signalled Interrupts.
    pub const MSI: u8 = 0x05;
    /// Vendor-specific (virtio's configuration structures).
    pub const VENDOR: u8 = 0x09;
    /// PCI Express.
    pub const EXPRESS: u8 = 0x10;
    /// MSI-X.
    pub const MSIX: u8 = 0x11;
}

/// Command register bits.
pub mod command {
    /// The function answers I/O port accesses to its BARs.
    pub const IO: u16 = 1 << 0;
    /// The function answers memory accesses to its BARs.
    pub const MEMORY: u16 = 1 << 1;
    /// The function may start DMA.
    pub const BUS_MASTER: u16 = 1 << 2;
    /// The function's legacy INTx interrupt is masked.
    pub const INTX_DISABLE: u16 = 1 << 10;
}

/// Where a function sits: segment, bus, device and function number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    /// The PCI segment group.
    pub segment: u16,
    /// The bus.
    pub bus: u8,
    /// The device on the bus (0 to 31).
    pub device: u8,
    /// The function of the device (0 to 7).
    pub function: u8,
}

impl Address {
    /// Where its configuration space is in its bus's ECAM window.
    pub fn offset_in_bus(&self) -> usize {
        (usize::from(self.device) << 15) | (usize::from(self.function) << 12)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:02x}:{:02x}.{}", self.segment, self.bus, self.device, self.function)
    }
}

/// A base address register: where a range of the function's registers is
/// in memory or I/O space, and its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    /// Memory-mapped registers.
    Memory {
        /// Physical address of the range.
        address: u64,
        /// Its size in bytes, a power of two.
        size: u64,
        /// Whether reads have no side effects, so caching may combine them.
        prefetchable: bool,
    },
    /// I/O ports.
    Io {
        /// The first port.
        port: u32,
        /// How many ports.
        size: u32,
    },
}

/// A PCI function, found by [`scan`].
#[derive(Clone, Debug)]
pub struct Function {
    address: Address,
    config: Mmio,
    bars: [Option<Bar>; 6],
}

impl Function {
    /// The function whose configuration space is `config`, if one is
    /// there (a missing function reads all ones). Its BARs are sized,
    /// which briefly turns off its decoding: call it before any driver
    /// uses the function.
    pub fn probe(address: Address, config: Mmio) -> Option<Function> {
        if config.len() < CONFIG_SIZE || config.read16(VENDOR_ID) == 0xffff {
            return None;
        }
        let mut function = Function { address, config, bars: [None; 6] };
        function.bars = function.size_bars();
        Some(function)
    }

    /// Where it sits.
    pub fn address(&self) -> Address {
        self.address
    }

    /// Its vendor ID.
    pub fn vendor_id(&self) -> u16 {
        self.config.read16(VENDOR_ID)
    }

    /// Its device ID.
    pub fn device_id(&self) -> u16 {
        self.config.read16(DEVICE_ID)
    }

    /// Its subsystem vendor ID (header type 0 only, else 0).
    pub fn subsystem_vendor_id(&self) -> u16 {
        if self.header_type() == 0 { self.config.read16(SUBSYSTEM_VENDOR_ID) } else { 0 }
    }

    /// Its subsystem ID (header type 0 only, else 0).
    pub fn subsystem_id(&self) -> u16 {
        if self.header_type() == 0 { self.config.read16(SUBSYSTEM_ID) } else { 0 }
    }

    /// Its class code: base class, subclass and programming interface.
    pub fn class(&self) -> (u8, u8, u8) {
        (self.config.read8(CLASS), self.config.read8(SUBCLASS), self.config.read8(PROG_IF))
    }

    /// Its revision.
    pub fn revision(&self) -> u8 {
        self.config.read8(REVISION)
    }

    /// Its header type without the multi-function bit: 0 for an endpoint,
    /// 1 for a PCI-to-PCI bridge.
    pub fn header_type(&self) -> u8 {
        self.config.read8(HEADER_TYPE) & 0x7f
    }

    /// Whether the device has functions other than 0.
    pub fn multifunction(&self) -> bool {
        self.config.read8(HEADER_TYPE) & 0x80 != 0
    }

    /// The bus behind a bridge.
    pub fn secondary_bus(&self) -> Option<u8> {
        (self.header_type() == 1).then(|| self.config.read8(SECONDARY_BUS))
    }

    /// The command register.
    pub fn command(&self) -> u16 {
        self.config.read16(COMMAND)
    }

    /// Sets the command register.
    pub fn set_command(&self, value: u16) {
        self.config.write16(COMMAND, value);
    }

    /// Lets the function answer accesses to its memory BARs and start DMA,
    /// with its legacy interrupt masked.
    pub fn enable_memory_and_dma(&self) {
        let value = self.command() | command::MEMORY | command::BUS_MASTER | command::INTX_DISABLE;
        self.set_command(value);
    }

    /// Reads a byte of its configuration space.
    pub fn read8(&self, offset: usize) -> u8 {
        self.config.read8(offset)
    }

    /// Reads 16 bits of its configuration space.
    pub fn read16(&self, offset: usize) -> u16 {
        self.config.read16(offset)
    }

    /// Reads 32 bits of its configuration space.
    pub fn read32(&self, offset: usize) -> u32 {
        self.config.read32(offset)
    }

    /// Writes a byte of its configuration space.
    pub fn write8(&self, offset: usize, value: u8) {
        self.config.write8(offset, value);
    }

    /// Writes 16 bits of its configuration space.
    pub fn write16(&self, offset: usize, value: u16) {
        self.config.write16(offset, value);
    }

    /// Writes 32 bits of its configuration space.
    pub fn write32(&self, offset: usize, value: u32) {
        self.config.write32(offset, value);
    }

    /// BAR `index` (0 to 5), as sized when the function was found.
    pub fn bar(&self, index: usize) -> Option<Bar> {
        self.bars.get(index).copied().flatten()
    }

    /// Sizes the BARs: writes all ones to each with decoding off, reads
    /// back which bits stuck, and puts the old value back.
    fn size_bars(&self) -> [Option<Bar>; 6] {
        let mut bars = [None; 6];
        let count = match self.header_type() {
            0 => 6,
            1 => 2,
            _ => return bars,
        };
        let saved = self.command();
        self.set_command(saved & !(command::IO | command::MEMORY));
        let mut index = 0;
        while index < count {
            let offset = BAR0 + 4 * index;
            let low = self.config.read32(offset);
            if low & 1 == 1 {
                self.config.write32(offset, u32::MAX);
                let mask = self.config.read32(offset) & !0x3;
                self.config.write32(offset, low);
                let size = (!mask).wrapping_add(1) & 0xffff;
                if mask != 0 && size != 0 {
                    bars[index] = Some(Bar::Io { port: low & !0x3, size });
                }
                index += 1;
                continue;
            }
            let wide = (low >> 1) & 0x3 == 0x2 && index + 1 < count;
            let prefetchable = low & 0x8 != 0;
            self.config.write32(offset, u32::MAX);
            let mask_low = self.config.read32(offset) & !0xf;
            self.config.write32(offset, low);
            let (address, mask) = if wide {
                let high = self.config.read32(offset + 4);
                self.config.write32(offset + 4, u32::MAX);
                let mask_high = self.config.read32(offset + 4);
                self.config.write32(offset + 4, high);
                (
                    (u64::from(high) << 32) | u64::from(low & !0xf),
                    (u64::from(mask_high) << 32) | u64::from(mask_low),
                )
            } else {
                (u64::from(low & !0xf), u64::from(mask_low) | 0xffff_ffff_0000_0000)
            };
            let size = (!mask).wrapping_add(1);
            if mask_low != 0 && size != 0 && size.is_power_of_two() {
                bars[index] = Some(Bar::Memory { address, size, prefetchable });
            }
            index += if wide { 2 } else { 1 };
        }
        self.set_command(saved);
        bars
    }

    /// Its capabilities: (ID, offset in configuration space) pairs, in
    /// list order. A list that loops or leaves the standard header's 256
    /// bytes ends early.
    pub fn capabilities(&self) -> Capabilities<'_> {
        let first = if self.config.read16(STATUS) & STATUS_CAPABILITIES != 0 {
            self.config.read8(CAPABILITIES) & !0x3
        } else {
            0
        };
        Capabilities { function: self, next: first, seen: 0 }
    }

    /// The offset of its first capability with ID `id`.
    pub fn find_capability(&self, id: u8) -> Option<usize> {
        self.capabilities().find(|&(found, _)| found == id).map(|(_, offset)| offset)
    }
}

/// The iterator [`Function::capabilities`] returns.
pub struct Capabilities<'a> {
    function: &'a Function,
    next: u8,
    seen: usize,
}

impl Iterator for Capabilities<'_> {
    type Item = (u8, usize);

    fn next(&mut self) -> Option<(u8, usize)> {
        if self.next < 0x40 || self.seen >= MAX_CAPABILITIES {
            return None;
        }
        let offset = usize::from(self.next);
        self.seen += 1;
        let id = self.function.config.read8(offset);
        self.next = self.function.config.read8(offset + 1) & !0x3;
        Some((id, offset))
    }
}

/// Walks the buses of one PCI segment from `first_bus`, following bridges
/// to the buses behind them while they stay within `last_bus`, and calls
/// `found` for every function present. `map_bus` gives the ECAM window of
/// a bus ([`BUS_SIZE`] bytes), or `None` if it cannot be mapped. Each bus
/// is visited once, however the bridges point.
pub fn scan(
    segment: u16,
    first_bus: u8,
    last_bus: u8,
    map_bus: &mut dyn FnMut(u8) -> Option<Mmio>,
    found: &mut dyn FnMut(Function),
) {
    let mut visited = [0u64; 4];
    let mut pending = [0u8; 256];
    let mut count = 0;
    pending[count] = first_bus;
    count += 1;
    while count > 0 {
        count -= 1;
        let bus = pending[count];
        let bit = 1u64 << (bus % 64);
        if visited[usize::from(bus / 64)] & bit != 0 || !(first_bus..=last_bus).contains(&bus) {
            continue;
        }
        visited[usize::from(bus / 64)] |= bit;
        let Some(window) = map_bus(bus) else { continue };
        for device in 0..32 {
            for function in 0..8 {
                let address = Address { segment, bus, device, function };
                let Some(config) = window.window(address.offset_in_bus(), CONFIG_SIZE) else {
                    continue;
                };
                let Some(found_function) = Function::probe(address, config) else {
                    if function == 0 {
                        break;
                    }
                    continue;
                };
                let more = found_function.multifunction();
                if let Some(secondary) = found_function.secondary_bus()
                    && count < pending.len()
                {
                    pending[count] = secondary;
                    count += 1;
                }
                found(found_function);
                if function == 0 && !more {
                    break;
                }
            }
        }
    }
}

const MSI_CONTROL: usize = 2;
const MSI_ADDRESS: usize = 4;
const MSI_CONTROL_ENABLE: u16 = 1 << 0;
const MSI_CONTROL_MULTIPLE: u16 = 0x7 << 4;
const MSI_CONTROL_64BIT: u16 = 1 << 7;

/// A function's MSI capability: one interrupt message, written to a
/// fixed address.
#[derive(Clone, Copy, Debug)]
pub struct Msi {
    offset: usize,
}

impl Msi {
    /// The function's MSI capability, if it has one.
    pub fn find(function: &Function) -> Option<Msi> {
        function.find_capability(capability::MSI).map(|offset| Msi { offset })
    }

    /// Sets the function to send `message` for its one interrupt and turns
    /// MSI on.
    pub fn enable(&self, function: &Function, message: MsiMessage) {
        let control = function.read16(self.offset + MSI_CONTROL);
        let wide = control & MSI_CONTROL_64BIT != 0;
        function.write32(self.offset + MSI_ADDRESS, message.address as u32);
        let data_offset = if wide {
            function.write32(self.offset + MSI_ADDRESS + 4, (message.address >> 32) as u32);
            self.offset + 12
        } else {
            self.offset + 8
        };
        function.write16(data_offset, message.data as u16);
        let control = (control & !MSI_CONTROL_MULTIPLE) | MSI_CONTROL_ENABLE;
        function.write16(self.offset + MSI_CONTROL, control);
    }

    /// Turns MSI off.
    pub fn disable(&self, function: &Function) {
        let control = function.read16(self.offset + MSI_CONTROL);
        function.write16(self.offset + MSI_CONTROL, control & !MSI_CONTROL_ENABLE);
    }
}

const MSIX_CONTROL: usize = 2;
const MSIX_TABLE: usize = 4;
const MSIX_PBA: usize = 8;
const MSIX_CONTROL_ENABLE: u16 = 1 << 15;
const MSIX_CONTROL_MASK_ALL: u16 = 1 << 14;
/// Bytes per entry of the MSI-X table.
pub const MSIX_ENTRY_SIZE: usize = 16;

/// A function's MSI-X capability: a table of interrupt messages in one of
/// its memory BARs.
#[derive(Clone, Copy, Debug)]
pub struct MsiX {
    offset: usize,
    entries: u16,
    table_bar: usize,
    table_offset: u64,
}

impl MsiX {
    /// The function's MSI-X capability, if it has one.
    pub fn find(function: &Function) -> Option<MsiX> {
        let offset = function.find_capability(capability::MSIX)?;
        let control = function.read16(offset + MSIX_CONTROL);
        let table = function.read32(offset + MSIX_TABLE);
        let _pba = function.read32(offset + MSIX_PBA);
        Some(MsiX {
            offset,
            entries: (control & 0x7ff) + 1,
            table_bar: (table & 0x7) as usize,
            table_offset: u64::from(table & !0x7),
        })
    }

    /// How many interrupt messages the table holds.
    pub fn entries(&self) -> u16 {
        self.entries
    }

    /// Which BAR holds the table, and where in it.
    pub fn table_location(&self) -> (usize, u64) {
        (self.table_bar, self.table_offset)
    }

    /// The table's bytes in its BAR.
    pub fn table_len(&self) -> usize {
        usize::from(self.entries) * MSIX_ENTRY_SIZE
    }

    /// Turns MSI-X on with every entry masked by the function mask, so
    /// entries can be set before any fires.
    pub fn enable_masked(&self, function: &Function) {
        let control = function.read16(self.offset + MSIX_CONTROL);
        function.write16(
            self.offset + MSIX_CONTROL,
            control | MSIX_CONTROL_ENABLE | MSIX_CONTROL_MASK_ALL,
        );
    }

    /// Lifts the function mask: entries not masked themselves can fire.
    pub fn unmask_all(&self, function: &Function) {
        let control = function.read16(self.offset + MSIX_CONTROL);
        function.write16(self.offset + MSIX_CONTROL, control & !MSIX_CONTROL_MASK_ALL);
    }

    /// Turns MSI-X off.
    pub fn disable(&self, function: &Function) {
        let control = function.read16(self.offset + MSIX_CONTROL);
        function.write16(self.offset + MSIX_CONTROL, control & !MSIX_CONTROL_ENABLE);
    }

    /// Sets entry `index` of `table` (the table's window, from
    /// [`MsiX::table_location`]) to send `message`, unmasked.
    pub fn set_entry(&self, table: &Mmio, index: u16, message: MsiMessage) {
        let base = usize::from(index) * MSIX_ENTRY_SIZE;
        table.write32(base + 12, 1);
        table.write32(base, message.address as u32);
        table.write32(base + 4, (message.address >> 32) as u32);
        table.write32(base + 8, message.data);
        table.write32(base + 12, 0);
    }

    /// Masks entry `index` of `table`.
    pub fn mask_entry(&self, table: &Mmio, index: u16) {
        table.write32(usize::from(index) * MSIX_ENTRY_SIZE + 12, 1);
    }
}
