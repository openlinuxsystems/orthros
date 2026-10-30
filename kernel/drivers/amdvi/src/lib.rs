// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! AMD-Vi DMA remapping (the *AMD I/O Virtualization Technology (IOMMU)
//! Specification*, revision 3.10), for the IOMMUs the ACPI IVRS table
//! describes.
//!
//! An IOMMU sits between a group of PCI devices and memory, and
//! translates each of their DMA requests by the *device ID* it carries,
//! the device's bus, device and function: the device table's entry for it
//! names the device's *domain* and its page table, of AMD-Vi's own format
//! ([`HostPages`]). A device whose entry says so reaches no memory at all,
//! and one that touches a page its domain does not map is stopped, and
//! the IOMMU logs an event.
//!
//! - [`Unit`] takes an IOMMU over: switches off what firmware left on,
//!   gives it a device table in which every device reaches nothing, a
//!   command buffer and an event log, and turns translation on. It
//!   attaches domains to devices and has the IOMMU forget what it caches
//!   after a change ([`Invalidate`]).
//! - [`Domain`] is a device's page table, `radix-iopt`'s, each page mapped
//!   at its own physical address. AMD-Vi leaves only seven bits of an
//!   entry to software, so a page's entry counts its mappings there, up
//!   to 127, and a table is found empty by looking at its entries.
//! - [`EventLog`] reads the events the IOMMU logs, apart from the rest of
//!   it, for its interrupt.
//!
//! - Interrupt remapping: each device's entry can point to a table of
//!   [`INTERRUPTS`] interrupt entries, which an interrupt the device raises
//!   picks by the low bits of its message's data, and takes its vector and
//!   destination from. [`Unit::remap`] writes an entry. With
//!   [`Setup::remap_interrupts`], every device's interrupts are blocked
//!   until then; without it, a device whose entry has no table raises
//!   its interrupts as they are.
//!
//! The crate is a compartment: it has no `unsafe` code. It reaches an
//! IOMMU through its [`Mmio`] registers and its tables through the
//! [`Tables`] the kernel gives it. Nothing here sleeps or takes a lock:
//! the kernel serializes each unit and each domain, and passes in how to
//! wait. The tests run it against a simulated IOMMU that walks the tables
//! as the specification says hardware does.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use core::fmt;
use core::sync::atomic::Ordering;

use radix_hw::{Mmio, device_barrier};
pub use radix_iopt::{Format, Invalidate, MapError, Memory, PAGE, blocks};

/// Memory for an IOMMU's tables, with the runs of pages its device table
/// takes.
pub trait Tables: Memory + Clone {
    /// `count` zeroed pages in a row, by the first's physical address;
    /// `None` when memory runs out.
    fn alloc_run(&self, count: usize) -> Option<u64>;
    /// Gives back a run [`Tables::alloc_run`] gave.
    fn free_run(&self, phys: u64, count: usize);
}

/// The registers (AMD IOMMU 3.10, section 3.4).
mod reg {
    pub const DEVICE_TABLE: usize = 0x0000;
    pub const COMMAND_BUFFER: usize = 0x0008;
    pub const EVENT_LOG: usize = 0x0010;
    pub const CONTROL: usize = 0x0018;
    pub const EXCLUSION_BASE: usize = 0x0020;
    pub const EXCLUSION_LIMIT: usize = 0x0028;
    pub const FEATURES: usize = 0x0030;
    pub const COMMAND_HEAD: usize = 0x2000;
    pub const COMMAND_TAIL: usize = 0x2008;
    pub const EVENT_HEAD: usize = 0x2010;
    pub const EVENT_TAIL: usize = 0x2018;
    pub const STATUS: usize = 0x2020;
    /// The registers the engine reaches lie below this.
    pub const END: usize = 0x4000;
}

/// Bits of the control register.
mod control {
    pub const ENABLE: u64 = 1 << 0;
    pub const HT_TUNNEL: u64 = 1 << 1;
    pub const EVENT_LOG: u64 = 1 << 2;
    pub const EVENT_INTERRUPT: u64 = 1 << 3;
    /// Invalidations time out after a second.
    pub const TIMEOUT_1S: u64 = 4 << 5;
    pub const PASS_POSTED_WRITE: u64 = 1 << 8;
    pub const PASS_RESPONSE: u64 = 1 << 9;
    pub const COHERENT: u64 = 1 << 10;
    pub const ISOCHRONOUS: u64 = 1 << 11;
    pub const COMMAND_BUFFER: u64 = 1 << 12;
}

/// Bits of the status register.
mod status {
    pub const EVENT_OVERFLOW: u64 = 1 << 0;
    pub const EVENT_INTERRUPT: u64 = 1 << 1;
    pub const COMPLETION_INTERRUPT: u64 = 1 << 2;
    pub const EVENT_LOG_RUNNING: u64 = 1 << 3;
    pub const COMMANDS_RUNNING: u64 = 1 << 4;
}

/// Command opcodes, in bits 60 to 63 of a command's first word.
mod command {
    pub const COMPLETION_WAIT: u64 = 1 << 60;
    pub const INVALIDATE_DEVICE: u64 = 2 << 60;
    pub const INVALIDATE_PAGES: u64 = 3 << 60;
    pub const INVALIDATE_INTERRUPTS: u64 = 5 << 60;
    pub const INVALIDATE_ALL: u64 = 8 << 60;
}

/// Bits of a device table entry's first word.
mod dte {
    pub const VALID: u64 = 1 << 0;
    pub const TRANSLATION_VALID: u64 = 1 << 1;
    pub const READ: u64 = 1 << 61;
    pub const WRITE: u64 = 1 << 62;
    /// Reaches nothing: translation valid, mode 0 (no page table), and
    /// neither read nor write allowed.
    pub const BLOCKED: u64 = VALID | TRANSLATION_VALID;
}

/// Bits of a device table entry's third word, its interrupts'.
mod interrupt {
    /// The word is valid: its interrupts are not passed through as they
    /// are.
    pub const VALID: u64 = 1 << 0;
    /// A table of 2 to the 8 entries.
    pub const LENGTH: u64 = 8 << 1;
    /// The table's address.
    pub const TABLE: u64 = 0x000f_ffff_ffff_ffc0;
    /// What happens to fixed and arbitrated interrupts: aborted, or
    /// remapped through the table.
    pub const CONTROL: u64 = 3 << 60;
    pub const REMAPPED: u64 = 2 << 60;
}

/// The entries of a device's interrupt remapping table: one for each
/// vector.
pub const INTERRUPTS: usize = 256;

/// The IOMMU walks 4-level tables: every IOMMU does (HATS 0).
pub const LEVELS: u8 = 4;
/// The bytes of a device table entry.
const DTE_BYTES: usize = 32;
/// Commands and events are 16 bytes; each ring takes one page of them.
const RING_BYTES: u64 = PAGE;
/// The size field of a ring of 256 entries: log2 of its length.
const RING_SIZE: u64 = 8 << 56;
/// The byte offsets the head and tail registers hold.
const RING_POINTER: u64 = 0x7_fff0;
/// Commands put in the ring before a completion wait.
const BATCH: usize = 64;
/// An invalidation of every page of a domain: the size bit set, with every
/// address bit but the top one.
const EVERY_PAGE: u64 = 0x7fff_ffff_ffff_f000;

/// How long to wait for the IOMMU to do what it was told.
const WAIT_US: u32 = 1_000_000;
/// How often to look meanwhile.
const LOOK_US: u32 = 1;

/// AMD-Vi's page table entries (section 2.2.3): present in bit 0, the
/// next level in bits 9 to 11, read and write in bits 61 and 62. The
/// IOMMU ignores bits 1 to 4, 7, 8 and 63, which keep a page's count.
#[derive(Debug)]
pub struct HostPages;

/// The bits of an entry the IOMMU ignores.
const IGNORED: u64 = (0xf << 1) | (3 << 7) | (1 << 63);
const PRESENT: u64 = 1 << 0;
const NEXT_LEVEL_SHIFT: u32 = 9;
/// Forces the device's accesses to the page to be coherent with the CPUs'
/// caches.
const FORCE_COHERENT: u64 = 1 << 60;
const READ: u64 = 1 << 61;
const WRITE: u64 = 1 << 62;

impl Format for HostPages {
    const COUNT_MAX: u64 = 0x7f;
    const COUNTS_TABLES: bool = false;

    fn table(child: u64, level: u8) -> u64 {
        child | PRESENT | (u64::from(level - 1) << NEXT_LEVEL_SHIFT) | READ | WRITE
    }

    fn page(address: u64) -> u64 {
        address | PRESENT | FORCE_COHERENT | READ | WRITE
    }

    fn present(entry: u64) -> bool {
        entry & PRESENT != 0
    }

    fn count(entry: u64) -> u64 {
        ((entry >> 1) & 0xf) | (((entry >> 7) & 3) << 4) | ((entry >> 63) << 6)
    }

    fn with_count(entry: u64, count: u64) -> u64 {
        (entry & !IGNORED) | ((count & 0xf) << 1) | (((count >> 4) & 3) << 7) | ((count >> 6) << 63)
    }
}

/// A device's page table, in AMD-Vi's format.
pub type Domain = radix_iopt::Domain<HostPages>;

/// What an IOMMU can do: its extended feature register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features(pub u64);

impl Features {
    /// It takes the command that invalidates everything it caches.
    pub fn invalidate_all(&self) -> bool {
        self.0 & (1 << 6) != 0
    }

    /// The most page table levels it walks.
    pub fn levels(&self) -> u8 {
        4 + ((self.0 >> 10) & 3) as u8
    }
}

/// Why an IOMMU could not be taken over, or stopped working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Its registers reach past the window mapped for them.
    Registers,
    /// Memory for its tables ran out.
    NoMemory,
    /// It did not do what it was told within a second.
    Stuck(&'static str),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Registers => f.write_str("its registers reach past what was mapped"),
            Refused::NoMemory => f.write_str("out of memory for its tables"),
            Refused::Stuck(what) => write!(f, "it did not {what} within a second"),
        }
    }
}

/// How firmware says an IOMMU is to be set up: the IVHD block's flags and
/// what its capability block says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Setup {
    /// The IVHD flags: HtTunEn, PassPW, ResPassPW and Isoc, from bit 0.
    pub flags: u8,
    /// The highest device ID it translates: its device table reaches it.
    pub last_device: u16,
    /// It caches entries that are not present (the capability block's
    /// NpCache bit), as an emulated IOMMU may.
    pub caches_absent: bool,
    /// Every device's interrupts are blocked until [`Unit::remap`] gives
    /// it some.
    pub remap_interrupts: bool,
}

/// An event the IOMMU logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// The device ID of the device it is about, if it is about one.
    pub device: Option<u16>,
    /// The address it is about: the page a device tried to reach, or a
    /// table's or command's.
    pub address: u64,
    /// The device tried to write, not read.
    pub write: bool,
    /// The event's code (section 2.5).
    pub code: u8,
    /// Its flags.
    pub flags: u16,
}

impl Event {
    fn decode(low: u64, high: u64) -> Event {
        let code = ((low >> 60) & 0xf) as u8;
        let flags = ((low >> 48) & 0xfff) as u16;
        let device = matches!(code, 1..=4 | 7 | 8).then_some(low as u16);
        Event { device, address: high, write: flags & (1 << 5) != 0, code, flags }
    }

    /// It is about an interrupt the device raised, not a DMA request.
    pub fn interrupt(&self) -> bool {
        self.flags & (1 << 3) != 0
    }

    /// What it means.
    pub fn why(&self) -> &'static str {
        let present = self.flags & (1 << 4) != 0;
        let reserved = self.flags & (1 << 7) != 0;
        match self.code {
            1 | 2 if self.interrupt() => "the interrupt is not one given to it",
            1 => "its device table entry is invalid",
            2 if reserved => "a page table entry has a reserved bit set",
            2 if !present => "the page is not mapped for it",
            2 => "the page is mapped without that access",
            3 => "the IOMMU could not read its device table",
            4 => "the IOMMU could not read a page table",
            5 => "the IOMMU refused a command",
            6 => "the IOMMU could not read a command",
            7 => "its IOTLB did not answer an invalidation",
            8 => "it made a request the IOMMU does not take",
            _ => "an event of another kind",
        }
    }
}

/// An IOMMU's command buffer.
struct Commands {
    /// The page of 16-byte commands.
    base: u64,
    /// The page whose first word a completion wait writes.
    done: u64,
    /// Where the next command goes, in bytes.
    tail: u64,
    /// The number the last completion wait writes.
    sequence: u64,
}

/// An IOMMU, taken over.
pub struct Unit<M: Tables> {
    regs: Mmio,
    memory: M,
    features: Features,
    setup: Setup,
    /// The device table: its first page and how many.
    devices: (u64, usize),
    commands: Commands,
    events: u64,
    control: u64,
    found_on: bool,
}

impl<M: Tables> Unit<M> {
    /// Takes over the IOMMU whose registers are `regs`, set up as `setup`
    /// says: switches off what firmware or an earlier kernel left on,
    /// gives it a device table in which every device reaches nothing, a
    /// command buffer and an event log, turns translation on, and has it
    /// forget what it cached. An IOMMU runs its command buffer only while
    /// it translates (QEMU's does so, and Linux takes one over in this
    /// order), so translation goes on with every device blocked, and each
    /// device reaches memory once [`Unit::attach`]ed.
    pub fn new(regs: Mmio, memory: M, setup: Setup) -> Result<Unit<M>, Refused> {
        if regs.len() < reg::END {
            return Err(Refused::Registers);
        }
        let features = Features(regs.read64(reg::FEATURES));
        let was = regs.read64(reg::CONTROL);
        let found_on = was & control::ENABLE != 0;
        regs.write64(reg::CONTROL, 0);
        let pages = ((usize::from(setup.last_device) + 1) * DTE_BYTES).div_ceil(PAGE as usize);
        let table = memory.alloc_run(pages).ok_or(Refused::NoMemory)?;
        let mut rings = [0; 3];
        for at in 0..rings.len() {
            let Some(page) = memory.alloc_page() else {
                memory.free_run(table, pages);
                for &page in &rings[..at] {
                    memory.free_page(page);
                }
                return Err(Refused::NoMemory);
            };
            rings[at] = page;
        }
        let [base, done, events] = rings;
        let mut unit = Unit {
            regs,
            memory,
            features,
            setup,
            devices: (table, pages),
            commands: Commands { base, done, tail: 0, sequence: 0 },
            events,
            control: 0,
            found_on,
        };
        let stopped = |regs: &Mmio| {
            regs.read64(reg::STATUS) & (status::EVENT_LOG_RUNNING | status::COMMANDS_RUNNING) == 0
        };
        unit.wait(stopped, "stop its command buffer and event log")?;
        let interrupts = if setup.remap_interrupts { interrupt::VALID } else { 0 };
        for page in 0..pages {
            let entries = unit.memory.entries(table + page as u64 * PAGE);
            for at in (0..entries.len()).step_by(4) {
                entries[at].store(dte::BLOCKED, Ordering::Relaxed);
                entries[at + 2].store(interrupts, Ordering::Relaxed);
            }
        }
        device_barrier();
        unit.regs.write64(reg::DEVICE_TABLE, table | (pages as u64 - 1));
        unit.regs.write64(reg::COMMAND_BUFFER, base | RING_SIZE);
        unit.regs.write64(reg::COMMAND_HEAD, 0);
        unit.regs.write64(reg::COMMAND_TAIL, 0);
        unit.regs.write64(reg::EVENT_LOG, events | RING_SIZE);
        unit.regs.write64(reg::EVENT_HEAD, 0);
        unit.regs.write64(reg::EVENT_TAIL, 0);
        // A range firmware let devices reach untranslated goes.
        unit.regs.write64(reg::EXCLUSION_BASE, 0);
        unit.regs.write64(reg::EXCLUSION_LIMIT, 0);
        unit.regs.write64(
            reg::STATUS,
            status::EVENT_OVERFLOW | status::EVENT_INTERRUPT | status::COMPLETION_INTERRUPT,
        );
        let flag = |bit: u8, on: u64| if setup.flags & (1 << bit) != 0 { on } else { 0 };
        unit.control = control::ENABLE
            | control::COHERENT
            | control::TIMEOUT_1S
            | control::COMMAND_BUFFER
            | control::EVENT_LOG
            | flag(0, control::HT_TUNNEL)
            | flag(1, control::PASS_POSTED_WRITE)
            | flag(2, control::PASS_RESPONSE)
            | flag(3, control::ISOCHRONOUS);
        unit.regs.write64(reg::CONTROL, unit.control);
        unit.forget_everything()?;
        Ok(unit)
    }

    /// What it can do.
    pub fn features(&self) -> Features {
        self.features
    }

    /// The levels of its domains' page tables.
    pub fn levels(&self) -> u8 {
        LEVELS
    }

    /// Whether firmware or an earlier kernel had translation on.
    pub fn found_on(&self) -> bool {
        self.found_on
    }

    /// Whether translation is on.
    pub fn translating(&self) -> bool {
        self.regs.read64(reg::CONTROL) & control::ENABLE != 0
    }

    /// Whether it remaps interrupts: it was set up with
    /// [`Setup::remap_interrupts`].
    pub fn remapping(&self) -> bool {
        self.setup.remap_interrupts
    }

    /// The memory it was given.
    pub fn memory(&self) -> &M {
        &self.memory
    }

    /// The device IDs its device table reaches.
    pub fn devices(&self) -> u32 {
        (self.devices.1 * PAGE as usize / DTE_BYTES) as u32
    }

    /// A new, empty domain numbered `id` for its devices.
    pub fn domain(&self, id: u16) -> Option<Domain> {
        Domain::new(&self.memory, id, LEVELS, true)
    }

    /// Turns translation on again, after [`Unit::disable`].
    pub fn enable(&mut self) {
        self.control |= control::ENABLE;
        self.regs.write64(reg::CONTROL, self.control);
    }

    /// Turns translation off: every device reaches all memory again, and
    /// the IOMMU runs no commands until [`Unit::enable`].
    pub fn disable(&mut self) {
        self.control &= !control::ENABLE;
        self.regs.write64(reg::CONTROL, self.control);
    }

    /// Has the IOMMU signal its interrupt when it logs an event; the kernel
    /// sets the message up in its PCI function's MSI capability.
    pub fn interrupt_on_events(&mut self) {
        self.control |= control::EVENT_INTERRUPT;
        self.regs.write64(reg::CONTROL, self.control);
    }

    /// The first word of the device table entry of `device`, and its
    /// entries.
    fn entry(&self, device: u16) -> Option<(&[core::sync::atomic::AtomicU64; 512], usize)> {
        let offset = usize::from(device) * DTE_BYTES;
        if offset >= self.devices.1 * PAGE as usize {
            return None;
        }
        let page = self.memory.entries(self.devices.0 + (offset as u64 & !(PAGE - 1)));
        Some((page, (offset % PAGE as usize) / 8))
    }

    /// Has the device `device` reach what `domain` maps, and nothing
    /// else, in place of what it reached.
    pub fn attach(&mut self, device: u16, domain: &Domain) -> Result<(), Refused> {
        self.detach(device)?;
        let Some((entries, at)) = self.entry(device) else { return Err(Refused::Registers) };
        // The second word, with the domain, goes first: until the first
        // word changes, the entry still blocks.
        entries[at + 1].store(u64::from(domain.id()), Ordering::Relaxed);
        let first = dte::VALID
            | dte::TRANSLATION_VALID
            | (u64::from(domain.levels()) << 9)
            | domain.top()
            | dte::READ
            | dte::WRITE;
        entries[at].store(first, Ordering::Release);
        self.submit(&[
            [command::INVALIDATE_DEVICE | u64::from(device), 0],
            pages(domain.id(), EVERY_PAGE, true, true),
        ])
    }

    /// Has the device `device` reach nothing.
    pub fn detach(&mut self, device: u16) -> Result<(), Refused> {
        let Some((entries, at)) = self.entry(device) else { return Ok(()) };
        let was = entries[at].load(Ordering::Acquire);
        if was == dte::BLOCKED {
            return Ok(());
        }
        let domain = entries[at + 1].load(Ordering::Relaxed) as u16;
        entries[at].store(dte::BLOCKED, Ordering::Release);
        entries[at + 1].store(0, Ordering::Relaxed);
        self.submit(&[
            [command::INVALIDATE_DEVICE | u64::from(device), 0],
            pages(domain, EVERY_PAGE, true, true),
        ])
    }

    /// Has an interrupt the device `device` raises with data `vector` (its
    /// low 11 bits pick the entry) arrive as `vector` at the CPU whose APIC
    /// ID is `destination`, fixed and edge-triggered, in physical
    /// destination mode. The device's table is made the first time. The
    /// message the device writes is the one it would without remapping.
    pub fn remap(&mut self, device: u16, vector: u8, destination: u8) -> Result<(), Refused> {
        let entry = 1 | (u64::from(destination) << 8) | (u64::from(vector) << 16);
        let (table, made) = {
            let Some((entries, at)) = self.entry(device) else { return Err(Refused::Registers) };
            let word = entries[at + 2].load(Ordering::Acquire);
            if word & interrupt::CONTROL == interrupt::REMAPPED {
                (word & interrupt::TABLE, false)
            } else {
                let table = self.memory.alloc_page().ok_or(Refused::NoMemory)?;
                self.set_interrupt(table, vector, entry);
                let word = interrupt::VALID | interrupt::LENGTH | table | interrupt::REMAPPED;
                entries[at + 2].store(word, Ordering::Release);
                (table, true)
            }
        };
        let forget = [command::INVALIDATE_INTERRUPTS | u64::from(device), 0];
        if made {
            return self.submit(&[[command::INVALIDATE_DEVICE | u64::from(device), 0], forget]);
        }
        self.set_interrupt(table, vector, entry);
        self.submit(&[forget])
    }

    /// Takes away the interrupt entry `vector` of the device `device`: once
    /// this returns, an interrupt it raises with that data is blocked.
    pub fn unremap(&mut self, device: u16, vector: u8) -> Result<(), Refused> {
        let word = match self.entry(device) {
            Some((entries, at)) => entries[at + 2].load(Ordering::Acquire),
            None => return Ok(()),
        };
        if word & interrupt::CONTROL != interrupt::REMAPPED {
            return Ok(());
        }
        self.set_interrupt(word & interrupt::TABLE, vector, 0);
        self.submit(&[[command::INVALIDATE_INTERRUPTS | u64::from(device), 0]])
    }

    /// Writes `entry` as interrupt entry `vector` of the table at `table`.
    fn set_interrupt(&self, table: u64, vector: u8, entry: u64) {
        // Two 32-bit entries share each word.
        let word = &self.memory.entries(table)[usize::from(vector) / 2];
        let shift = u32::from(vector % 2) * 32;
        let was = word.load(Ordering::Relaxed);
        word.store((was & !(0xffff_ffff << shift)) | (entry << shift), Ordering::Release);
    }

    /// Has the IOMMU forget every device table entry and every page it
    /// cached, with the one command if it takes it, else entry by entry
    /// (the domains it caches are those firmware used, which the kernel's
    /// do not reuse unforgotten: [`Unit::attach`] forgets each).
    fn forget_everything(&mut self) -> Result<(), Refused> {
        if self.features.invalidate_all() {
            return self.submit(&[[command::INVALIDATE_ALL, 0]]);
        }
        let mut batch = [[0; 2]; BATCH];
        let devices = self.devices() as usize;
        for first in (0..devices).step_by(BATCH) {
            let count = BATCH.min(devices - first);
            for (n, slot) in batch[..count].iter_mut().enumerate() {
                *slot = [command::INVALIDATE_DEVICE | (first + n) as u64, 0];
            }
            self.submit(&batch[..count])?;
        }
        Ok(())
    }

    /// Puts `commands` in the command buffer, then a completion wait, and
    /// waits until the IOMMU reaches it.
    fn submit(&mut self, commands: &[[u64; 2]]) -> Result<(), Refused> {
        let entries = self.memory.entries(self.commands.base);
        let put = |tail: &mut u64, command: [u64; 2]| {
            let at = (*tail / 8) as usize;
            entries[at].store(command[0], Ordering::Relaxed);
            entries[at + 1].store(command[1], Ordering::Relaxed);
            *tail = (*tail + 16) % RING_BYTES;
        };
        for chunk in commands.chunks(BATCH) {
            for &command in chunk {
                put(&mut self.commands.tail, command);
            }
            self.commands.sequence = self.commands.sequence.wrapping_add(1).max(1);
            let sequence = self.commands.sequence;
            let done = &self.memory.entries(self.commands.done)[0];
            done.store(0, Ordering::Relaxed);
            // Store `sequence` at `done` once every command before is done.
            let wait = [command::COMPLETION_WAIT | self.commands.done | 1, sequence];
            put(&mut self.commands.tail, wait);
            device_barrier();
            self.regs.write64(reg::COMMAND_TAIL, self.commands.tail);
            let mut waited = 0;
            while done.load(Ordering::Acquire) != sequence {
                if waited >= WAIT_US {
                    return Err(Refused::Stuck("carry out its commands"));
                }
                self.memory.delay_us(LOOK_US);
                waited += LOOK_US;
            }
        }
        Ok(())
    }

    /// Waits up to a second until `done`.
    fn wait(&self, done: impl Fn(&Mmio) -> bool, what: &'static str) -> Result<(), Refused> {
        let mut waited = 0;
        while !done(&self.regs) {
            if waited >= WAIT_US {
                return Err(Refused::Stuck(what));
            }
            self.memory.delay_us(LOOK_US);
            waited += LOOK_US;
        }
        Ok(())
    }

    /// What reads its event log apart from the rest of it, for its
    /// interrupt: the registers it touches are its own, so it need not
    /// wait for a change to the tables.
    pub fn event_log(&self) -> EventLog<M> {
        EventLog { regs: self.regs.clone(), memory: self.memory.clone(), base: self.events }
    }
}

/// The command that invalidates the pages of `domain` that `address`
/// names: one page, or with `range` the block its low bits say (section
/// 2.4.3); `tables` if page directory entries changed too.
fn pages(domain: u16, address: u64, range: bool, tables: bool) -> [u64; 2] {
    [
        command::INVALIDATE_PAGES | (u64::from(domain) << 32),
        (address & !(PAGE - 1)) | (u64::from(tables) << 1) | u64::from(range),
    ]
}

impl<M: Tables> Invalidate for Unit<M> {
    type Error = Refused;

    fn forget_pages(
        &mut self,
        domain: u16,
        blocks: &[(u64, u8)],
        leaf: bool,
    ) -> Result<(), Refused> {
        let mut batch = [[0; 2]; BATCH];
        for chunk in blocks.chunks(BATCH) {
            for (slot, &(address, order)) in batch.iter_mut().zip(chunk) {
                // A block of 2 to the order pages is named by its address
                // with the bits below its size, but the lowest, set.
                *slot = match order {
                    0 => pages(domain, address, false, !leaf),
                    _ => {
                        let low = (1u64 << (11 + u32::from(order))) - PAGE;
                        pages(domain, address | low, true, !leaf)
                    }
                };
            }
            self.submit(&batch[..chunk.len()])?;
        }
        Ok(())
    }

    fn forget_domain(&mut self, domain: u16) -> Result<(), Refused> {
        self.submit(&[pages(domain, EVERY_PAGE, true, true)])
    }

    fn caches_absent(&self) -> bool {
        self.setup.caches_absent
    }

    fn mapped(&mut self) -> Result<(), Refused> {
        Ok(())
    }
}

/// An IOMMU's event log ([`Unit::event_log`]).
pub struct EventLog<M: Tables> {
    regs: Mmio,
    memory: M,
    base: u64,
}

impl<M: Tables> EventLog<M> {
    /// Calls `each` with every event logged since last asked, and takes
    /// them from the log; returns whether events were lost meanwhile, the
    /// log being full, in which case it starts the log again.
    pub fn read(&mut self, mut each: impl FnMut(Event)) -> bool {
        // Cleared first, so that an event logged from here on signals
        // again.
        self.regs.write64(reg::STATUS, status::EVENT_INTERRUPT);
        let entries = self.memory.entries(self.base);
        let mut head = self.regs.read64(reg::EVENT_HEAD) & RING_POINTER;
        let tail = self.regs.read64(reg::EVENT_TAIL) & RING_POINTER;
        while head != tail && head < RING_BYTES {
            let at = (head / 8) as usize;
            each(Event::decode(
                entries[at].load(Ordering::Acquire),
                entries[at + 1].load(Ordering::Acquire),
            ));
            head = (head + 16) % RING_BYTES;
        }
        self.regs.write64(reg::EVENT_HEAD, head);
        let lost = self.regs.read64(reg::STATUS) & status::EVENT_OVERFLOW != 0;
        if lost {
            // A full log stops; it starts again empty (section 3.3.2.2).
            let on = self.regs.read64(reg::CONTROL);
            self.regs.write64(reg::CONTROL, on & !control::EVENT_LOG);
            self.regs.write64(reg::EVENT_HEAD, 0);
            self.regs.write64(reg::EVENT_TAIL, 0);
            self.regs.write64(reg::STATUS, status::EVENT_OVERFLOW);
            self.regs.write64(reg::CONTROL, on);
        }
        lost
    }
}
