// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The engine against a simulated IOMMU: its registers as AMD IOMMU 3.10
//! has them, a command buffer, an event log, caches of device table
//! entries and of translations that keep what they hold until told to
//! forget it, and table walks done from the specification rather than
//! from the engine's code.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use proptest::prelude::*;
use radix_hw::{Mmio, Registers};

use crate::{
    Domain, Event, Invalidate, LEVELS, Memory, PAGE, Refused, Setup, Tables, Unit, blocks,
};

/// A page of table entries, aligned as the IOMMU needs.
#[repr(C, align(4096))]
struct Table([AtomicU64; 512]);

/// Host memory for tables: each page leaked, found again by the address
/// it stands at.
#[derive(Default)]
struct HostMemory {
    pages: Mutex<HashMap<u64, &'static Table>>,
    /// Allocations allowed before they fail.
    allowed: AtomicUsize,
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

    fn word(&self, phys: u64) -> &'static AtomicU64 {
        &self.table(phys & !(PAGE - 1)).0[(phys as usize & 0xfff) / 8]
    }

    fn read(&self, phys: u64) -> u64 {
        self.word(phys).load(Ordering::Acquire)
    }

    fn take(&self) -> bool {
        let allowed = self.allowed.load(Ordering::Relaxed);
        if allowed == 0 {
            return false;
        }
        if allowed != usize::MAX {
            self.allowed.store(allowed - 1, Ordering::Relaxed);
        }
        true
    }
}

impl Memory for HostMemory {
    fn alloc_page(&self) -> Option<u64> {
        self.alloc_run(1)
    }

    fn free_page(&self, phys: u64) {
        self.free_run(phys, 1);
    }

    fn entries(&self, phys: u64) -> &[AtomicU64; 512] {
        &self.table(phys).0
    }

    fn flush(&self, _entry: &AtomicU64) {}

    fn delay_us(&self, _us: u32) {}
}

impl HostMemory {
    fn alloc_run(&self, count: usize) -> Option<u64> {
        if !self.take() {
            return None;
        }
        let run: &'static [Table] =
            Vec::from_iter((0..count).map(|_| Table([const { AtomicU64::new(0) }; 512]))).leak();
        let first = run.as_ptr() as u64;
        let mut pages = self.pages.lock().unwrap();
        for (n, table) in run.iter().enumerate() {
            pages.insert(first + n as u64 * PAGE, table);
        }
        Some(first)
    }

    fn free_run(&self, phys: u64, count: usize) {
        let mut pages = self.pages.lock().unwrap();
        for n in 0..count as u64 {
            assert!(pages.remove(&(phys + n * PAGE)).is_some(), "a page given back twice");
        }
    }
}

impl Tables for Arc<HostMemory> {
    fn alloc_run(&self, count: usize) -> Option<u64> {
        HostMemory::alloc_run(self, count)
    }

    fn free_run(&self, phys: u64, count: usize) {
        HostMemory::free_run(self, phys, count);
    }
}

/// What the simulated IOMMU was told to forget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Told {
    Device(u16),
    /// A domain's pages: the first, how many bytes, and whether page
    /// directory entries too.
    Pages {
        domain: u16,
        start: u64,
        bytes: u128,
        tables: bool,
    },
    All,
}

#[derive(Default)]
struct State {
    control: u64,
    status: u64,
    device_table: u64,
    commands: u64,
    command_head: u64,
    command_tail: u64,
    events: u64,
    event_head: u64,
    event_tail: u64,
    exclusion: (u64, u64),
    told: Vec<Told>,
    /// The first two words of each device table entry it read.
    entries: HashMap<u16, (u64, u64)>,
    /// What it translated: (domain, page) to page.
    iotlb: BTreeMap<(u16, u64), u64>,
}

/// A simulated IOMMU.
struct FakeIommu {
    features: u64,
    memory: Arc<HostMemory>,
    state: Mutex<State>,
}

const ENABLE: u64 = 1;
const EVENT_LOG: u64 = 1 << 2;
const COMMAND_BUFFER: u64 = 1 << 12;
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;

impl FakeIommu {
    fn new(memory: &Arc<HostMemory>, features: u64) -> Arc<FakeIommu> {
        Arc::new(FakeIommu {
            features,
            memory: memory.clone(),
            state: Mutex::new(State::default()),
        })
    }

    fn mmio(self: &Arc<FakeIommu>) -> Mmio {
        Mmio::fake(self.clone(), 0x4000)
    }

    fn told(&self) -> Vec<Told> {
        core::mem::take(&mut self.state.lock().unwrap().told)
    }

    /// Logs an event, if the log is on and has room.
    fn log(&self, state: &mut State, low: u64, high: u64) {
        if state.control & (ENABLE | EVENT_LOG) != ENABLE | EVENT_LOG || state.status & 1 != 0 {
            return;
        }
        let len = 4096;
        if (state.event_tail + 16) % len == state.event_head {
            state.status |= 1;
            return;
        }
        let at = (state.events & ADDRESS) + state.event_tail;
        self.memory.word(at).store(low, Ordering::Release);
        self.memory.word(at + 8).store(high, Ordering::Release);
        state.event_tail = (state.event_tail + 16) % len;
        state.status |= 2;
    }

    /// Translates a DMA request of the device `device` to `address`, as
    /// the IOMMU does: through its caches, else by reading its device
    /// table entry and walking the tables. A request it stops is logged,
    /// with its flags.
    fn dma(&self, device: u16, address: u64, write: bool) -> Result<u64, u16> {
        let mut state = self.state.lock().unwrap();
        let result = self.translate(&mut state, device, address, write);
        if let Err(flags) = result {
            let flags = flags | (u16::from(write) << 5);
            let low = u64::from(device) | (u64::from(flags) << 48) | (2 << 60);
            self.log(&mut state, low, address);
        }
        result
    }

    fn translate(
        &self,
        state: &mut State,
        device: u16,
        address: u64,
        write: bool,
    ) -> Result<u64, u16> {
        const PRESENT: u16 = 1 << 4;
        const PERMISSION: u16 = 1 << 6;
        const RESERVED: u16 = 1 << 7;
        if state.control & ENABLE == 0 {
            return Ok(address);
        }
        let (first, second) = match state.entries.get(&device) {
            Some(&cached) => cached,
            None => {
                let table = state.device_table & ADDRESS;
                let size = ((state.device_table & 0x1ff) + 1) * PAGE;
                let at = u64::from(device) * 32;
                assert!(at < size, "a device past the device table");
                let entry = (self.memory.read(table + at), self.memory.read(table + at + 8));
                state.entries.insert(device, entry);
                entry
            }
        };
        if first & 1 == 0 {
            return Ok(address);
        }
        assert!(first & 2 != 0, "the engine leaves no entry valid without translation");
        let allowed =
            |entry: u64| if write { entry & (1 << 62) != 0 } else { entry & (1 << 61) != 0 };
        let mode = ((first >> 9) & 7) as u8;
        if !allowed(first) {
            return Err(PERMISSION);
        }
        if mode == 0 {
            return Ok(address);
        }
        let domain = second as u16;
        let page = address & !(PAGE - 1);
        if let Some(&to) = state.iotlb.get(&(domain, page)) {
            return Ok(to | (address & (PAGE - 1)));
        }
        if address >> (12 + 9 * u32::from(mode)) != 0 {
            return Err(PRESENT);
        }
        let mut table = first & ADDRESS;
        let mut level = mode;
        loop {
            let at = (address >> (12 + 9 * u32::from(level - 1))) & 0x1ff;
            let entry = self.memory.read(table + at * 8);
            if entry & 1 == 0 {
                return Err(0);
            }
            // Bits 52 to 58 are reserved; 59 and 60 are attributes.
            if entry & (0x7f << 52) != 0 {
                return Err(PRESENT | RESERVED);
            }
            if !allowed(entry) {
                return Err(PRESENT | PERMISSION);
            }
            let next = ((entry >> 9) & 7) as u8;
            if next == 0 {
                assert_eq!(level, 1, "only 4 KiB pages are mapped");
                break;
            }
            assert_eq!(next, level - 1, "no level is skipped");
            table = entry & ADDRESS;
            level = next;
        }
        let to = self.memory.read(table + ((address >> 12) & 0x1ff) * 8) & ADDRESS;
        state.iotlb.insert((domain, page), to);
        Ok(to | (address & (PAGE - 1)))
    }

    /// Runs the commands up to the tail.
    fn run(&self, state: &mut State) {
        while state.command_head != state.command_tail {
            let at = (state.commands & ADDRESS) + state.command_head;
            let (low, high) = (self.memory.read(at), self.memory.read(at + 8));
            match low >> 60 {
                1 => {
                    assert_eq!(low & 6, 0, "no interrupt and no flush on completion");
                    if low & 1 != 0 {
                        self.memory
                            .word(low & 0x000f_ffff_ffff_fff8)
                            .store(high, Ordering::Release);
                    }
                }
                2 => {
                    let device = low as u16;
                    state.entries.remove(&device);
                    state.told.push(Told::Device(device));
                }
                3 => {
                    assert_eq!((low >> 48) & 0xfff, 0, "reserved bits of the command");
                    let domain = (low >> 32) as u16;
                    let range = high & 1 != 0;
                    let tables = high & 2 != 0;
                    let address = high & !(PAGE - 1);
                    let (start, bytes) = if range {
                        // The first clear bit from 12 up says the size.
                        let ones = (address >> 12).trailing_ones();
                        let bytes = 1u128 << (12 + ones + 1);
                        (address & !((bytes as u64).wrapping_sub(1)), bytes)
                    } else {
                        (address, u128::from(PAGE))
                    };
                    state.iotlb.retain(|&(d, page), _| {
                        d != domain
                            || !(u128::from(start)..u128::from(start) + bytes)
                                .contains(&u128::from(page))
                    });
                    state.told.push(Told::Pages { domain, start, bytes, tables });
                }
                8 => {
                    state.entries.clear();
                    state.iotlb.clear();
                    state.told.push(Told::All);
                }
                other => panic!("command {other} the engine does not give"),
            }
            state.command_head = (state.command_head + 16) % 4096;
        }
    }
}

impl Registers for FakeIommu {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 8, "the registers are read 64 bits at once");
        let state = self.state.lock().unwrap();
        match offset {
            0x00 => state.device_table,
            0x08 => state.commands,
            0x10 => state.events,
            0x18 => state.control,
            0x20 => state.exclusion.0,
            0x28 => state.exclusion.1,
            0x30 => self.features,
            0x2000 => state.command_head,
            0x2008 => state.command_tail,
            0x2010 => state.event_head,
            0x2018 => state.event_tail,
            0x2020 => state.status,
            _ => 0,
        }
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 8, "the registers are written 64 bits at once");
        let mut state = self.state.lock().unwrap();
        let running = |control: u64, bits: u64| control & bits == bits;
        match offset {
            0x00 => {
                assert!(!running(state.control, ENABLE), "a device table moved while translating");
                state.device_table = value;
            }
            0x08 => state.commands = value,
            0x10 => state.events = value,
            0x18 => {
                if running(value, ENABLE) {
                    assert!(state.device_table != 0, "translation on without a device table");
                }
                state.control = value;
                // As QEMU's, its rings run only while it translates.
                let mut running_bits = 0;
                if running(value, ENABLE | COMMAND_BUFFER) {
                    running_bits |= 1 << 4;
                }
                if running(value, ENABLE | EVENT_LOG) {
                    running_bits |= 1 << 3;
                }
                state.status = (state.status & !0x18) | running_bits;
                if running(value, ENABLE | COMMAND_BUFFER) {
                    self.run(&mut state);
                }
            }
            0x20 => state.exclusion.0 = value,
            0x28 => state.exclusion.1 = value,
            0x2000 => {
                assert!(!running(state.control, COMMAND_BUFFER), "the head moved while running");
                state.command_head = value & 0x7_fff0;
            }
            0x2008 => {
                state.command_tail = value & 0x7_fff0;
                if running(state.control, ENABLE | COMMAND_BUFFER) {
                    self.run(&mut state);
                }
            }
            0x2010 => state.event_head = value & 0x7_fff0,
            0x2018 => {
                assert!(!running(state.control, EVENT_LOG), "the event tail moved while logging");
                state.event_tail = value & 0x7_fff0;
            }
            0x2020 => state.status &= !(value & 7),
            _ => panic!("a write to register {offset:#x}"),
        }
    }
}

/// The features of an IOMMU that takes INVALIDATE_IOMMU_ALL.
const INVALIDATE_ALL: u64 = 1 << 6;

const DISK: u16 = 0x0018; // 00:03.0
const OTHER: u16 = 0x0020; // 00:04.0

/// An IOMMU taken over, its simulation and its memory.
fn unit(features: u64, setup: Setup) -> (Unit<Arc<HostMemory>>, Arc<FakeIommu>, Arc<HostMemory>) {
    let memory = HostMemory::new();
    let fake = FakeIommu::new(&memory, features);
    let unit = Unit::new(fake.mmio(), memory.clone(), setup).unwrap();
    (unit, fake, memory)
}

fn setup() -> Setup {
    Setup { flags: 0, last_device: 0xff, caches_absent: false }
}

/// Unmaps `frames` from `domain` and has `unit` forget them, as the
/// kernel does once a request ends.
fn unmap(unit: &mut Unit<Arc<HostMemory>>, domain: &mut Domain, frames: &[(u64, u32)]) {
    let memory = unit.memory().clone();
    radix_iopt::unmap(domain, unit, &memory, frames).unwrap();
}

fn map(unit: &mut Unit<Arc<HostMemory>>, domain: &mut Domain, frames: &[(u64, u32)]) {
    let memory = unit.memory().clone();
    assert_eq!(radix_iopt::map(domain, unit, &memory, frames), Ok(true));
}

fn events(unit: &Unit<Arc<HostMemory>>) -> (Vec<Event>, bool) {
    let mut found = Vec::new();
    let lost = unit.event_log().read(|event| found.push(event));
    (found, lost)
}

#[test]
fn an_iommu_is_taken_over_and_turned_on() {
    let (mut unit, fake, memory) = unit(INVALIDATE_ALL, Setup { flags: 0b1011, ..setup() });
    assert!(!unit.found_on() && unit.translating());
    assert_eq!(unit.devices(), 256);
    // Everything it cached was forgotten with the one command.
    assert_eq!(fake.told(), [Told::All]);
    {
        let state = fake.state.lock().unwrap();
        // Translating, coherent, a second's timeout, the command buffer,
        // the event log, HT tunnelling, posted writes passed and
        // isochronous traffic.
        let on = ENABLE | (1 << 10) | (4 << 5) | (1 << 12) | (1 << 2) | 2 | (1 << 8) | (1 << 11);
        assert_eq!(state.control, on);
        // A device table of two pages, of 256 entries.
        assert_eq!(state.device_table & 0x1ff, 1);
        assert_eq!(state.commands >> 56, 8);
        assert_eq!(state.events >> 56, 8);
        // Every entry blocks.
        let table = state.device_table & ADDRESS;
        for device in 0..256u64 {
            assert_eq!(memory.read(table + device * 32), 3);
        }
    }
    assert_eq!(fake.dma(DISK, 0x1000, false), Err(1 << 6));
    let (found, lost) = events(&unit);
    assert!(!lost);
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].device, found[0].address, found[0].code), (Some(DISK), 0x1000, 2));
    assert_eq!(found[0].why(), "the page is not mapped for it");
    // The device table's two pages, the command buffer and its completion
    // page, the event log.
    assert_eq!(memory.live(), 5);
    unit.disable();
    assert!(!unit.translating());
    unit.enable();
    assert!(unit.translating());
}

#[test]
fn without_the_one_command_every_entry_is_forgotten() {
    let (_unit, fake, _memory) = unit(0, setup());
    let told = fake.told();
    assert_eq!(told.len(), 256);
    assert!(told.iter().enumerate().all(|(n, told)| *told == Told::Device(n as u16)));
}

#[test]
fn what_firmware_left_on_goes_off_first() {
    let memory = HostMemory::new();
    let fake = FakeIommu::new(&memory, INVALIDATE_ALL);
    {
        let mut state = fake.state.lock().unwrap();
        state.control = ENABLE | COMMAND_BUFFER | EVENT_LOG | (1 << 3);
        state.status = 0x18;
        state.exclusion = (0x8000_0001, 0x8fff_f000);
        state.device_table = 0x1234_0000;
    }
    let unit = Unit::new(fake.mmio(), memory.clone(), setup()).unwrap();
    assert!(unit.found_on() && unit.translating());
    let state = fake.state.lock().unwrap();
    assert_eq!(state.exclusion, (0, 0));
    assert_ne!(state.device_table, 0x1234_0000);
}

#[test]
fn iommus_that_cannot_be_used_are_refused() {
    let memory = HostMemory::new();
    let fake = FakeIommu::new(&memory, 0);
    let short = Mmio::fake(fake.clone(), 0x1000);
    assert_eq!(Unit::new(short, memory.clone(), setup()).err(), Some(Refused::Registers));
    memory.allowed.store(2, Ordering::Relaxed);
    assert_eq!(Unit::new(fake.mmio(), memory.clone(), setup()).err(), Some(Refused::NoMemory));
    assert_eq!(memory.live(), 0, "what was allocated was given back");
}

#[test]
fn a_device_reaches_what_its_domain_maps_and_nothing_else() {
    let (mut unit, fake, memory) = unit(INVALIDATE_ALL, setup());
    let mut domain = unit.domain(1).unwrap();
    fake.told();
    unit.attach(DISK, &domain).unwrap();
    assert_eq!(
        fake.told(),
        [Told::Device(DISK), Told::Pages { domain: 1, start: 0, bytes: 1 << 64, tables: true },]
    );
    let frames = [(0x1234_5678, 0x10), (0x8000_0000, 0x3000)];
    map(&mut unit, &mut domain, &frames);
    assert!(fake.told().is_empty(), "nothing to forget after mapping");
    assert_eq!(fake.dma(DISK, 0x1234_5600, true), Ok(0x1234_5600));
    assert_eq!(fake.dma(DISK, 0x8000_2fff, false), Ok(0x8000_2fff));
    // The pages beside them, and other devices, are stopped.
    assert!(fake.dma(DISK, 0x1234_6000, true).is_err());
    assert!(fake.dma(DISK, 0x8000_3000, false).is_err());
    assert!(fake.dma(OTHER, 0x1234_5000, false).is_err());
    let (found, _) = events(&unit);
    assert_eq!(found.len(), 3);
    assert_eq!(
        (found[0].device, found[0].address, found[0].write),
        (Some(DISK), 0x1234_6000, true)
    );
    assert_eq!(found[0].why(), "the page is not mapped for it");
    assert_eq!((found[1].write, found[2].device), (false, Some(OTHER)));
    assert!(events(&unit).0.is_empty(), "the log was emptied");
    // Unmapped, the pages go; the IOMMU forgets them; the tables go too.
    unmap(&mut unit, &mut domain, &frames);
    assert!(fake.dma(DISK, 0x1234_5600, true).is_err());
    assert!(fake.dma(DISK, 0x8000_1000, false).is_err());
    // The IOMMU's five pages, and the domain's top table.
    assert_eq!(memory.live(), 6);
    unit.detach(DISK).unwrap();
    assert!(fake.dma(DISK, 0x1000, false).is_err());
    domain.destroy(&memory);
    assert_eq!(memory.live(), 5);
}

#[test]
fn a_new_entry_replaces_one_only_once_it_is_forgotten() {
    let (mut unit, fake, _memory) = unit(INVALIDATE_ALL, setup());
    let mut first = unit.domain(1).unwrap();
    let second = unit.domain(2).unwrap();
    unit.attach(DISK, &first).unwrap();
    map(&mut unit, &mut first, &[(0x5000, 4096)]);
    assert_eq!(fake.dma(DISK, 0x5000, false), Ok(0x5000));
    fake.told();
    unit.attach(DISK, &second).unwrap();
    assert_eq!(
        fake.told(),
        [
            Told::Device(DISK),
            Told::Pages { domain: 1, start: 0, bytes: 1 << 64, tables: true },
            Told::Device(DISK),
            Told::Pages { domain: 2, start: 0, bytes: 1 << 64, tables: true },
        ]
    );
    assert!(fake.dma(DISK, 0x5000, false).is_err());
}

#[test]
fn blocks_are_forgotten_as_ranges() {
    let (mut unit, fake, _memory) = unit(INVALIDATE_ALL, setup());
    fake.told();
    let found: Vec<(u64, u8)> = blocks(0x3000, 13).collect();
    unit.forget_pages(5, &found, true).unwrap();
    assert_eq!(
        fake.told(),
        [
            Told::Pages { domain: 5, start: 0x3000, bytes: 0x1000, tables: false },
            Told::Pages { domain: 5, start: 0x4000, bytes: 0x4000, tables: false },
            Told::Pages { domain: 5, start: 0x8000, bytes: 0x8000, tables: false },
        ]
    );
    unit.forget_pages(5, &[(1 << 30, 18)], false).unwrap();
    assert_eq!(
        fake.told(),
        [Told::Pages { domain: 5, start: 1 << 30, bytes: 1 << 30, tables: true }]
    );
    unit.forget_domain(6).unwrap();
    assert_eq!(fake.told(), [Told::Pages { domain: 6, start: 0, bytes: 1 << 64, tables: true }]);
}

#[test]
fn an_iommu_that_caches_absent_entries_forgets_new_pages() {
    let (mut unit, fake, _memory) = unit(INVALIDATE_ALL, Setup { caches_absent: true, ..setup() });
    let mut domain = unit.domain(3).unwrap();
    unit.attach(DISK, &domain).unwrap();
    fake.told();
    map(&mut unit, &mut domain, &[(0x7000, 4096)]);
    assert_eq!(
        fake.told(),
        [Told::Pages { domain: 3, start: 0x7000, bytes: 0x1000, tables: false }]
    );
}

#[test]
fn a_full_event_log_starts_again() {
    let (unit, fake, _memory) = unit(INVALIDATE_ALL, setup());
    // 255 entries fit in a ring of 256.
    for page in 0..300 {
        let _ = fake.dma(OTHER, page * PAGE, false);
    }
    let (found, lost) = events(&unit);
    assert!(lost);
    assert_eq!(found.len(), 255);
    let _ = fake.dma(OTHER, 0x9000, true);
    let (found, lost) = events(&unit);
    assert!(!lost);
    assert_eq!((found.len(), found[0].address), (1, 0x9000));
}

#[test]
fn every_page_table_level_is_walked() {
    let (mut unit, fake, memory) = unit(INVALIDATE_ALL, setup());
    let mut domain = unit.domain(1).unwrap();
    unit.attach(DISK, &domain).unwrap();
    let top = (1u64 << 47) - PAGE;
    map(&mut unit, &mut domain, &[(top, 4096)]);
    assert_eq!(fake.dma(DISK, top + 0x10, true), Ok(top + 0x10));
    assert_eq!(domain.levels(), LEVELS);
    let mut cleared = |_: u64, _: u64| {};
    assert!(domain.map_range(&memory, 1 << 48, 4096, &mut cleared).is_err());
}

proptest! {
    /// However requests map and unmap pages in two domains, a device
    /// reaches exactly the pages its domain has mapped, even after the
    /// IOMMU cached them; and once all are unmapped, no table is left.
    #[test]
    fn devices_reach_exactly_what_is_mapped(
        ops in proptest::collection::vec((any::<bool>(), 0usize..2, 0u64..24, 1u32..3), 1..80),
    ) {
        let (mut unit, fake, memory) = unit(INVALIDATE_ALL, setup());
        let mut domains = [unit.domain(1).unwrap(), unit.domain(2).unwrap()];
        let devices = [DISK, OTHER];
        unit.attach(DISK, &domains[0]).unwrap();
        unit.attach(OTHER, &domains[1]).unwrap();
        // Pages spread over tables: 0..8 in one, 8..16 in the next 2 MiB,
        // 16..24 in the next 1 GiB.
        let place = |page: u64| match page / 8 {
            0 => page * PAGE,
            1 => (1 << 21) + (page % 8) * PAGE,
            _ => (1 << 30) + (page % 8) * PAGE,
        };
        let mut counts = [[0u32; 24]; 2];
        let mut held: [Vec<(u64, u32)>; 2] = [Vec::new(), Vec::new()];
        for (add, which, page, pages) in ops {
            let frames: Vec<(u64, u32)> =
                (page..(page + u64::from(pages)).min(24)).map(|p| (place(p), PAGE as u32)).collect();
            if add || held[which].is_empty() {
                map(&mut unit, &mut domains[which], &frames);
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
            for (d, device) in devices.iter().enumerate() {
                for p in 0..24u64 {
                    let reached = fake.dma(*device, place(p) + 8, false).is_ok();
                    prop_assert_eq!(reached, counts[d][p as usize] > 0);
                }
            }
            events(&unit);
        }
        for which in 0..2 {
            let frames = core::mem::take(&mut held[which]);
            unmap(&mut unit, &mut domains[which], &frames);
            for p in 0..24 {
                prop_assert_eq!(domains[which].lookup(&memory, place(p)), None);
            }
        }
        // The IOMMU's five pages, and the two domains' top tables.
        prop_assert_eq!(memory.live(), 7);
    }
}
