// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The engine against a simulated unit: its registers as VT-d 4.1 has
//! them, an invalidation queue, fault recording, an IOTLB that keeps what
//! it translated until told to forget it, and table walks done from the
//! specification rather than from the engine's code.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use proptest::prelude::*;
use radix_hw::{Mmio, Registers};

use crate::{Caps, Domain, Flush, MapError, Memory, PAGE, Refused, Unit, blocks};

/// A page of table entries, aligned as the unit needs.
#[repr(C, align(4096))]
struct Table([AtomicU64; 512]);

/// Host memory for tables: each page leaked, found again by the address
/// it stands at.
#[derive(Default)]
struct HostMemory {
    pages: Mutex<HashMap<u64, &'static Table>>,
    /// Allocations allowed before they fail.
    allowed: AtomicUsize,
    flushes: AtomicUsize,
}

impl HostMemory {
    fn new() -> Arc<HostMemory> {
        let memory = HostMemory::default();
        memory.allowed.store(usize::MAX, Ordering::Relaxed);
        Arc::new(memory)
    }

    fn live(&self) -> usize {
        self.pages.lock().unwrap().len()
    }

    fn table(&self, phys: u64) -> &'static Table {
        self.pages.lock().unwrap().get(&phys).copied().expect("a table the engine made")
    }

    fn read(&self, phys: u64) -> u64 {
        self.table(phys & !(PAGE - 1)).0[(phys as usize & 0xfff) / 8].load(Ordering::Acquire)
    }
}

impl Memory for Arc<HostMemory> {
    fn alloc_page(&self) -> Option<u64> {
        let allowed = self.allowed.load(Ordering::Relaxed);
        if allowed == 0 {
            return None;
        }
        if allowed != usize::MAX {
            self.allowed.store(allowed - 1, Ordering::Relaxed);
        }
        let table: &'static Table = Box::leak(Box::new(Table([const { AtomicU64::new(0) }; 512])));
        let phys = core::ptr::from_ref(table) as u64;
        self.pages.lock().unwrap().insert(phys, table);
        Some(phys)
    }

    fn free_page(&self, phys: u64) {
        assert!(self.pages.lock().unwrap().remove(&phys).is_some(), "a page freed twice");
    }

    fn entries(&self, phys: u64) -> &[AtomicU64; 512] {
        &self.table(phys).0
    }

    fn flush(&self, _entry: &AtomicU64) {
        self.flushes.fetch_add(1, Ordering::Relaxed);
    }

    fn delay_us(&self, _us: u32) {}
}

/// What the simulated unit was told to invalidate, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Told {
    Context { granularity: u64, source: u16, domain: u16, queued: bool },
    Iotlb { granularity: u64, domain: u16, address: u64, order: u8, queued: bool },
}

#[derive(Default)]
struct State {
    status: u32,
    root_register: u64,
    root: Option<u64>,
    context_command: u64,
    iotlb_address: u64,
    iotlb_command: u64,
    queue_address: u64,
    queue_head: u64,
    fault_status: u32,
    fault_control: u32,
    fault_message: (u32, u32, u32),
    protected: u32,
    /// The fault recording registers: low and high words.
    records: Vec<(u64, u64)>,
    next_record: usize,
    told: Vec<Told>,
    /// What the IOTLB keeps: (domain, page) to page.
    iotlb: BTreeMap<(u16, u64), u64>,
    /// Commands it does not carry out.
    deaf: u32,
    /// Refuse every queued descriptor.
    refuse: bool,
}

/// A simulated remapping unit.
struct FakeUnit {
    caps: Caps,
    memory: Arc<HostMemory>,
    state: Mutex<State>,
}

const FAULT_OFFSET: usize = 0x200;
const IOTLB_OFFSET: usize = 0x100;
const RECORDS: usize = 4;

/// Capabilities: 256 domains, 39- and 48-bit tables, page-selective
/// invalidation up to 2^9 pages, four fault records.
fn cap(levels: &[u8]) -> u64 {
    let sagaw = levels.iter().map(|&l| if l == 3 { 0b10 } else { 0b100 }).fold(0, |a, b| a | b);
    2 | (sagaw << 8)
        | (47 << 16)
        | (((FAULT_OFFSET / 16) as u64) << 24)
        | (1 << 39)
        | (((RECORDS - 1) as u64) << 40)
        | (9 << 48)
        | (3 << 54)
        | (3 << 5)
}

/// Extended capabilities: coherent if asked, queued if asked, interrupt
/// remapping.
fn ecap(coherent: bool, queued: bool) -> u64 {
    u64::from(coherent) | (u64::from(queued) << 1) | (1 << 3) | (((IOTLB_OFFSET / 16) as u64) << 8)
}

impl FakeUnit {
    fn new(memory: &Arc<HostMemory>, caps: Caps) -> Arc<FakeUnit> {
        let state = State { records: vec![(0, 0); RECORDS], ..State::default() };
        Arc::new(FakeUnit { caps, memory: memory.clone(), state: Mutex::new(state) })
    }

    fn mmio(self: &Arc<FakeUnit>) -> Mmio {
        Mmio::fake(self.clone(), self.caps.register_bytes())
    }

    /// Translates a DMA request of the device `source` to `address`, as
    /// the unit does: through its IOTLB, else by walking the tables, and
    /// records a fault if it is stopped.
    fn dma(&self, source: u16, address: u64, write: bool) -> Result<u64, u8> {
        let mut state = self.state.lock().unwrap();
        let result = self.translate(&mut state, source, address, write);
        if let Err(reason) = result {
            let at = state.next_record;
            if state.records[at].1 & (1 << 63) == 0 {
                let kind = if write { 0 } else { 1 << 62 };
                state.records[at] = (
                    address & !(PAGE - 1),
                    (1 << 63) | kind | (u64::from(reason) << 32) | u64::from(source),
                );
                if state.fault_status & 2 == 0 {
                    state.fault_status = (state.fault_status & !0xff00) | ((at as u32) << 8);
                }
                state.fault_status |= 2;
                state.next_record = (at + 1) % RECORDS;
            } else {
                state.fault_status |= 1;
            }
        }
        result
    }

    fn translate(
        &self,
        state: &mut State,
        source: u16,
        address: u64,
        write: bool,
    ) -> Result<u64, u8> {
        if state.status & (1 << 31) == 0 {
            return Ok(address);
        }
        let root = state.root.expect("translation with a root table");
        let entry = self.memory.read(root + u64::from(source >> 8) * 16);
        if entry & 1 == 0 {
            return Err(0x01);
        }
        let context = entry & 0x000f_ffff_ffff_f000;
        let low = self.memory.read(context + u64::from(source & 0xff) * 16);
        let high = self.memory.read(context + u64::from(source & 0xff) * 16 + 8);
        if low & 1 == 0 {
            return Err(0x02);
        }
        if (low >> 2) & 3 != 0 {
            return Err(0x03);
        }
        let levels = match high & 7 {
            1 => 3,
            2 => 4,
            _ => return Err(0x03),
        };
        let domain = (high >> 8) as u16;
        if let Some(&page) = state.iotlb.get(&(domain, address & !(PAGE - 1))) {
            return Ok(page | (address & (PAGE - 1)));
        }
        if address >> (12 + 9 * levels) != 0 {
            return Err(0x04);
        }
        let mut table = low & 0x000f_ffff_ffff_f000;
        for level in (1..=levels).rev() {
            let at = (address >> (12 + 9 * (level - 1))) & 0x1ff;
            let entry = self.memory.read(table + at * 8);
            if entry & (3 << 62) != 0 || (level > 1 && entry & (1 << 7) != 0) {
                return Err(0x0c);
            }
            let allowed = if write { entry & 2 != 0 } else { entry & 1 != 0 };
            if !allowed {
                return Err(if write { 0x05 } else { 0x06 });
            }
            table = entry & 0x000f_ffff_ffff_f000;
        }
        state.iotlb.insert((domain, address & !(PAGE - 1)), table);
        Ok(table | (address & (PAGE - 1)))
    }

    fn told(&self) -> Vec<Told> {
        core::mem::take(&mut self.state.lock().unwrap().told)
    }

    /// Carries out an IOTLB invalidation.
    fn forget(state: &mut State, told: Told) {
        state.told.push(told);
        match told {
            Told::Iotlb { granularity: 1, .. } => state.iotlb.clear(),
            Told::Iotlb { granularity: 2, domain, .. } => state.iotlb.retain(|k, _| k.0 != domain),
            Told::Iotlb { granularity: 3, domain, address, order, .. } => {
                let end = address + (PAGE << order);
                state.iotlb.retain(|k, _| k.0 != domain || !(address..end).contains(&k.1));
            }
            _ => {}
        }
    }

    /// Runs the queue's descriptors up to `tail`.
    fn run_queue(&self, state: &mut State, tail: u64) {
        while state.queue_head != tail {
            let at = state.queue_address + state.queue_head;
            let (low, high) = (self.memory.read(at), self.memory.read(at + 8));
            if state.refuse {
                state.fault_status |= 1 << 4;
                return;
            }
            match low & 0xf {
                1 => state.told.push(Told::Context {
                    granularity: (low >> 4) & 3,
                    source: (low >> 32) as u16,
                    domain: (low >> 16) as u16,
                    queued: true,
                }),
                2 => Self::forget(
                    state,
                    Told::Iotlb {
                        granularity: (low >> 4) & 3,
                        domain: (low >> 16) as u16,
                        address: high & !(PAGE - 1),
                        order: (high & 0x3f) as u8,
                        queued: true,
                    },
                ),
                5 => {
                    assert!(low & (1 << 5) != 0, "a wait descriptor writes its status");
                    let address = high & !3;
                    let table = self.memory.table(address & !(PAGE - 1));
                    let word = &table.0[(address as usize & 0xfff) / 8];
                    let old = word.load(Ordering::Relaxed);
                    word.store((old & !0xffff_ffff) | (low >> 32), Ordering::Release);
                }
                _ => {
                    state.fault_status |= 1 << 4;
                    return;
                }
            }
            state.queue_head = (state.queue_head + 16) % (256 * 16);
        }
    }
}

impl Registers for FakeUnit {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.state.lock().unwrap();
        let fault = offset.checked_sub(FAULT_OFFSET).filter(|&o| o < RECORDS * 16);
        if let Some(at) = fault {
            let (low, high) = state.records[at / 16];
            return match (at % 16, size) {
                (0, 8) => low,
                (8, 8) => high,
                (12, 4) => high >> 32,
                _ => panic!("fault record read at {at} of {size}"),
            };
        }
        match offset {
            0x00 => 0x10,
            0x08 => self.caps.cap,
            0x10 => self.caps.ecap,
            0x1c => u64::from(state.status),
            0x20 => state.root_register,
            0x28 => state.context_command,
            0x34 => u64::from(state.fault_status),
            0x38 => u64::from(state.fault_control),
            0x64 => u64::from(state.protected),
            0x80 => state.queue_head,
            0x90 => state.queue_address,
            IOTLB_OFFSET => state.iotlb_address,
            o if o == IOTLB_OFFSET + 8 => state.iotlb_command,
            _ => 0,
        }
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.state.lock().unwrap();
        let queued = state.status & (1 << 26) != 0;
        let fault = offset.checked_sub(FAULT_OFFSET).filter(|&o| o < RECORDS * 16);
        if let Some(at) = fault {
            assert_eq!((at % 16, size), (12, 4), "fault records are cleared by their top word");
            if value & (1 << 31) != 0 {
                state.records[at / 16].1 &= !(1 << 63);
            }
            if state.records.iter().all(|r| r.1 & (1 << 63) == 0) {
                state.fault_status &= !2;
            }
            return;
        }
        match offset {
            0x18 => {
                let command = value as u32 & !state.deaf;
                // The bits that stay follow the command; the others act once.
                for bit in [31, 26, 25] {
                    let mask = 1 << bit;
                    if command & mask != 0 {
                        if bit == 31 {
                            assert!(state.root.is_some(), "translation on without a root table");
                        }
                        if bit == 26 && state.status & mask == 0 {
                            state.queue_head = 0;
                        }
                        state.status |= mask;
                    } else if value as u32 & mask == 0 {
                        state.status &= !mask;
                    }
                }
                if command & (1 << 30) != 0 {
                    state.root = Some(state.root_register & !0xfff);
                    state.status |= 1 << 30;
                }
            }
            0x20 => state.root_register = value,
            0x28 => {
                assert!(!queued, "a register invalidation while the queue is on");
                if value & (1 << 63) != 0 {
                    state.told.push(Told::Context {
                        granularity: (value >> 61) & 3,
                        source: (value >> 16) as u16,
                        domain: value as u16,
                        queued: false,
                    });
                }
                state.context_command = value & !(1 << 63);
            }
            0x34 => state.fault_status &= !(value as u32 & 0x7d),
            0x38 => state.fault_control = value as u32,
            0x3c => state.fault_message.0 = value as u32,
            0x40 => state.fault_message.1 = value as u32,
            0x44 => state.fault_message.2 = value as u32,
            0x64 => {
                state.protected = value as u32;
                if value & (1 << 31) == 0 {
                    state.protected &= !1;
                }
            }
            // The tail is set to 0 before the queue goes on; only then
            // does the unit run what it points past.
            0x88 if queued => self.run_queue(&mut state, value & 0xfff0),
            0x88 => assert_eq!(value, 0, "a queue tail with the queue off"),
            0x90 => state.queue_address = value & !0xfff,
            IOTLB_OFFSET => state.iotlb_address = value,
            o if o == IOTLB_OFFSET + 8 => {
                assert!(!queued, "a register invalidation while the queue is on");
                if value & (1 << 63) != 0 {
                    let address = state.iotlb_address;
                    Self::forget(
                        &mut state,
                        Told::Iotlb {
                            granularity: (value >> 60) & 3,
                            domain: (value >> 32) as u16,
                            address: address & !(PAGE - 1),
                            order: (address & 0x3f) as u8,
                            queued: false,
                        },
                    );
                }
                let granularity = (value >> 60) & 3;
                state.iotlb_command = (value & !(1 << 63)) | (granularity << 57);
            }
            _ => {}
        }
    }
}

/// A unit taken over, its simulation and its memory.
fn unit(
    levels: &[u8],
    coherent: bool,
    queued: bool,
) -> (Unit<Arc<HostMemory>>, Arc<FakeUnit>, Arc<HostMemory>) {
    let memory = HostMemory::new();
    let caps = Caps { cap: cap(levels), ecap: ecap(coherent, queued) };
    let fake = FakeUnit::new(&memory, caps);
    let unit = Unit::new(fake.mmio(), memory.clone()).unwrap();
    (unit, fake, memory)
}

/// Unmaps `frames` from `domain` and has `unit` forget them, as the
/// kernel does once a request ends.
fn unmap(unit: &mut Unit<Arc<HostMemory>>, domain: &mut Domain, frames: &[(u64, u32)]) {
    let memory = unit.memory().clone();
    let mut runs = Vec::new();
    let tables = domain.unmap(&memory, frames, &mut |from, pages| runs.push((from, pages)));
    let mut flushes = Vec::new();
    for (from, pages) in runs {
        for (address, order) in blocks(from, pages) {
            flushes.push(Flush::Pages { domain: domain.id(), address, order, leaf: !tables });
        }
    }
    unit.flush(&flushes).unwrap();
    domain.release(&memory);
}

const DISK: u16 = 0x0018; // 00:03.0
const OTHER: u16 = 0x0020; // 00:04.0

#[test]
fn a_unit_is_taken_over_and_turned_on() {
    let (mut unit, fake, _memory) = unit(&[3, 4], true, true);
    assert_eq!(unit.levels(), 4);
    assert_eq!(unit.version(), (1, 0));
    assert!(unit.queued() && !unit.found_on() && !unit.translating());
    // Everything it cached was forgotten through the queue.
    assert_eq!(
        fake.told(),
        [
            Told::Context { granularity: 1, source: 0, domain: 0, queued: true },
            Told::Iotlb { granularity: 1, domain: 0, address: 0, order: 0, queued: true },
        ]
    );
    fake.state.lock().unwrap().protected = (1 << 31) | 1;
    unit.enable().unwrap();
    assert!(unit.translating());
    assert_eq!(fake.state.lock().unwrap().protected, 0, "protected memory is switched off");
    // With translation on and nothing attached, devices reach nothing.
    assert_eq!(fake.dma(DISK, 0x1000, false), Err(0x01));
    unit.set_fault_interrupt(0xfee0_0000, 0x51);
    assert_eq!(fake.state.lock().unwrap().fault_message, (0x51, 0xfee0_0000, 0));
    unit.disable().unwrap();
    assert_eq!(fake.dma(DISK, 0x1000, false), Ok(0x1000));
}

#[test]
fn what_firmware_left_on_goes_off_first() {
    let memory = HostMemory::new();
    let caps = Caps { cap: cap(&[4]), ecap: ecap(true, true) };
    let fake = FakeUnit::new(&memory, caps);
    {
        let mut state = fake.state.lock().unwrap();
        state.root = Some(0x7000);
        state.status = (1 << 31) | (1 << 26) | (1 << 25) | (1 << 30);
    }
    let unit = Unit::new(fake.mmio(), memory.clone()).unwrap();
    assert!(unit.found_on());
    let status = fake.state.lock().unwrap().status;
    assert_eq!(status & (1 << 31), 0, "translation is off until enabled");
    assert_eq!(status & (1 << 25), 0, "interrupt remapping is off");
    assert_ne!(status & (1 << 26), 0, "the queue is on again, at its own page");
    assert_ne!(fake.state.lock().unwrap().root, Some(0x7000));
}

#[test]
fn units_that_cannot_be_used_are_refused() {
    let memory = HostMemory::new();
    let caps = Caps { cap: cap(&[]), ecap: ecap(true, true) };
    let fake = FakeUnit::new(&memory, caps);
    assert_eq!(Unit::new(fake.mmio(), memory.clone()).err(), Some(Refused::NoTables));
    let caps = Caps { cap: cap(&[4]), ecap: ecap(true, true) };
    let fake = FakeUnit::new(&memory, caps);
    let short = Mmio::fake(fake.clone(), 0x100);
    assert_eq!(Unit::new(short, memory.clone()).err(), Some(Refused::Registers));
    let fake = FakeUnit::new(&memory, caps);
    fake.state.lock().unwrap().deaf = 1 << 30;
    let refused = Unit::new(fake.mmio(), memory.clone()).err();
    assert_eq!(refused, Some(Refused::Stuck("take its root table")));
    memory.allowed.store(1, Ordering::Relaxed);
    let fake = FakeUnit::new(&memory, caps);
    assert_eq!(Unit::new(fake.mmio(), memory.clone()).err(), Some(Refused::NoMemory));
}

#[test]
fn a_device_reaches_what_its_domain_maps_and_nothing_else() {
    let (mut unit, fake, memory) = unit(&[3, 4], true, true);
    let mut domain = unit.domain(1).unwrap();
    unit.attach(0, 0x18, &domain).unwrap();
    unit.enable().unwrap();
    fake.told();
    let frames = [(0x1234_5678, 0x10), (0x8000_0000, 0x3000)];
    domain.map(&memory, &frames, &mut |_, _| panic!("nothing went")).unwrap();
    assert_eq!(fake.dma(DISK, 0x1234_5600, true), Ok(0x1234_5600));
    assert_eq!(fake.dma(DISK, 0x8000_2fff, false), Ok(0x8000_2fff));
    // The pages beside them, and other devices, are stopped.
    assert_eq!(fake.dma(DISK, 0x1234_6000, true), Err(0x05));
    assert_eq!(fake.dma(DISK, 0x8000_3000, false), Err(0x06));
    assert_eq!(fake.dma(OTHER, 0x1234_5000, false), Err(0x02));
    assert_eq!(fake.dma(0x0500, 0x1234_5000, false), Err(0x01));
    let mut faults = Vec::new();
    assert!(!unit.faults(|fault| faults.push(fault)));
    assert_eq!(faults.len(), 4);
    assert_eq!((faults[0].source, faults[0].address, faults[0].write), (DISK, 0x1234_6000, true));
    assert_eq!(faults[0].why(), "the page is not mapped for it to write");
    assert_eq!((faults[1].device(), faults[1].function(), faults[1].write), (3, 0, false));
    assert_eq!(faults[2].why(), "it has no context entry");
    assert_eq!((faults[3].bus(), faults[3].why()), (5, "its bus has no root entry"));
    assert_eq!(fake.state.lock().unwrap().fault_status & 2, 0, "every record was cleared");
    assert!(!unit.faults(|_| panic!("no new fault")));
    // A fifth fault while four are recorded is lost, and said to be.
    for page in 0..5 {
        let _ = fake.dma(DISK, 0x4000_0000 + page * PAGE, false);
    }
    let mut count = 0;
    assert!(unit.faults(|_| count += 1));
    assert_eq!(count, 4);
    // Unmapped, the pages go; the unit forgets them; the tables go too.
    unmap(&mut unit, &mut domain, &frames);
    assert_eq!(fake.dma(DISK, 0x1234_5600, true), Err(0x05));
    assert_eq!(fake.dma(DISK, 0x8000_1000, false), Err(0x06));
    // Root, context, queue, status, the domain's top and the context table.
    assert_eq!(memory.live(), 5);
    unit.detach(0, 0x18).unwrap();
    assert_eq!(fake.dma(DISK, 0x1000, false), Err(0x02));
    domain.destroy(&memory);
    assert_eq!(memory.live(), 4);
}

#[test]
fn a_page_two_requests_map_stays_until_both_end() {
    let (mut unit, fake, memory) = unit(&[4], true, true);
    let mut domain = unit.domain(7).unwrap();
    unit.attach(0, 0x18, &domain).unwrap();
    unit.enable().unwrap();
    let page = [(0x5000, 0x200)];
    let tail = [(0x5800, 0x800)];
    domain.map(&memory, &page, &mut |_, _| {}).unwrap();
    domain.map(&memory, &tail, &mut |_, _| {}).unwrap();
    assert_eq!(domain.lookup(&memory, 0x5000), Some((0x5000, 2)));
    // The device has the page in its IOTLB now.
    assert_eq!(fake.dma(DISK, 0x5100, false), Ok(0x5100));
    fake.told();
    unmap(&mut unit, &mut domain, &page);
    assert_eq!(fake.told(), [], "nothing went, so nothing is invalidated");
    assert_eq!(fake.dma(DISK, 0x5900, true), Ok(0x5900));
    unmap(&mut unit, &mut domain, &tail);
    assert_eq!(
        fake.told(),
        [Told::Iotlb { granularity: 3, domain: 7, address: 0x5000, order: 0, queued: true }]
    );
    assert_eq!(fake.dma(DISK, 0x5100, false), Err(0x06), "the IOTLB forgot it");
    assert_eq!(domain.lookup(&memory, 0x5000), None);
}

#[test]
fn a_page_mapped_too_often_stays_for_good() {
    let (unit, _fake, memory) = unit(&[4], true, true);
    let mut domain = unit.domain(2).unwrap();
    for _ in 0..1100 {
        domain.map(&memory, &[(0x9000, 8)], &mut |_, _| {}).unwrap();
    }
    assert_eq!(domain.lookup(&memory, 0x9000), Some((0x9000, 0x3ff)));
    for _ in 0..1100 {
        domain.unmap(&memory, &[(0x9000, 8)], &mut |_, _| panic!("it stays"));
    }
    assert_eq!(domain.lookup(&memory, 0x9000), Some((0x9000, 0x3ff)));
}

#[test]
fn three_levels_reach_39_bits() {
    let (mut unit, fake, memory) = unit(&[3], true, true);
    assert_eq!(unit.levels(), 3);
    let mut domain = unit.domain(3).unwrap();
    assert_eq!(domain.width(), 39);
    unit.attach(2, 0x08, &domain).unwrap();
    unit.enable().unwrap();
    domain.map(&memory, &[(0x7f_ffff_f000, 0x1000)], &mut |_, _| {}).unwrap();
    assert_eq!(fake.dma(0x0208, 0x7f_ffff_f010, true), Ok(0x7f_ffff_f010));
    let high = [(0x80_0000_0000, 0x1000)];
    assert_eq!(domain.map(&memory, &high, &mut |_, _| {}), Err(MapError::TooHigh));
    assert_eq!(fake.dma(0x0208, 0x80_0000_0000, false), Err(0x04));
}

#[test]
fn a_mapping_that_runs_out_of_memory_takes_back_what_it_mapped() {
    let (mut unit, fake, memory) = unit(&[4], true, true);
    let mut domain = unit.domain(4).unwrap();
    unit.attach(0, 0x18, &domain).unwrap();
    unit.enable().unwrap();
    // The first frame needs three tables; the second, far away, three more.
    memory.allowed.store(4, Ordering::Relaxed);
    let frames = [(0x10_0000, 0x2000), (0x40_0000_0000, 0x1000)];
    let mut went = Vec::new();
    let result = domain.map(&memory, &frames, &mut |from, pages| went.push((from, pages)));
    assert_eq!(result, Err(MapError::NoMemory));
    assert_eq!(went, [(0x10_0000, 2)]);
    memory.allowed.store(usize::MAX, Ordering::Relaxed);
    let flushes: Vec<Flush> = went
        .iter()
        .flat_map(|&(from, pages)| blocks(from, pages))
        .map(|(address, order)| Flush::Pages { domain: 4, address, order, leaf: false })
        .collect();
    unit.flush(&flushes).unwrap();
    domain.release(&memory);
    assert_eq!(fake.dma(DISK, 0x10_0000, false), Err(0x06));
    assert_eq!(domain.lookup(&memory, 0x10_1000), None);
}

#[test]
fn a_unit_without_a_queue_invalidates_through_its_registers() {
    let (mut unit, fake, memory) = unit(&[4], true, false);
    assert!(!unit.queued());
    assert_eq!(
        fake.told(),
        [
            Told::Context { granularity: 1, source: 0, domain: 0, queued: false },
            Told::Iotlb { granularity: 1, domain: 0, address: 0, order: 0, queued: false },
        ]
    );
    let mut domain = unit.domain(9).unwrap();
    unit.attach(0, 0x18, &domain).unwrap();
    assert_eq!(
        fake.told(),
        [
            Told::Context { granularity: 3, source: DISK, domain: 9, queued: false },
            Told::Iotlb { granularity: 2, domain: 9, address: 0, order: 0, queued: false },
        ]
    );
    unit.enable().unwrap();
    domain.map(&memory, &[(0x20_0000, 0x4000)], &mut |_, _| {}).unwrap();
    assert_eq!(fake.dma(DISK, 0x20_3000, false), Ok(0x20_3000));
    unmap(&mut unit, &mut domain, &[(0x20_0000, 0x4000)]);
    assert_eq!(
        fake.told(),
        [Told::Iotlb { granularity: 3, domain: 9, address: 0x20_0000, order: 2, queued: false }]
    );
    assert_eq!(fake.dma(DISK, 0x20_3000, false), Err(0x06));
}

#[test]
fn ranges_too_big_for_a_page_invalidation_take_the_domain() {
    let memory = HostMemory::new();
    // No page-selective invalidation.
    let caps = Caps { cap: cap(&[4]) & !(1 << 39), ecap: ecap(true, true) };
    let fake = FakeUnit::new(&memory, caps);
    let mut unit = Unit::new(fake.mmio(), memory.clone()).unwrap();
    fake.told();
    unit.flush(&[Flush::Pages { domain: 5, address: 0x1000, order: 0, leaf: true }]).unwrap();
    assert_eq!(
        fake.told(),
        [Told::Iotlb { granularity: 2, domain: 5, address: 0, order: 0, queued: true }]
    );
    let (mut unit, fake, _memory) = self::unit(&[4], true, true);
    fake.told();
    unit.flush(&[Flush::Pages { domain: 5, address: 0, order: 10, leaf: true }]).unwrap();
    assert_eq!(
        fake.told(),
        [Told::Iotlb { granularity: 2, domain: 5, address: 0, order: 0, queued: true }]
    );
}

#[test]
fn a_unit_that_does_not_snoop_has_its_entries_written_back() {
    let (mut unit, _fake, memory) = unit(&[4], false, true);
    let mut domain = unit.domain(1).unwrap();
    let before = memory.flushes.load(Ordering::Relaxed);
    unit.attach(0, 0x18, &domain).unwrap();
    domain.map(&memory, &[(0x3000, 0x1000)], &mut |_, _| {}).unwrap();
    // The root entry, the context entry, three tables' entries and the
    // page's, besides the queue's descriptors.
    assert!(memory.flushes.load(Ordering::Relaxed) - before >= 6);
}

#[test]
fn a_new_entry_replaces_a_present_one_only_once_it_is_forgotten() {
    let (mut unit, fake, memory) = unit(&[4], true, true);
    let mut first = unit.domain(1).unwrap();
    let mut second = unit.domain(2).unwrap();
    unit.attach(0, 0x18, &first).unwrap();
    unit.enable().unwrap();
    first.map(&memory, &[(0x6000, 0x1000)], &mut |_, _| {}).unwrap();
    second.map(&memory, &[(0x7000, 0x1000)], &mut |_, _| {}).unwrap();
    assert_eq!(fake.dma(DISK, 0x6000, false), Ok(0x6000));
    fake.told();
    unit.attach(0, 0x18, &second).unwrap();
    let told = fake.told();
    assert_eq!(told[0], Told::Context { granularity: 3, source: DISK, domain: 1, queued: true });
    assert_eq!(told[2], Told::Context { granularity: 3, source: DISK, domain: 2, queued: true });
    assert_eq!(fake.dma(DISK, 0x6000, false), Err(0x06));
    assert_eq!(fake.dma(DISK, 0x7000, false), Ok(0x7000));
}

#[test]
fn a_queue_that_refuses_a_descriptor_is_reported() {
    let (mut unit, fake, _memory) = unit(&[4], true, true);
    fake.state.lock().unwrap().refuse = true;
    assert_eq!(unit.flush(&[Flush::All]), Err(Refused::QueueError));
    assert_eq!(fake.state.lock().unwrap().fault_status & (1 << 4), 0, "the error is cleared");
}

#[test]
fn ranges_split_into_aligned_blocks() {
    let all = |start, pages| blocks(start, pages).collect::<Vec<_>>();
    assert_eq!(all(0x1000, 1), [(0x1000, 0)]);
    assert_eq!(all(0, 8), [(0, 3)]);
    assert_eq!(all(0x3000, 6), [(0x3000, 0), (0x4000, 2), (0x8000, 0)]);
    assert_eq!(all(0x10_0000, 0), []);
}

proptest! {
    /// However requests map and unmap pages in two domains, a device
    /// reaches exactly the pages its domain has mapped, even after its
    /// IOTLB held them; and once all are unmapped, no table is left.
    #[test]
    fn devices_reach_exactly_what_is_mapped(
        ops in proptest::collection::vec((any::<bool>(), 0usize..2, 0u64..24, 1u32..3), 1..80),
    ) {
        let (mut unit, fake, memory) = unit(&[4], true, true);
        let mut domains = [unit.domain(1).unwrap(), unit.domain(2).unwrap()];
        let sources = [DISK, OTHER];
        unit.attach(0, 0x18, &domains[0]).unwrap();
        unit.attach(0, 0x20, &domains[1]).unwrap();
        unit.enable().unwrap();
        // Pages spread over tables: 0..8 in one, 8..16 in the next 2 MiB,
        // 16..24 in the next 1 GiB.
        let place = |page: u64| match page / 8 {
            0 => page * PAGE,
            1 => (1 << 21) + (page % 8) * PAGE,
            _ => (1 << 30) + (page % 8) * PAGE,
        };
        let mut counts = [[0u32; 24]; 2];
        let mut held: [Vec<(u64, u32)>; 2] = [Vec::new(), Vec::new()];
        for (map, which, page, pages) in ops {
            let frames: Vec<(u64, u32)> =
                (page..(page + u64::from(pages)).min(24)).map(|p| (place(p), PAGE as u32)).collect();
            if map || held[which].is_empty() {
                domains[which].map(&memory, &frames, &mut |_, _| {}).unwrap();
                for p in page..(page + u64::from(pages)).min(24) {
                    counts[which][p as usize] += 1;
                }
                held[which].extend_from_slice(&frames);
            } else {
                let frame = held[which].remove(0);
                unmap(&mut unit, &mut domains[which], &[frame]);
                let p = (0..24).find(|&p| place(p) == frame.0).unwrap();
                counts[which][p as usize] -= 1;
            }
            for (d, source) in sources.iter().enumerate() {
                for p in 0..24u64 {
                    let reached = fake.dma(*source, place(p) + 8, false).is_ok();
                    prop_assert_eq!(reached, counts[d][p as usize] > 0);
                }
            }
            unit.faults(|_| {});
        }
        for which in 0..2 {
            let frames = core::mem::take(&mut held[which]);
            unmap(&mut unit, &mut domains[which], &frames);
            for p in 0..24 {
                prop_assert_eq!(domains[which].lookup(&memory, place(p)), None);
            }
        }
        // The root table, bus 0's context table, the queue and its status
        // page, and the two domains' top tables.
        prop_assert_eq!(memory.live(), 6);
    }
}
