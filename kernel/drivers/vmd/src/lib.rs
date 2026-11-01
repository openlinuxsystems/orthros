// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel's Volume Management Device for the Radix kernel (Phase 11g, ADR
//! 0249): the PCI domain a VMD keeps its NVMe disks in.
//!
//! On many laptops with Intel processors from Tiger Lake on (and on Xeon
//! servers), the firmware's "RST" or "RAID" setting hides the NVMe disks,
//! and the root ports they are on, behind a VMD: a PCI function of its own
//! (class 0104h, Intel's vendor ID and one of [`DEVICES`]) whose BARs are
//! the configuration space of a PCI domain of its own (CFGBAR, BAR 0,
//! one megabyte for each bus as ECAM lays it out, counted from the first
//! bus the domain has), and two windows of memory the devices in it are
//! placed in (MEMBAR1, BAR 2, below 4 GiB, and MEMBAR2, BAR 4,
//! prefetchable; MEMBAR2's first [`MEMBAR2_KEPT`] bytes are the VMD's own
//! MSI-X table and registers). The firmware sets nothing in that domain:
//! the operating system numbers its buses, and assigns its devices' BARs
//! and its bridges' windows, as it would a host bridge's.
//!
//! [`first_bus`] reads where the domain's buses start: on the models whose
//! buses are restricted, VMCONFIG's bits 8 and 9 say 0, 128 or 224.
//! [`Plan::configure`] then walks the domain through a [`Config`] the
//! caller gives: it turns every device's decoding off, numbers the buses
//! behind each bridge depth first, sizes every memory BAR, packs them into
//! the bridges' windows from the largest alignment down, places the
//! windows and the first bus's BARs in MEMBAR1 and MEMBAR2, writes them,
//! and turns the bridges' and devices' memory on. Prefetchable 64-bit
//! BARs go into MEMBAR2 where every bridge above them has a 64-bit
//! prefetchable window; every other memory BAR into MEMBAR1. The domain
//! has no I/O space, so I/O BARs are left off.
//!
//! The registers are those of the *PCI Local Bus Specification* (3.0) and
//! the *PCI-to-PCI Bridge Architecture Specification* (1.2); VMCAP and
//! VMCONFIG were read from FreeBSD's `vmd(4)` (BSD-2-Clause), from which
//! no code is taken. The crate is a compartment with no `unsafe` code; its
//! tests run it against a simulated domain.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use core::fmt;

/// Intel's vendor ID.
pub const VENDOR: u16 = 0x8086;

/// The device IDs of the VMDs known, and whether their buses are
/// restricted (start at 0, 128 or 224 as VMCONFIG says).
pub const DEVICES: [(u16, bool); 10] = [
    // Xeon Scalable, Skylake and Cascade Lake.
    (0x201d, false),
    // Xeon Scalable, Ice Lake and later.
    (0x28c0, true),
    // Core, 11th generation (Tiger Lake).
    (0x9a0b, true),
    // Core, 11th generation desktop (Rocket Lake).
    (0x4c3d, true),
    // Core, 12th generation (Alder Lake).
    (0x467f, true),
    // Core, 13th and 14th generation (Raptor Lake).
    (0xa77f, true),
    // Core Ultra, series 1 (Meteor Lake).
    (0x7d0b, true),
    // Core Ultra, series 2 (Arrow Lake).
    (0xad0b, true),
    // Later Core Ultra processors.
    (0xb06f, true),
    (0xb60b, true),
];

/// The VMD's own configuration registers: VMCAP (bit 0: its buses are
/// restricted) and VMCONFIG (bits 8 and 9: where they start).
pub const VMCAP: usize = 0x40;
/// See [`VMCAP`].
pub const VMCONFIG: usize = 0x44;

/// The BARs of the VMD: its domain's configuration space, and the two
/// windows of memory for the devices in it.
pub const CFGBAR: usize = 0;
/// See [`CFGBAR`].
pub const MEMBAR1: usize = 2;
/// See [`CFGBAR`].
pub const MEMBAR2: usize = 4;

/// MEMBAR2's bytes the VMD keeps: its MSI-X table and pending bits (the
/// first 8 KiB), and the 16 bytes of shadow registers after them on the
/// models that have them.
pub const MEMBAR2_KEPT: u64 = 0x2010;

/// The most functions a domain has here: root ports, switches and disks.
pub const MAX_FUNCTIONS: usize = 32;
/// The most memory BARs of them.
const MAX_NEEDS: usize = 96;
/// The most bridges deep a domain is walked.
const MAX_DEPTH: u8 = 6;

/// Bridges' windows are aligned to and sized in megabytes.
const WINDOW_LOG2: u8 = 20;

/// A PCI function's configuration registers.
mod reg {
    pub const ID: u16 = 0x00;
    pub const COMMAND: u16 = 0x04;
    pub const HEADER: u16 = 0x0c;
    pub const BAR0: u16 = 0x10;
    pub const BUS_NUMBERS: u16 = 0x18;
    pub const IO_BASE: u16 = 0x1c;
    pub const MEMORY_BASE: u16 = 0x20;
    pub const PREFETCHABLE_BASE: u16 = 0x24;
    pub const PREFETCHABLE_BASE_UPPER: u16 = 0x28;
    pub const PREFETCHABLE_LIMIT_UPPER: u16 = 0x2c;
    pub const IO_UPPER: u16 = 0x30;
}

/// The command register's bits.
mod command {
    pub const IO: u16 = 1 << 0;
    pub const MEMORY: u16 = 1 << 1;
    pub const BUS_MASTER: u16 = 1 << 2;
}

/// Where a function is in the domain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Location {
    /// Its bus.
    pub bus: u8,
    /// Its device, 0 to 31.
    pub device: u8,
    /// Its function, 0 to 7.
    pub function: u8,
}

/// The domain's configuration space: CFGBAR, as the caller maps it. A
/// function that is not there reads all ones.
pub trait Config {
    /// Reads the 32 bits at `offset` (a multiple of 4) of `at`'s registers.
    fn read32(&mut self, at: Location, offset: u16) -> u32;
    /// Writes the 32 bits at `offset` (a multiple of 4).
    fn write32(&mut self, at: Location, offset: u16, value: u32);
    /// Writes the 16 bits at `offset` (a multiple of 2).
    fn write16(&mut self, at: Location, offset: u16, value: u16);
}

/// A range of memory: a window of the VMD's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    /// Its first byte.
    pub base: u64,
    /// Its bytes.
    pub size: u64,
}

impl Range {
    /// One past its last byte.
    pub fn end(&self) -> u64 {
        self.base.saturating_add(self.size)
    }
}

/// The domain behind a VMD: its buses and its two windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Domain {
    /// Its first bus, and its last.
    pub first_bus: u8,
    /// See `first_bus`.
    pub last_bus: u8,
    /// MEMBAR1: memory below 4 GiB.
    pub memory: Range,
    /// MEMBAR2, past what the VMD keeps of it: prefetchable memory.
    pub prefetchable: Range,
}

impl Domain {
    /// The domain whose buses start at `first_bus`, with as many as
    /// CFGBAR's `config_size` bytes hold (up to the 255th), and whose
    /// windows are the BARs `membar1` and `membar2`.
    pub fn new(
        first_bus: u8,
        config_size: u64,
        membar1: Range,
        membar2: Range,
    ) -> Result<Domain, Error> {
        let buses = config_size >> 20;
        if buses == 0 {
            return Err(Error::NoConfigSpace);
        }
        let last = (u64::from(first_bus) + buses - 1).min(255) as u8;
        if membar1.end() > 1 << 32 {
            return Err(Error::MemoryAbove4G);
        }
        let prefetchable = if membar2.size > MEMBAR2_KEPT {
            Range { base: membar2.base + MEMBAR2_KEPT, size: membar2.size - MEMBAR2_KEPT }
        } else {
            Range::default()
        };
        Ok(Domain { first_bus, last_bus: last, memory: membar1, prefetchable })
    }
}

/// Where the domain's buses start, from the VMD's VMCAP and VMCONFIG:
/// the first bus unless `restricted` (from [`DEVICES`]) and VMCAP says
/// they are.
pub fn first_bus(restricted: bool, vmcap: u32, vmconfig: u32) -> Result<u8, Error> {
    if !restricted || vmcap & 1 == 0 {
        return Ok(0);
    }
    match (vmconfig >> 8) & 0x3 {
        0 => Ok(0),
        1 => Ok(128),
        2 => Ok(224),
        _ => Err(Error::BusRange),
    }
}

/// Whether `vendor` and `device` are a VMD's, and if so whether its buses
/// are restricted.
pub fn model(vendor: u16, device: u16) -> Option<bool> {
    if vendor != VENDOR {
        return None;
    }
    DEVICES.iter().find(|&&(id, _)| id == device).map(|&(_, restricted)| restricted)
}

/// Why a domain could not be set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// CFGBAR has not even one bus.
    NoConfigSpace,
    /// VMCONFIG names a bus range the specification reserves.
    BusRange,
    /// MEMBAR1 lies above 4 GiB, where bridges cannot reach.
    MemoryAbove4G,
    /// More than [`MAX_FUNCTIONS`] functions, or more memory BARs than
    /// are kept.
    TooManyFunctions,
    /// The devices need more memory than a window has.
    NoRoom {
        /// The prefetchable window, MEMBAR2, else MEMBAR1.
        prefetchable: bool,
        /// The bytes needed.
        needed: u64,
        /// The window's.
        room: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoConfigSpace => f.write_str("its configuration space (BAR 0) has no bus"),
            Error::BusRange => f.write_str("it names a range of buses that is reserved"),
            Error::MemoryAbove4G => f.write_str("its memory window (BAR 2) lies above 4 GiB"),
            Error::TooManyFunctions => {
                write!(f, "more than {MAX_FUNCTIONS} functions are behind it")
            }
            Error::NoRoom { prefetchable, needed, room } => write!(
                f,
                "its devices need {} KiB of {}memory and its window has {} KiB",
                needed.div_ceil(1024),
                if *prefetchable { "prefetchable " } else { "" },
                room / 1024
            ),
        }
    }
}

/// What [`Plan::configure`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// The last bus numbered.
    pub last_bus: u8,
    /// Bridges found.
    pub bridges: usize,
    /// Other functions found.
    pub endpoints: usize,
    /// I/O BARs, left off.
    pub io_bars: usize,
    /// Bridges given no bus: too deep, or past the domain's last.
    pub unnumbered: usize,
    /// Bytes of MEMBAR1 and of MEMBAR2 used.
    pub memory_used: u64,
    /// See `memory_used`.
    pub prefetchable_used: u64,
}

/// The windows a bridge forwards, and the BARs: memory below 4 GiB, and
/// prefetchable memory.
const MEMORY: usize = 0;
const PREFETCHABLE: usize = 1;

/// The function on the domain's first bus has no bridge above it.
const ROOT: u8 = u8::MAX;

/// A bridge's window: its size and alignment, where it is in the window
/// above, and its address.
#[derive(Clone, Copy, Debug, Default)]
struct Window {
    size: u64,
    align: u8,
    offset: u64,
    base: u64,
}

/// A function found.
#[derive(Clone, Copy, Debug, Default)]
struct Entry {
    at: Location,
    /// The bridge it is behind, an index of the entries, or [`ROOT`].
    parent: u8,
    bridge: bool,
    /// Its prefetchable BARs may go into a prefetchable window.
    prefetchable_ok: bool,
    /// A bridge: its prefetchable window has 64-bit addresses.
    wide_prefetchable: bool,
    /// A bridge: the buses behind it, none if `secondary` is 0.
    secondary: u8,
    subordinate: u8,
    windows: [Window; 2],
    /// It has a memory BAR placed.
    decodes: bool,
}

/// A memory BAR.
#[derive(Clone, Copy, Debug, Default)]
struct Need {
    owner: u8,
    bar: u8,
    log2: u8,
    wide: bool,
    space: usize,
    offset: u64,
}

/// What the walk of a domain found and placed; [`Plan::new`] is empty.
/// Kept apart from the caller's stack, as it is a few kilobytes.
pub struct Plan {
    entries: [Entry; MAX_FUNCTIONS],
    count: usize,
    needs: [Need; MAX_NEEDS],
    need_count: usize,
    summary: Summary,
}

impl Default for Plan {
    fn default() -> Plan {
        Plan::new()
    }
}

const EMPTY_WINDOW: Window = Window { size: 0, align: 0, offset: 0, base: 0 };
const EMPTY_ENTRY: Entry = Entry {
    at: Location { bus: 0, device: 0, function: 0 },
    parent: ROOT,
    bridge: false,
    prefetchable_ok: false,
    wide_prefetchable: false,
    secondary: 0,
    subordinate: 0,
    windows: [EMPTY_WINDOW; 2],
    decodes: false,
};
const EMPTY_NEED: Need = Need { owner: 0, bar: 0, log2: 0, wide: false, space: 0, offset: 0 };
const EMPTY_SUMMARY: Summary = Summary {
    last_bus: 0,
    bridges: 0,
    endpoints: 0,
    io_bars: 0,
    unnumbered: 0,
    memory_used: 0,
    prefetchable_used: 0,
};

impl Plan {
    /// An empty plan.
    pub const fn new() -> Plan {
        Plan {
            entries: [EMPTY_ENTRY; MAX_FUNCTIONS],
            count: 0,
            needs: [EMPTY_NEED; MAX_NEEDS],
            need_count: 0,
            summary: EMPTY_SUMMARY,
        }
    }

    /// Walks `domain` through `config`: numbers its buses, places every
    /// memory BAR and bridge window, writes them, and turns the memory of
    /// what has some on (and bridges' forwarding of DMA). Leaves bus
    /// mastering of the devices themselves to their drivers.
    pub fn configure<C: Config>(
        &mut self,
        config: &mut C,
        domain: &Domain,
    ) -> Result<Summary, Error> {
        // Reset in place: a new plan would be built on the stack first.
        self.count = 0;
        self.need_count = 0;
        self.summary = EMPTY_SUMMARY;
        let mut next = u16::from(domain.first_bus) + 1;
        let prefetchable_ok = domain.prefetchable.size > 0;
        self.discover(config, domain, domain.first_bus, ROOT, prefetchable_ok, 0, &mut next)?;
        self.summary.last_bus = (next - 1) as u8;
        for index in 0..self.count {
            self.size_bars(config, index)?;
        }
        // Children were found after their bridges, so the deepest bridges
        // are measured first.
        for index in (0..self.count).rev() {
            if self.entries[index].bridge {
                for space in [MEMORY, PREFETCHABLE] {
                    let end = self.pack(index as u8, space, 0);
                    let window = &mut self.entries[index].windows[space];
                    window.size = if end == 0 { 0 } else { align_up(end, WINDOW_LOG2) };
                    window.align = window.align.max(WINDOW_LOG2);
                }
            }
        }
        for (space, range) in [(MEMORY, domain.memory), (PREFETCHABLE, domain.prefetchable)] {
            let end = self.pack(ROOT, space, range.base);
            let used = end.saturating_sub(range.base);
            if used > 0 && end > range.end() {
                return Err(Error::NoRoom {
                    prefetchable: space == PREFETCHABLE,
                    needed: used,
                    room: range.size,
                });
            }
            if space == MEMORY {
                self.summary.memory_used = used;
            } else {
                self.summary.prefetchable_used = used;
            }
        }
        self.place();
        self.write(config);
        Ok(self.summary)
    }

    /// Finds the functions on `bus`, behind the bridge `parent`, and
    /// numbers the buses behind its bridges from `next`.
    #[allow(clippy::too_many_arguments, reason = "the walk's state, kept off a struct")]
    fn discover<C: Config>(
        &mut self,
        config: &mut C,
        domain: &Domain,
        bus: u8,
        parent: u8,
        prefetchable_ok: bool,
        depth: u8,
        next: &mut u16,
    ) -> Result<(), Error> {
        for device in 0..32 {
            for function in 0..8 {
                let at = Location { bus, device, function };
                if config.read32(at, reg::ID) & 0xffff == 0xffff {
                    if function == 0 {
                        break;
                    }
                    continue;
                }
                let header = (config.read32(at, reg::HEADER) >> 16) as u8;
                let saved = config.read32(at, reg::COMMAND) as u16;
                config.write16(
                    at,
                    reg::COMMAND,
                    saved & !(command::IO | command::MEMORY | command::BUS_MASTER),
                );
                if self.count >= MAX_FUNCTIONS {
                    return Err(Error::TooManyFunctions);
                }
                let index = self.count;
                self.count += 1;
                let bridge = header & 0x7f == 1;
                let mut entry = Entry { at, parent, bridge, prefetchable_ok, ..EMPTY_ENTRY };
                if bridge {
                    self.summary.bridges += 1;
                    close_windows(config, at);
                    entry.wide_prefetchable = config.read32(at, reg::PREFETCHABLE_BASE) & 0xf == 1;
                    if depth >= MAX_DEPTH || *next > u16::from(domain.last_bus) {
                        // Behind it stays unreachable.
                        self.summary.unnumbered += 1;
                        set_buses(config, at, bus, 0, 0);
                        self.entries[index] = entry;
                    } else {
                        let secondary = *next as u8;
                        *next += 1;
                        // Every bus to the last forwarded while those
                        // behind are found.
                        set_buses(config, at, bus, secondary, domain.last_bus);
                        entry.secondary = secondary;
                        self.entries[index] = entry;
                        let behind = prefetchable_ok && entry.wide_prefetchable;
                        self.discover(
                            config,
                            domain,
                            secondary,
                            index as u8,
                            behind,
                            depth + 1,
                            next,
                        )?;
                        let subordinate = (*next - 1) as u8;
                        set_buses(config, at, bus, secondary, subordinate);
                        self.entries[index].subordinate = subordinate;
                    }
                } else {
                    self.summary.endpoints += 1;
                    self.entries[index] = entry;
                }
                if function == 0 && header & 0x80 == 0 {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Sizes entry `index`'s BARs, with its decoding off: writes all ones
    /// to each, reads back which bits stuck, and clears it.
    fn size_bars<C: Config>(&mut self, config: &mut C, index: usize) -> Result<(), Error> {
        let entry = self.entries[index];
        let count = if entry.bridge { 2 } else { 6 };
        let at = entry.at;
        let mut bar = 0;
        while bar < count {
            let offset = reg::BAR0 + 4 * bar as u16;
            let low = config.read32(at, offset);
            if low & 1 == 1 {
                config.write32(at, offset, u32::MAX);
                let mask = config.read32(at, offset) & !0x3;
                config.write32(at, offset, 0);
                if mask != 0 {
                    self.summary.io_bars += 1;
                }
                bar += 1;
                continue;
            }
            let wide = (low >> 1) & 0x3 == 0x2 && bar + 1 < count;
            let prefetchable = low & 0x8 != 0;
            config.write32(at, offset, u32::MAX);
            let mask_low = config.read32(at, offset) & !0xf;
            config.write32(at, offset, 0);
            let mask_high = if wide {
                config.write32(at, offset + 4, u32::MAX);
                let mask = config.read32(at, offset + 4);
                config.write32(at, offset + 4, 0);
                mask
            } else {
                u32::MAX
            };
            let mask = (u64::from(mask_high) << 32) | u64::from(mask_low);
            let size = (!mask).wrapping_add(1);
            if mask_low != 0 && size.is_power_of_two() {
                if self.need_count >= MAX_NEEDS {
                    return Err(Error::TooManyFunctions);
                }
                let space = if prefetchable && wide && entry.prefetchable_ok {
                    PREFETCHABLE
                } else {
                    MEMORY
                };
                self.needs[self.need_count] = Need {
                    owner: index as u8,
                    bar: bar as u8,
                    log2: size.trailing_zeros() as u8,
                    wide,
                    space,
                    offset: 0,
                };
                self.need_count += 1;
            }
            bar += if wide { 2 } else { 1 };
        }
        Ok(())
    }

    /// Lays out what goes in `space` behind `container` (an entry's index,
    /// or [`ROOT`]) from `start`, the largest alignment first, so that
    /// each lands aligned with little left between; records where each
    /// lands, and returns the end. Saturates at `u64::MAX`.
    fn pack(&mut self, container: u8, space: usize, start: u64) -> u64 {
        let mut cursor = start;
        let mut most = 0;
        for log2 in (0..64u8).rev() {
            for need in &mut self.needs[..self.need_count] {
                if need.space == space
                    && need.log2 == log2
                    && self.entries[usize::from(need.owner)].parent == container
                {
                    cursor = align_up(cursor, log2);
                    need.offset = cursor;
                    cursor = cursor.saturating_add(1 << log2);
                    most = most.max(log2);
                }
            }
            for entry in &mut self.entries[..self.count] {
                let window = &mut entry.windows[space];
                if entry.bridge
                    && entry.parent == container
                    && window.size > 0
                    && window.align == log2
                {
                    cursor = align_up(cursor, log2);
                    window.offset = cursor;
                    cursor = cursor.saturating_add(window.size);
                    most = most.max(log2);
                }
            }
        }
        if container != ROOT {
            let window = &mut self.entries[usize::from(container)].windows[space];
            window.align = most;
        }
        cursor
    }

    /// Turns the offsets [`Plan::pack`] found into addresses: the first
    /// bus's are already, the rest are their bridge's window's base plus
    /// theirs. Bridges come before what is behind them.
    fn place(&mut self) {
        for index in 0..self.count {
            let entry = self.entries[index];
            if !entry.bridge {
                continue;
            }
            for space in [MEMORY, PREFETCHABLE] {
                let offset = entry.windows[space].offset;
                let base = match entry.parent {
                    ROOT => offset,
                    parent => self.entries[usize::from(parent)].windows[space].base + offset,
                };
                self.entries[index].windows[space].base = base;
            }
        }
        for need in &mut self.needs[..self.need_count] {
            let parent = self.entries[usize::from(need.owner)].parent;
            if parent != ROOT {
                need.offset += self.entries[usize::from(parent)].windows[need.space].base;
            }
            self.entries[usize::from(need.owner)].decodes = true;
        }
    }

    /// Writes the BARs and windows placed, then turns memory on.
    fn write<C: Config>(&self, config: &mut C) {
        for need in &self.needs[..self.need_count] {
            let at = self.entries[usize::from(need.owner)].at;
            let offset = reg::BAR0 + 4 * u16::from(need.bar);
            config.write32(at, offset, need.offset as u32);
            if need.wide {
                config.write32(at, offset + 4, (need.offset >> 32) as u32);
            }
        }
        for entry in &self.entries[..self.count] {
            if !entry.bridge {
                continue;
            }
            let at = entry.at;
            let memory = entry.windows[MEMORY];
            if memory.size > 0 {
                let limit = memory.base + memory.size - 1;
                config.write32(at, reg::MEMORY_BASE, window_register(memory.base, limit));
            }
            let prefetchable = entry.windows[PREFETCHABLE];
            if prefetchable.size > 0 {
                let limit = prefetchable.base + prefetchable.size - 1;
                config.write32(at, reg::PREFETCHABLE_BASE_UPPER, (prefetchable.base >> 32) as u32);
                config.write32(at, reg::PREFETCHABLE_LIMIT_UPPER, (limit >> 32) as u32);
                config.write32(
                    at,
                    reg::PREFETCHABLE_BASE,
                    window_register(prefetchable.base, limit),
                );
            }
        }
        for entry in &self.entries[..self.count] {
            let on = if entry.bridge {
                command::MEMORY | command::BUS_MASTER
            } else if entry.decodes {
                command::MEMORY
            } else {
                0
            };
            if on != 0 {
                let now = config.read32(entry.at, reg::COMMAND) as u16;
                config.write16(entry.at, reg::COMMAND, now | on);
            }
        }
    }
}

/// `value` rounded up to a multiple of 2^`log2`, saturating.
fn align_up(value: u64, log2: u8) -> u64 {
    let mask = (1u64 << log2) - 1;
    value.checked_add(mask).map_or(u64::MAX, |v| v & !mask)
}

/// A bridge's base and limit register: bits 20 to 31 of each, in the
/// top twelve bits of each half.
fn window_register(base: u64, limit: u64) -> u32 {
    let half = |address: u64| ((address >> 16) as u32) & 0xfff0;
    half(base) | (half(limit) << 16)
}

/// Sets a bridge's primary, secondary and subordinate bus numbers,
/// keeping its secondary latency timer.
fn set_buses<C: Config>(config: &mut C, at: Location, primary: u8, secondary: u8, last: u8) {
    let latency = config.read32(at, reg::BUS_NUMBERS) & 0xff00_0000;
    let numbers = u32::from(primary) | (u32::from(secondary) << 8) | (u32::from(last) << 16);
    config.write32(at, reg::BUS_NUMBERS, latency | numbers);
}

/// Closes every window of a bridge: each base above its limit.
fn close_windows<C: Config>(config: &mut C, at: Location) {
    config.write16(at, reg::IO_BASE, 0x00f0);
    config.write32(at, reg::IO_UPPER, 0);
    config.write32(at, reg::MEMORY_BASE, 0x0000_fff0);
    config.write32(at, reg::PREFETCHABLE_BASE, 0x0000_fff0);
    config.write32(at, reg::PREFETCHABLE_BASE_UPPER, u32::MAX);
    config.write32(at, reg::PREFETCHABLE_LIMIT_UPPER, 0);
}
