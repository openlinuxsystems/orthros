// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel VT-d DMA remapping (the *Intel Virtualization Technology for
//! Directed I/O Architecture Specification*, revision 4.1), for the units
//! the ACPI DMAR table describes.
//!
//! A remapping unit sits between a group of PCI devices and memory, and
//! translates each of their DMA requests by the *requester ID* it carries,
//! the device's bus, device and function: the root table's entry for the
//! bus points to a context table, whose entry for the device names the
//! device's *domain* and its page table, of the kind the CPU's are
//! (second-level translation, in legacy mode). A device whose context
//! entry is not present reaches no memory at all, and one that touches a
//! page its domain does not map is stopped, and the fault recorded.
//!
//! - [`Unit`] takes a unit over: checks what it can do, switches off what
//!   firmware left on, gives it a root table, turns its invalidation queue
//!   on where it has one, and then translation. It attaches domains to
//!   devices, invalidates what it caches after a change ([`Unit::flush`]),
//!   and reads and clears its fault records ([`Unit::faults`]).
//! - [`Domain`] is a device's page table, `radix-iopt`'s in the
//!   second-level format ([`SecondLevel`]), each page mapped at its own
//!   physical address. Its entries count their mappings, and its tables'
//!   entries the entries in use below, in bits 52 to 61, which the unit
//!   ignores. [`Unit`] invalidates for it ([`Invalidate`]).
//!
//! Interrupt remapping, the unit's other half, is not used yet: a device
//! may still send any interrupt message.
//!
//! The crate is a compartment: it has no `unsafe` code. It reaches a unit
//! through its [`Mmio`] registers and its tables through the [`Memory`]
//! the kernel gives it. Nothing here sleeps or takes a lock: the kernel
//! serializes each unit and each domain, and passes in how to wait. The
//! tests run it against a simulated unit that walks the tables as the
//! specification says hardware does.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
mod tests;

use core::fmt;
use core::sync::atomic::Ordering;

use radix_hw::{Mmio, device_barrier};
pub use radix_iopt::{Format, Invalidate, MapError, Memory, PAGE, blocks};

/// Second-level page table entries (VT-d 4.1, section 9.8): read and
/// write in bits 0 and 1, the counts of a domain in bits 52 to 61.
#[derive(Debug)]
pub struct SecondLevel;

impl Format for SecondLevel {
    const COUNT_MAX: u64 = 0x3ff;
    const COUNTS_TABLES: bool = true;

    fn table(child: u64, _level: u8) -> u64 {
        child | 3
    }

    fn page(address: u64) -> u64 {
        address | 3
    }

    fn present(entry: u64) -> bool {
        entry & 1 != 0
    }

    fn count(entry: u64) -> u64 {
        (entry >> 52) & Self::COUNT_MAX
    }

    fn with_count(entry: u64, count: u64) -> u64 {
        (entry & !(Self::COUNT_MAX << 52)) | (count << 52)
    }
}

/// A device's page table, in the second-level format.
pub type Domain = radix_iopt::Domain<SecondLevel>;

/// The registers (VT-d 4.1, chapter 11).
mod reg {
    pub const VERSION: usize = 0x00;
    pub const CAP: usize = 0x08;
    pub const ECAP: usize = 0x10;
    pub const GLOBAL_COMMAND: usize = 0x18;
    pub const GLOBAL_STATUS: usize = 0x1c;
    pub const ROOT_TABLE: usize = 0x20;
    pub const CONTEXT_COMMAND: usize = 0x28;
    pub const FAULT_STATUS: usize = 0x34;
    pub const FAULT_CONTROL: usize = 0x38;
    pub const FAULT_DATA: usize = 0x3c;
    pub const FAULT_ADDRESS: usize = 0x40;
    pub const FAULT_UPPER_ADDRESS: usize = 0x44;
    pub const PROTECTED_MEMORY: usize = 0x64;
    pub const QUEUE_TAIL: usize = 0x88;
    pub const QUEUE_ADDRESS: usize = 0x90;
    /// The registers the engine reaches lie below this, besides the
    /// fault recording and IOTLB registers, which the capabilities place.
    pub const FIXED_END: usize = 0x100;
}

/// Bits of the global command register, and of the status register at
/// the same places.
mod global {
    /// Translation is on.
    pub const TRANSLATION: u32 = 1 << 31;
    /// The root table's address is taken.
    pub const ROOT_TABLE: u32 = 1 << 30;
    /// The write buffer is flushed.
    pub const WRITE_BUFFER: u32 = 1 << 27;
    /// The invalidation queue is on.
    pub const QUEUE: u32 = 1 << 26;
    /// Interrupt remapping is on.
    pub const INTERRUPT_REMAPPING: u32 = 1 << 25;
}

/// Bits of the fault status register.
mod fault_status {
    /// A fault was lost: every fault recording register was full.
    pub const OVERFLOW: u32 = 1 << 0;
    /// A fault recording register holds a fault.
    pub const PENDING: u32 = 1 << 1;
    /// The invalidation queue stopped at a descriptor it refused.
    pub const QUEUE_ERROR: u32 = 1 << 4;
}

/// How long to wait for the unit to do what it was told.
const WAIT_US: u32 = 1_000_000;
/// How often to look meanwhile.
const LOOK_US: u32 = 1;

/// The entries of the invalidation queue: one page of 16-byte
/// descriptors.
const QUEUE_ENTRIES: usize = 256;

/// The physical address in a table entry.
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;
/// A root or context entry is present.
const PRESENT: u64 = 1;

/// What a unit can do: its capability and extended capability
/// registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// The capability register.
    pub cap: u64,
    /// The extended capability register.
    pub ecap: u64,
}

impl Caps {
    /// The unit's capabilities, read from its registers.
    pub fn read(regs: &Mmio) -> Caps {
        Caps { cap: regs.read64(reg::CAP), ecap: regs.read64(reg::ECAP) }
    }

    fn cap_bits(&self, shift: u32, width: u32) -> u64 {
        (self.cap >> shift) & ((1 << width) - 1)
    }

    fn ecap_bits(&self, shift: u32, width: u32) -> u64 {
        (self.ecap >> shift) & ((1 << width) - 1)
    }

    /// How many domains it tells apart.
    pub fn domains(&self) -> u32 {
        1 << (4 + 2 * self.cap_bits(0, 3).min(6))
    }

    /// Software must flush its write buffer after changing tables.
    pub fn write_buffer_flush(&self) -> bool {
        self.cap_bits(4, 1) != 0
    }

    /// It has protected memory regions, which firmware may have left on.
    pub fn protected_memory(&self) -> bool {
        self.cap_bits(5, 2) != 0
    }

    /// It caches entries that are not present (a unit a hypervisor
    /// emulates), so making one present must be invalidated too.
    pub fn caching_mode(&self) -> bool {
        self.cap_bits(7, 1) != 0
    }

    /// The levels of the page tables it walks that Radix uses: four (48
    /// bits of address) if it can, else three (39 bits).
    pub fn levels(&self) -> Option<u8> {
        let sagaw = self.cap_bits(8, 5);
        if sagaw & 0b100 != 0 {
            Some(4)
        } else if sagaw & 0b10 != 0 {
            Some(3)
        } else {
            None
        }
    }

    /// The widest address it translates, in bits.
    pub fn address_width(&self) -> u8 {
        self.cap_bits(16, 6) as u8 + 1
    }

    /// Where its fault recording registers start.
    pub fn fault_offset(&self) -> usize {
        self.cap_bits(24, 10) as usize * 16
    }

    /// It invalidates the IOTLB a range of pages at a time.
    pub fn page_selective(&self) -> bool {
        self.cap_bits(39, 1) != 0
    }

    /// How many fault recording registers it has.
    pub fn fault_records(&self) -> usize {
        self.cap_bits(40, 8) as usize + 1
    }

    /// The most pages a page-selective invalidation covers: 2 to this.
    pub fn max_order(&self) -> u8 {
        self.cap_bits(48, 6) as u8
    }

    /// It drains writes before invalidating the IOTLB, if asked.
    pub fn drain_writes(&self) -> bool {
        self.cap_bits(54, 1) != 0
    }

    /// It drains reads before invalidating the IOTLB, if asked.
    pub fn drain_reads(&self) -> bool {
        self.cap_bits(55, 1) != 0
    }

    /// Its table walks look in the CPU's caches.
    pub fn coherent(&self) -> bool {
        self.ecap_bits(0, 1) != 0
    }

    /// It has an invalidation queue.
    pub fn queued(&self) -> bool {
        self.ecap_bits(1, 1) != 0
    }

    /// It can remap interrupts.
    pub fn interrupt_remapping(&self) -> bool {
        self.ecap_bits(3, 1) != 0
    }

    /// Where its IOTLB registers are.
    pub fn iotlb_offset(&self) -> usize {
        self.ecap_bits(8, 10) as usize * 16
    }

    /// How many bytes its registers take, in whole pages.
    pub fn register_bytes(&self) -> usize {
        let faults = self.fault_offset() + 16 * self.fault_records();
        let iotlb = self.iotlb_offset() + 16;
        faults.max(iotlb).max(reg::FIXED_END).next_multiple_of(PAGE as usize)
    }
}

/// Why a unit could not be taken over, or stopped working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Its registers reach past the window mapped for them.
    Registers,
    /// It walks neither 3- nor 4-level tables.
    NoTables,
    /// Memory for its tables ran out.
    NoMemory,
    /// It did not do what it was told within a second.
    Stuck(&'static str),
    /// Its invalidation queue refused a descriptor, and stopped.
    QueueError,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Registers => f.write_str("its registers reach past what was mapped"),
            Refused::NoTables => f.write_str("it walks neither 3- nor 4-level page tables"),
            Refused::NoMemory => f.write_str("out of memory for its tables"),
            Refused::Stuck(what) => write!(f, "it did not {what} within a second"),
            Refused::QueueError => f.write_str("its invalidation queue refused a descriptor"),
        }
    }
}

/// What to invalidate of what a unit caches, after its tables changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flush {
    /// Every context entry.
    Contexts,
    /// The context entry of the device `source` (its requester ID), of
    /// domain `domain`.
    Context {
        /// The device's requester ID: bus, then device and function.
        source: u16,
        /// The domain its entry named.
        domain: u16,
    },
    /// Every translation.
    All,
    /// Every translation of a domain.
    Domain(u16),
    /// The translations of 2 to the `order` pages from `address`, which
    /// is aligned to as many, of domain `domain`; `leaf` if no table was
    /// taken away, only page entries.
    Pages {
        /// The domain.
        domain: u16,
        /// The first page's address.
        address: u64,
        /// How many pages: 2 to this.
        order: u8,
        /// Only page entries changed.
        leaf: bool,
    },
}

/// A fault the unit recorded: a device's request it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    /// The device's requester ID.
    pub source: u16,
    /// The page it tried to reach.
    pub address: u64,
    /// It tried to write, not read.
    pub write: bool,
    /// The unit's reason for the fault.
    pub reason: u8,
}

impl Fault {
    /// The device's bus.
    pub fn bus(&self) -> u8 {
        (self.source >> 8) as u8
    }

    /// The device's number on its bus.
    pub fn device(&self) -> u8 {
        (self.source >> 3) as u8 & 0x1f
    }

    /// Its function.
    pub fn function(&self) -> u8 {
        self.source as u8 & 7
    }

    /// What the reason means (VT-d 4.1, appendix A).
    pub fn why(&self) -> &'static str {
        match self.reason {
            0x01 => "its bus has no root entry",
            0x02 => "it has no context entry",
            0x03 => "its context entry is invalid",
            0x04 => "the address is past its domain's width",
            0x05 => "the page is not mapped for it to write",
            0x06 => "the page is not mapped for it to read",
            0x07 => "a page table's address is past the unit's width",
            0x08 => "the root table's address is past the unit's width",
            0x09 => "its context table's address is past the unit's width",
            0x0a => "its root entry has a reserved bit set",
            0x0b => "its context entry has a reserved bit set",
            0x0c => "a page table entry has a reserved bit set",
            0x0d => "its context entry does not let requests of that kind through",
            _ => "a fault of another kind",
        }
    }
}

/// A unit's invalidation queue.
struct Queue {
    /// The page of descriptors.
    base: u64,
    /// The page whose first word the unit writes when a wait descriptor
    /// is reached.
    status: u64,
    /// Where the next descriptor goes.
    tail: usize,
    /// The number the last wait descriptor writes.
    sequence: u32,
}

/// A remapping unit, taken over.
pub struct Unit<M: Memory> {
    regs: Mmio,
    memory: M,
    caps: Caps,
    version: u32,
    levels: u8,
    /// The command register's bits that stay set.
    command: u32,
    root: u64,
    queue: Option<Queue>,
    found_on: bool,
}

impl<M: Memory> Unit<M> {
    /// Takes over the unit whose registers are `regs`: switches off what
    /// firmware or an earlier kernel left on (translation, interrupt
    /// remapping, the invalidation queue), gives it an empty root table,
    /// turns its invalidation queue on if it has one, and invalidates
    /// what it caches. Translation stays off until [`Unit::enable`]; the
    /// root table makes every device reach nothing once it is on.
    pub fn new(regs: Mmio, memory: M) -> Result<Unit<M>, Refused> {
        let caps = Caps::read(&regs);
        if regs.len() < caps.register_bytes() {
            return Err(Refused::Registers);
        }
        let levels = caps.levels().ok_or(Refused::NoTables)?;
        let version = regs.read32(reg::VERSION);
        let status = regs.read32(reg::GLOBAL_STATUS);
        let on = global::TRANSLATION | global::INTERRUPT_REMAPPING | global::QUEUE;
        let mut unit = Unit {
            regs,
            memory,
            caps,
            version,
            levels,
            command: status & on,
            root: 0,
            queue: None,
            found_on: status & global::TRANSLATION != 0,
        };
        for bit in [global::TRANSLATION, global::INTERRUPT_REMAPPING, global::QUEUE] {
            if status & bit != 0 {
                unit.set(bit, false)?;
            }
        }
        unit.root = unit.memory.alloc_page().ok_or(Refused::NoMemory)?;
        unit.regs.write64(reg::ROOT_TABLE, unit.root);
        unit.once(global::ROOT_TABLE, true, "take its root table")?;
        if caps.queued() {
            let base = unit.memory.alloc_page().ok_or(Refused::NoMemory)?;
            let Some(status) = unit.memory.alloc_page() else {
                unit.memory.free_page(base);
                return Err(Refused::NoMemory);
            };
            unit.regs.write64(reg::QUEUE_TAIL, 0);
            // 256 descriptors of 128 bits: a page.
            unit.regs.write64(reg::QUEUE_ADDRESS, base);
            unit.set(global::QUEUE, true)?;
            unit.queue = Some(Queue { base, status, tail: 0, sequence: 0 });
        }
        unit.flush(&[Flush::Contexts, Flush::All])?;
        Ok(unit)
    }

    /// What it can do.
    pub fn caps(&self) -> Caps {
        self.caps
    }

    /// Its version: major, minor.
    pub fn version(&self) -> (u8, u8) {
        ((self.version >> 4) as u8 & 0xf, self.version as u8 & 0xf)
    }

    /// The levels of its domains' page tables.
    pub fn levels(&self) -> u8 {
        self.levels
    }

    /// Whether firmware or an earlier kernel had translation on.
    pub fn found_on(&self) -> bool {
        self.found_on
    }

    /// Whether it invalidates through its queue.
    pub fn queued(&self) -> bool {
        self.queue.is_some()
    }

    /// Whether translation is on.
    pub fn translating(&self) -> bool {
        self.regs.read32(reg::GLOBAL_STATUS) & global::TRANSLATION != 0
    }

    /// The memory it was given.
    pub fn memory(&self) -> &M {
        &self.memory
    }

    /// A new, empty domain numbered `id` for its devices, with its tables
    /// written back to memory as the unit needs them.
    pub fn domain(&self, id: u16) -> Option<Domain> {
        Domain::new(&self.memory, id, self.levels, self.caps.coherent())
    }

    /// Turns translation on, then switches off the protected memory
    /// regions firmware may have left on, which translation now covers.
    pub fn enable(&mut self) -> Result<(), Refused> {
        self.flush_write_buffer()?;
        self.set(global::TRANSLATION, true)?;
        if self.caps.protected_memory() {
            let enabled = self.regs.read32(reg::PROTECTED_MEMORY);
            if enabled & (1 << 31) != 0 {
                self.regs.write32(reg::PROTECTED_MEMORY, enabled & !(1 << 31));
                self.wait(|regs| regs.read32(reg::PROTECTED_MEMORY) & 1 == 0, "unprotect memory")?;
            }
        }
        Ok(())
    }

    /// Turns translation off: every device reaches all memory again.
    pub fn disable(&mut self) -> Result<(), Refused> {
        self.set(global::TRANSLATION, false)
    }

    /// Has the unit raise `data` at `address` when it records a fault.
    pub fn set_fault_interrupt(&mut self, address: u64, data: u32) {
        self.regs.write32(reg::FAULT_DATA, data);
        self.regs.write32(reg::FAULT_ADDRESS, address as u32);
        self.regs.write32(reg::FAULT_UPPER_ADDRESS, (address >> 32) as u32);
        self.regs.write32(reg::FAULT_CONTROL, 0);
    }

    /// Has the device at `bus` and `devfn` (device and function) reach
    /// what `domain` maps, and nothing else, in place of what it reached.
    pub fn attach(&mut self, bus: u8, devfn: u8, domain: &Domain) -> Result<(), Refused> {
        let (context, at) = self.context_entry(bus, devfn, true)?;
        let source = (u16::from(bus) << 8) | u16::from(devfn);
        if let Some(was) = self.clear_context(context, at) {
            // A present entry is never changed in place: it goes, the unit
            // forgets it, and the new one comes.
            self.flush(&[Flush::Context { source, domain: was }, Flush::Domain(was)])?;
        }
        let entries = self.memory.entries(context);
        let width = u64::from(domain.levels() - 2);
        entries[at + 1].store(width | (u64::from(domain.id()) << 8), Ordering::Relaxed);
        entries[at].store(domain.top() | PRESENT, Ordering::Release);
        self.memory.flush(&entries[at]);
        self.flush(&[Flush::Context { source, domain: domain.id() }, Flush::Domain(domain.id())])
    }

    /// Has the device at `bus` and `devfn` reach nothing.
    pub fn detach(&mut self, bus: u8, devfn: u8) -> Result<(), Refused> {
        let Ok((context, at)) = self.context_entry(bus, devfn, false) else { return Ok(()) };
        let Some(was) = self.clear_context(context, at) else { return Ok(()) };
        let source = (u16::from(bus) << 8) | u16::from(devfn);
        self.flush(&[Flush::Context { source, domain: was }, Flush::Domain(was)])
    }

    /// Clears the context entry at `at` of the table `context`, if it is
    /// present; returns the domain it named.
    fn clear_context(&self, context: u64, at: usize) -> Option<u16> {
        let entries = self.memory.entries(context);
        if entries[at].load(Ordering::Acquire) & PRESENT == 0 {
            return None;
        }
        let was = (entries[at + 1].load(Ordering::Relaxed) >> 8) as u16;
        entries[at].store(0, Ordering::Release);
        self.memory.flush(&entries[at]);
        Some(was)
    }

    /// The context table of `bus`, made if `make` and there is none, and
    /// the index of the first word of the entry of `devfn` in it.
    fn context_entry(&self, bus: u8, devfn: u8, make: bool) -> Result<(u64, usize), Refused> {
        let roots = self.memory.entries(self.root);
        let at = usize::from(bus) * 2;
        let root = roots[at].load(Ordering::Acquire);
        let table = if root & PRESENT != 0 {
            root & ADDRESS
        } else if make {
            let table = self.memory.alloc_page().ok_or(Refused::NoMemory)?;
            roots[at].store(table | PRESENT, Ordering::Release);
            self.memory.flush(&roots[at]);
            table
        } else {
            return Err(Refused::NoMemory);
        };
        Ok((table, usize::from(devfn) * 2))
    }

    /// Invalidates what `flushes` say of what the unit caches, in order,
    /// and waits until it has.
    pub fn flush(&mut self, flushes: &[Flush]) -> Result<(), Refused> {
        self.flush_write_buffer()?;
        if self.queue.is_some() {
            let mut batch = [[0u64; 2]; 32];
            let mut count = 0;
            for &flush in flushes {
                batch[count] = self.descriptor(flush);
                count += 1;
                if count == batch.len() {
                    self.submit(&batch[..count])?;
                    count = 0;
                }
            }
            if count > 0 {
                self.submit(&batch[..count])?;
            }
            return Ok(());
        }
        for &flush in flushes {
            self.flush_by_register(flush)?;
        }
        Ok(())
    }

    /// What a page-selective flush becomes on this unit: itself, or a
    /// domain's if the unit cannot take it.
    fn fit(&self, flush: Flush) -> Flush {
        match flush {
            Flush::Pages { domain, order, .. }
                if !self.caps.page_selective() || order > self.caps.max_order() =>
            {
                Flush::Domain(domain)
            }
            other => other,
        }
    }

    /// The bits an IOTLB invalidation drains with.
    fn drains(&self) -> (u64, u64) {
        (u64::from(self.caps.drain_reads()), u64::from(self.caps.drain_writes()))
    }

    /// The invalidation descriptor of `flush` (VT-d 4.1, section 6.5.2).
    fn descriptor(&self, flush: Flush) -> [u64; 2] {
        let (reads, writes) = self.drains();
        let iotlb = |granularity: u64, domain: u16| {
            2 | (granularity << 4) | (writes << 6) | (reads << 7) | (u64::from(domain) << 16)
        };
        match self.fit(flush) {
            Flush::Contexts => [1 | (1 << 4), 0],
            Flush::Context { source, domain } => {
                [1 | (3 << 4) | (u64::from(domain) << 16) | (u64::from(source) << 32), 0]
            }
            Flush::All => [iotlb(1, 0), 0],
            Flush::Domain(domain) => [iotlb(2, domain), 0],
            Flush::Pages { domain, address, order, leaf } => {
                [iotlb(3, domain), address | (u64::from(leaf) << 6) | u64::from(order)]
            }
        }
    }

    /// Puts `descriptors` in the queue, then a wait descriptor, and waits
    /// until the unit reaches it.
    fn submit(&mut self, descriptors: &[[u64; 2]]) -> Result<(), Refused> {
        let Some(queue) = self.queue.as_mut() else { return Ok(()) };
        let entries = self.memory.entries(queue.base);
        let put = |tail: &mut usize, descriptor: [u64; 2]| {
            entries[*tail * 2].store(descriptor[0], Ordering::Relaxed);
            entries[*tail * 2 + 1].store(descriptor[1], Ordering::Relaxed);
            if !self.caps.coherent() {
                self.memory.flush(&entries[*tail * 2]);
            }
            *tail = (*tail + 1) % QUEUE_ENTRIES;
        };
        for &descriptor in descriptors {
            put(&mut queue.tail, descriptor);
        }
        queue.sequence = queue.sequence.wrapping_add(1).max(1);
        let sequence = queue.sequence;
        let status = self.memory.entries(queue.status);
        status[0].store(0, Ordering::Relaxed);
        // Status write, with the sequence number as its data.
        put(&mut queue.tail, [5 | (1 << 5) | (u64::from(sequence) << 32), queue.status]);
        device_barrier();
        self.regs.write64(reg::QUEUE_TAIL, (queue.tail as u64) << 4);
        let mut waited = 0;
        loop {
            if status[0].load(Ordering::Acquire) as u32 == sequence {
                return Ok(());
            }
            if self.regs.read32(reg::FAULT_STATUS) & fault_status::QUEUE_ERROR != 0 {
                self.regs.write32(reg::FAULT_STATUS, fault_status::QUEUE_ERROR);
                return Err(Refused::QueueError);
            }
            if waited >= WAIT_US {
                return Err(Refused::Stuck("invalidate what it caches"));
            }
            self.memory.delay_us(LOOK_US);
            waited += LOOK_US;
        }
    }

    /// Invalidates `flush` through the registers, for a unit without a
    /// queue (VT-d 4.1, sections 6.5.1 and 11.4.4).
    fn flush_by_register(&mut self, flush: Flush) -> Result<(), Refused> {
        let (reads, writes) = self.drains();
        let iotlb = self.caps.iotlb_offset();
        let (granularity, domain, address) = match self.fit(flush) {
            Flush::Contexts | Flush::Context { .. } => {
                let (granularity, source, domain) = match flush {
                    Flush::Context { source, domain } => (3, source, domain),
                    _ => (1, 0, 0),
                };
                let command =
                    (1 << 63) | (granularity << 61) | (u64::from(source) << 16) | u64::from(domain);
                self.regs.write64(reg::CONTEXT_COMMAND, command);
                let done = |regs: &Mmio| regs.read64(reg::CONTEXT_COMMAND) & (1 << 63) == 0;
                return self.wait(done, "invalidate its context cache");
            }
            Flush::All => (1, 0, None),
            Flush::Domain(domain) => (2, domain, None),
            Flush::Pages { domain, address, order, leaf } => {
                (3, domain, Some(address | (u64::from(leaf) << 6) | u64::from(order)))
            }
        };
        if let Some(address) = address {
            self.regs.write64(iotlb, address);
        }
        let command = (1 << 63)
            | (granularity << 60)
            | (reads << 49)
            | (writes << 48)
            | (u64::from(domain) << 32);
        self.regs.write64(iotlb + 8, command);
        self.wait(|regs| regs.read64(iotlb + 8) & (1 << 63) == 0, "invalidate its IOTLB")
    }

    /// Flushes the write buffer, on a unit that needs it.
    fn flush_write_buffer(&mut self) -> Result<(), Refused> {
        if self.caps.write_buffer_flush() {
            return self.once(global::WRITE_BUFFER, false, "flush its write buffer");
        }
        Ok(())
    }

    /// Sets or clears a command bit that stays (translation, the queue,
    /// interrupt remapping), and waits for its status to follow.
    fn set(&mut self, bit: u32, on: bool) -> Result<(), Refused> {
        if on {
            self.command |= bit;
        } else {
            self.command &= !bit;
        }
        self.regs.write32(reg::GLOBAL_COMMAND, self.command);
        let what = match bit {
            global::TRANSLATION if on => "turn translation on",
            global::TRANSLATION => "turn translation off",
            global::QUEUE if on => "turn its invalidation queue on",
            global::QUEUE => "turn its invalidation queue off",
            _ => "turn interrupt remapping off",
        };
        self.wait(|regs| (regs.read32(reg::GLOBAL_STATUS) & bit != 0) == on, what)
    }

    /// Gives a one-time command, and waits until its status bit is `done`.
    fn once(&mut self, bit: u32, done: bool, what: &'static str) -> Result<(), Refused> {
        self.regs.write32(reg::GLOBAL_COMMAND, self.command | bit);
        self.wait(|regs| (regs.read32(reg::GLOBAL_STATUS) & bit != 0) == done, what)
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

    /// Calls `each` with every fault recorded since last asked, and clears
    /// them; returns whether faults were lost meanwhile, every recording
    /// register being full.
    pub fn faults(&mut self, each: impl FnMut(Fault)) -> bool {
        self.fault_records().read(each)
    }

    /// What reads its fault records apart from the rest of it, for its
    /// fault interrupt: the registers it touches are its own, so it need
    /// not wait for a change to the tables.
    pub fn fault_records(&self) -> FaultRecords {
        FaultRecords { regs: self.regs.clone(), caps: self.caps }
    }
}

impl<M: Memory> Invalidate for Unit<M> {
    type Error = Refused;

    fn forget_pages(
        &mut self,
        domain: u16,
        blocks: &[(u64, u8)],
        leaf: bool,
    ) -> Result<(), Refused> {
        let mut batch = [Flush::All; 32];
        for chunk in blocks.chunks(batch.len()) {
            for (flush, &(address, order)) in batch.iter_mut().zip(chunk) {
                *flush = Flush::Pages { domain, address, order, leaf };
            }
            self.flush(&batch[..chunk.len()])?;
        }
        Ok(())
    }

    fn forget_domain(&mut self, domain: u16) -> Result<(), Refused> {
        self.flush(&[Flush::Domain(domain)])
    }

    fn caches_absent(&self) -> bool {
        // Such a unit caches what is not mapped too (VT-d 4.1, 6.1).
        self.caps.caching_mode()
    }

    fn mapped(&mut self) -> Result<(), Refused> {
        // The write buffer, on a unit that has one.
        self.flush(&[])
    }
}

/// A unit's fault recording registers ([`Unit::fault_records`]).
pub struct FaultRecords {
    regs: Mmio,
    caps: Caps,
}

impl FaultRecords {
    /// Calls `each` with every fault recorded since last asked, and clears
    /// them; returns whether faults were lost meanwhile, every recording
    /// register being full.
    pub fn read(&mut self, mut each: impl FnMut(Fault)) -> bool {
        let status = self.regs.read32(reg::FAULT_STATUS);
        if status & fault_status::PENDING != 0 {
            let records = self.caps.fault_records();
            let mut at = ((status >> 8) & 0xff) as usize % records;
            for _ in 0..records {
                let offset = self.caps.fault_offset() + at * 16;
                let high = self.regs.read64(offset + 8);
                if high & (1 << 63) == 0 {
                    break;
                }
                let low = self.regs.read64(offset);
                each(Fault {
                    source: high as u16,
                    address: low & !(PAGE - 1),
                    write: high & (1 << 62) == 0,
                    reason: (high >> 32) as u8,
                });
                // Writing the fault bit clears the record.
                self.regs.write32(offset + 12, 1 << 31);
                at = (at + 1) % records;
            }
        }
        let lost = status & fault_status::OVERFLOW != 0;
        if lost {
            self.regs.write32(reg::FAULT_STATUS, fault_status::OVERFLOW);
        }
        lost
    }
}
