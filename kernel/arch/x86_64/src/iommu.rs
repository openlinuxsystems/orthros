// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The IOMMU: Intel VT-d DMA remapping, through `radix-vtd` (ADR 0164).
//!
//! [`init`] takes over the remapping units the ACPI DMAR table lists, at
//! the end of `Platform::init`, before any driver starts a device. Each
//! PCI function that DMA memory is allocated or mapped for gets its own
//! domain, attached when the function is first named, and reaches only
//! its DMA memory, the pages mapped for its requests in flight, and the
//! memory the firmware reserved for it (RMRRs), each at its physical
//! address: a device address is a physical address, behind a unit or not.
//! A function whose DMA nothing asked for reaches no memory at all.
//!
//! Invalidation is strict: when [`unmap`] returns, the device can no
//! longer reach what it gave back. A unit that translates only the
//! display controller is left off, since no graphics driver hands it
//! memory yet, and `iommu=off` leaves every unit off. Faults are logged,
//! at most [`FAULTS_LOGGED`] in each [`FAULT_WINDOW_NS`].
//!
//! Locks: a device's domain, then its unit, then the frame allocator's.
//! The fault interrupt takes only the lock of the unit's fault records,
//! registers of their own, so it never waits for a change to the tables;
//! nothing is logged with a lock held.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use radix_acpi::{Dmar, DmarUnit, ScopeKind, mcfg_entries};
use radix_hw::pci::Address;
use radix_klog::TextBuf;
use radix_mm::Owner;
use radix_sync::{Once, SpinLock};
use radix_vtd::{Caps, Domain, FaultRecords, Flush, Memory, Refused, Unit, blocks};

use crate::memory::{self, PAGE};
use crate::{acpi, cpu, devices, time};

/// The most units used.
const MAX_UNITS: usize = 8;
/// The most devices a unit names in its scopes.
const MAX_SCOPES: usize = 16;
/// The most PCI functions with domains.
const MAX_DEVICES: usize = 128;
/// Runs of pages one unmapping remembers to invalidate one by one; past
/// that it invalidates the whole domain.
const RUNS: usize = 16;
/// Faults logged in each window.
pub const FAULTS_LOGGED: u32 = 10;
/// The window, in nanoseconds.
pub const FAULT_WINDOW_NS: u64 = 5_000_000_000;

/// Table memory: kernel pages, reached through the direct map.
#[derive(Clone, Copy, Debug)]
struct Tables;

impl Memory for Tables {
    fn alloc_page(&self) -> Option<u64> {
        memory::alloc_pages(1, false, Owner::Kernel)
    }

    fn free_page(&self, phys: u64) {
        memory::free_pages(phys, 1, Owner::Kernel);
    }

    fn entries(&self, phys: u64) -> &[AtomicU64; 512] {
        // SAFETY: the engine passes only pages `alloc_page` gave it and
        // has not given back: zeroed RAM, mapped by the direct map at
        // `phys_to_virt`, page-aligned, and touched only through atomics.
        unsafe { &*(memory::phys_to_virt(phys) as *const [AtomicU64; 512]) }
    }

    fn flush(&self, entry: &AtomicU64) {
        // CLFLUSH is ordered with later writes, so the register write that
        // has the unit look again comes after the line is in memory.
        cpu::clflush(core::ptr::from_ref(entry) as usize);
    }

    fn delay_us(&self, us: u32) {
        time::delay_us(u64::from(us));
    }
}

/// A device a unit names: one function, or a bridge and the buses below it.
#[derive(Clone, Copy, Debug)]
struct Scope {
    bus: u8,
    devfn: u8,
    /// A bridge's: its secondary and subordinate bus.
    below: Option<(u8, u8)>,
}

impl Scope {
    fn names(self, bus: u8, devfn: u8) -> bool {
        (self.bus == bus && self.devfn == devfn)
            || self.below.is_some_and(|(first, last)| (first..=last).contains(&bus))
    }
}

/// A unit and what it translates.
struct Remapper {
    registers: u64,
    segment: u16,
    include_all: bool,
    scopes: [Option<Scope>; MAX_SCOPES],
    /// `None` for a unit left off: its devices reach all memory.
    unit: Option<SpinLock<Unit<Tables>>>,
    /// Its fault recording registers, which its fault interrupt reads.
    faults: Option<SpinLock<FaultRecords>>,
    caps: Caps,
    /// The next domain number; 0 is never used, as caching mode wants.
    next_domain: AtomicU32,
}

impl Remapper {
    fn names(&self, source: Address) -> bool {
        let devfn = (source.device << 3) | source.function;
        self.segment == source.segment
            && self.scopes.iter().flatten().any(|scope| scope.names(source.bus, devfn))
    }
}

/// A function with DMA, and its domain.
struct Device {
    source: Address,
    /// The unit it is behind, if one translates it.
    remapper: Option<usize>,
    domain: SpinLock<Option<Domain>>,
}

static UNITS: [Once<Remapper>; MAX_UNITS] = [const { Once::new() }; MAX_UNITS];
static DEVICES: [Once<Device>; MAX_DEVICES] = [const { Once::new() }; MAX_DEVICES];
/// Held while a device is added.
static ADDING: SpinLock<()> = SpinLock::new(());
/// Some unit translates.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// When the present window of logged faults began, and how many it has.
static LOGGED: SpinLock<(u64, u32)> = SpinLock::new((0, 0));
/// A unit stopped invalidating; told of once.
static STALLED: AtomicBool = AtomicBool::new(false);

/// Whether a unit translates DMA.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// The tag a [`radix_hw::Dma`] mapped for `source` carries, for
/// [`untag`]: its entry's number, from 1; 0 if it has none.
pub fn tag(source: Address) -> u32 {
    DEVICES
        .iter()
        .map_while(Once::get)
        .position(|device| device.source == source)
        .map_or(0, |at| at as u32 + 1)
}

/// The function a tag from [`tag`] names; `None` for 0.
pub fn untag(tag: u32) -> Option<Address> {
    let at = usize::try_from(tag.checked_sub(1)?).ok()?;
    Some(DEVICES.get(at)?.get()?.source)
}

/// The configuration space of a function, through its segment's ECAM
/// window.
fn config(segment: u16, bus: u8, device: u8, function: u8) -> Option<radix_hw::Mmio> {
    let mcfg = acpi::get()?.mcfg.as_ref()?;
    let window = mcfg_entries(mcfg)
        .find(|w| w.segment == segment && (w.start_bus..=w.end_bus).contains(&bus))?;
    let offset = (u64::from(bus) << 20) | (u64::from(device) << 15) | (u64::from(function) << 12);
    let config = devices::map(window.base + offset, 4096)?;
    (config.read16(0) != 0xffff).then_some(config)
}

/// Where a scope's path leads: the bus and the (device, function) of its
/// last hop, through the bridges of the others.
fn resolve(segment: u16, start: u8, path: impl Iterator<Item = (u8, u8)>) -> Option<(u8, u8, u8)> {
    let mut hop: Option<(u8, u8, u8)> = None;
    for (device, function) in path {
        let bus = match hop {
            None => start,
            // The secondary bus of the bridge before.
            Some((bus, device, function)) => config(segment, bus, device, function)?.read8(0x19),
        };
        hop = Some((bus, device, function));
    }
    hop
}

/// The scopes of `unit` that name PCI devices, and whether each of its
/// functions is a display controller.
fn scopes(unit: &DmarUnit<'_>) -> ([Option<Scope>; MAX_SCOPES], bool) {
    let mut found = [None; MAX_SCOPES];
    let mut count = 0;
    let mut displays = true;
    for scope in unit.scopes() {
        if !matches!(scope.kind, ScopeKind::Endpoint | ScopeKind::Bridge) {
            continue;
        }
        let Some((bus, device, function)) = resolve(unit.segment, scope.start_bus, scope.path())
        else {
            continue;
        };
        let config = config(unit.segment, bus, device, function);
        let below = match (scope.kind, &config) {
            (ScopeKind::Bridge, Some(config)) => Some((config.read8(0x19), config.read8(0x1a))),
            _ => None,
        };
        displays &= scope.kind == ScopeKind::Endpoint
            && config.as_ref().is_some_and(|config| config.read8(0x0b) == 0x03);
        if count < MAX_SCOPES {
            found[count] = Some(Scope { bus, devfn: (device << 3) | function, below });
            count += 1;
        }
    }
    (found, displays && count > 0)
}

/// Takes over every unit the DMAR table lists and turns translation on,
/// unless `off`. Logs what it did.
pub fn init(off: bool) {
    let Some(acpi) = acpi::get() else { return };
    let Some(sdt) = acpi.tables.find(&acpi::PhysicalMemory, b"DMAR") else { return };
    let Ok(dmar) = Dmar::parse(&sdt) else {
        radix_klog::warn!("IOMMU: the DMAR table is too short; DMA remapping is off");
        return;
    };
    if off {
        radix_klog::info!(
            "IOMMU: DMA remapping left off by the iommu=off flag; every device reaches all memory"
        );
        return;
    }
    let mut count = 0;
    for found in dmar.units() {
        if count == MAX_UNITS {
            radix_klog::warn!(
                "IOMMU: more than {MAX_UNITS} remapping units; the one at {:#x} is left off",
                found.registers
            );
            continue;
        }
        if take(&found, count) {
            count += 1;
        }
    }
    reserve(&dmar);
    let mut on = false;
    for (index, slot) in UNITS.iter().enumerate() {
        let Some(remapper) = slot.get() else { break };
        let Some(unit) = &remapper.unit else { continue };
        let (enabled, version, found_on) = {
            let mut unit = unit.lock_irq();
            (unit.enable(), unit.version(), unit.found_on())
        };
        if let Err(err) = enabled {
            radix_klog::warn!(
                "IOMMU: the unit at {:#x} would not translate: {err}",
                remapper.registers
            );
            continue;
        }
        on = true;
        log_unit(index, remapper, version, found_on);
    }
    ACTIVE.store(on, Ordering::Release);
}

/// Takes over the unit `found` as unit `index`; false if it was not used.
fn take(found: &DmarUnit<'_>, index: usize) -> bool {
    let pages = found.register_pages.max(1) as usize;
    let Some(regs) = devices::map(found.registers, pages * PAGE as usize) else {
        radix_klog::warn!(
            "IOMMU: the registers of the unit at {:#x} could not be mapped",
            found.registers
        );
        return false;
    };
    let caps = Caps::read(&regs);
    // Some units' fault and IOTLB registers lie past the pages the table
    // gives.
    let regs = if caps.register_bytes() > regs.len() {
        match devices::map(found.registers, caps.register_bytes()) {
            Some(regs) => regs,
            None => return false,
        }
    } else {
        regs
    };
    let (scopes, displays) = scopes(found);
    let unit = if displays && !found.include_all {
        radix_klog::info!(
            "IOMMU: the unit at {:#x} translates only the display controller; left off, as no \
             graphics driver hands it memory yet",
            found.registers
        );
        None
    } else {
        match Unit::new(regs, Tables) {
            Ok(unit) => Some(unit),
            Err(err) => {
                radix_klog::warn!("IOMMU: the unit at {:#x} is not used: {err}", found.registers);
                None
            }
        }
    };
    let faults = unit.as_ref().map(|unit| SpinLock::new(unit.fault_records()));
    let unit = unit.map(|mut unit| {
        if let Some((_, message)) = devices::allocate(0, on_fault, index) {
            unit.set_fault_interrupt(message.address, message.data);
        }
        SpinLock::new(unit)
    });
    let remapper = Remapper {
        registers: found.registers,
        segment: found.segment,
        include_all: found.include_all,
        scopes,
        unit,
        faults,
        caps,
        next_domain: AtomicU32::new(1),
    };
    UNITS[index].set(remapper).is_ok()
}

/// Maps the memory the firmware reserved for devices (RMRRs) for them.
fn reserve(dmar: &Dmar<'_>) {
    for region in dmar.reserved() {
        for scope in region.scopes().filter(|scope| scope.kind == ScopeKind::Endpoint) {
            let Some((bus, device, function)) =
                resolve(region.segment, scope.start_bus, scope.path())
            else {
                continue;
            };
            let source = Address { segment: region.segment, bus, device, function };
            let len = region.limit - region.base + 1;
            if map_range(source, region.base, len) {
                radix_klog::info!(
                    "IOMMU: {source} keeps reaching {:#x}-{:#x}, which the firmware reserved for it",
                    region.base,
                    region.limit
                );
            } else {
                radix_klog::warn!(
                    "IOMMU: {source} could not be given {:#x}-{:#x}, which the firmware reserved \
                     for it",
                    region.base,
                    region.limit
                );
            }
        }
    }
}

/// Logs what unit `index` does, now that it translates.
fn log_unit(index: usize, remapper: &Remapper, version: (u8, u8), found_on: bool) {
    let caps = remapper.caps;
    let mut what = TextBuf::<48>::new();
    let _ = if remapper.include_all {
        write!(what, "every device on segment {}", remapper.segment)
    } else {
        write!(what, "{} devices", remapper.scopes.iter().flatten().count())
    };
    radix_klog::info!(
        "IOMMU: VT-d {}.{} unit {index} at {:#x} translates {what}: {}-level page tables, {} \
         domains, {} invalidation{}{}",
        version.0,
        version.1,
        remapper.registers,
        caps.levels().unwrap_or(0),
        caps.domains(),
        if caps.queued() { "queued" } else { "register" },
        if caps.caching_mode() { ", caching mode" } else { "" },
        if found_on { "; the firmware had left it on" } else { "" }
    );
}

/// The unit that translates `source`: one that names it, or else its
/// segment's catch-all.
fn remapper_of(source: Address) -> Option<usize> {
    let units = || UNITS.iter().map_while(Once::get).enumerate();
    units()
        .find(|(_, remapper)| !remapper.include_all && remapper.names(source))
        .or_else(|| {
            units().find(|(_, remapper)| remapper.include_all && remapper.segment == source.segment)
        })
        .map(|(index, _)| index)
}

/// `source`'s entry, if it has one.
fn find(source: Address) -> Option<&'static Device> {
    DEVICES.iter().map_while(Once::get).find(|device| device.source == source)
}

/// `source`'s entry, made with its domain, attached to its unit, if it
/// has none. `None` when there is no room or memory for it.
fn device(source: Address) -> Option<&'static Device> {
    if let Some(device) = find(source) {
        return Some(device);
    }
    let (added, id) = {
        let _adding = ADDING.lock_irq();
        if let Some(device) = find(source) {
            return Some(device);
        }
        let slot = DEVICES.iter().find(|slot| slot.get().is_none())?;
        let remapper = remapper_of(source);
        let mut id = None;
        let domain = match remapper.and_then(|index| UNITS[index].get()) {
            Some(Remapper { unit: Some(unit), next_domain, caps, .. }) => {
                let number = next_domain.load(Ordering::Relaxed);
                if number >= caps.domains() {
                    return None;
                }
                let mut unit = unit.lock_irq();
                let domain = unit.domain(number as u16)?;
                let devfn = (source.device << 3) | source.function;
                if unit.attach(source.bus, devfn, &domain).is_err() {
                    domain.destroy(unit.memory());
                    return None;
                }
                next_domain.store(number + 1, Ordering::Relaxed);
                id = Some(number);
                Some(domain)
            }
            _ => None,
        };
        let device = Device { source, remapper, domain: SpinLock::new(domain) };
        (slot.set(device).ok()?, id)
    };
    if let Some(id) = id {
        radix_klog::info!("IOMMU: {source} reaches only what is mapped for it (domain {id})");
    }
    Some(added)
}

/// What an unmapping cleared, for the unit to forget.
struct Cleared {
    runs: [(u64, u64); RUNS],
    count: usize,
    overflowed: bool,
}

impl Cleared {
    fn new() -> Cleared {
        Cleared { runs: [(0, 0); RUNS], count: 0, overflowed: false }
    }

    fn add(&mut self, address: u64, pages: u64) {
        if self.count == RUNS {
            self.overflowed = true;
        } else {
            self.runs[self.count] = (address, pages);
            self.count += 1;
        }
    }

    /// Has `unit` forget what was cleared in domain `domain`; `leaf` if
    /// no table went.
    fn flush(&self, unit: &mut Unit<Tables>, domain: u16, leaf: bool) -> Result<(), Refused> {
        if self.overflowed {
            return unit.flush(&[Flush::Domain(domain)]);
        }
        let mut batch = [Flush::All; 32];
        let mut count = 0;
        for &(start, pages) in &self.runs[..self.count] {
            for (address, order) in blocks(start, pages) {
                if count == batch.len() {
                    unit.flush(&batch)?;
                    count = 0;
                }
                batch[count] = Flush::Pages { domain, address, order, leaf };
                count += 1;
            }
        }
        if count > 0 { unit.flush(&batch[..count]) } else { Ok(()) }
    }
}

/// Tells the log, once, that a unit stopped invalidating.
fn stalled(err: Refused) {
    if !STALLED.swap(true, Ordering::Relaxed) {
        radix_klog::error!(
            "IOMMU: a unit stopped invalidating what it caches ({err}); devices may still reach \
             memory given back"
        );
    }
}

/// Runs `change` on `source`'s domain and unit. True if `source` is
/// translated by none: it reaches all memory.
fn with_domain(
    source: Address,
    change: impl FnOnce(&mut Domain, &mut Unit<Tables>) -> Result<bool, Refused>,
) -> Option<bool> {
    let device = device(source)?;
    let Some(index) = device.remapper else { return Some(true) };
    let Some(unit) = UNITS[index].get().and_then(|remapper| remapper.unit.as_ref()) else {
        return Some(true);
    };
    let result = {
        let mut domain = device.domain.lock_irq();
        let domain = domain.as_mut()?;
        let mut unit = unit.lock_irq();
        change(domain, &mut unit)
    };
    match result {
        Ok(mapped) => Some(mapped),
        Err(err) => {
            stalled(err);
            Some(false)
        }
    }
}

/// Maps the pages of `frames` for `source`, as `Devices::map_dma` says.
pub fn map(source: Address, frames: &[(u64, u32)]) -> bool {
    if !active() {
        return true;
    }
    with_domain(source, |domain, unit| {
        let mut cleared = Cleared::new();
        let mapped = domain.map(&Tables, frames, &mut |address, pages| cleared.add(address, pages));
        settle(
            domain,
            unit,
            mapped.is_ok(),
            &cleared,
            frames.iter().map(|&(p, l)| (p, u64::from(l))),
        )
    })
    .unwrap_or(false)
}

/// Maps `len` bytes from `start` for `source`, as [`map`], whether or
/// not translation is on yet.
pub fn map_range(source: Address, start: u64, len: u64) -> bool {
    with_domain(source, |domain, unit| {
        let mut cleared = Cleared::new();
        let mapped = domain.map_range(&Tables, start, len, &mut |address, pages| {
            cleared.add(address, pages);
        });
        settle(domain, unit, mapped.is_ok(), &cleared, core::iter::once((start, len)))
    })
    .unwrap_or(false)
}

/// After a mapping: has a unit in caching mode forget that the pages of
/// `ranges` were not mapped, or, if it failed, forget what it undid.
fn settle(
    domain: &mut Domain,
    unit: &mut Unit<Tables>,
    mapped: bool,
    cleared: &Cleared,
    ranges: impl Iterator<Item = (u64, u64)>,
) -> Result<bool, Refused> {
    if !mapped {
        cleared.flush(unit, domain.id(), false)?;
        domain.release(&Tables);
        return Ok(false);
    }
    if unit.caps().caching_mode() {
        // Such a unit caches what is not mapped too (VT-d 4.1, 6.1).
        let mut fresh = Cleared::new();
        for (start, len) in ranges {
            let first = start & !(PAGE - 1);
            let end = (start + len).next_multiple_of(PAGE);
            fresh.add(first, (end - first) / PAGE);
        }
        fresh.flush(unit, domain.id(), true)?;
    } else {
        // The write buffer, on a unit that has one.
        unit.flush(&[])?;
    }
    Ok(true)
}

/// Unmaps what [`map`] mapped for `source`, as `Devices::unmap_dma` says.
pub fn unmap(source: Address, frames: &[(u64, u32)]) {
    if !active() || find(source).is_none() {
        return;
    }
    let _ = with_domain(source, |domain, unit| {
        let mut cleared = Cleared::new();
        let tables = domain.unmap(&Tables, frames, &mut |address, pages| {
            cleared.add(address, pages);
        });
        // Tables the unit may still walk stay, if it would not forget.
        cleared.flush(unit, domain.id(), !tables)?;
        domain.release(&Tables);
        Ok(true)
    });
}

/// A unit's fault interrupt: logs and clears what it recorded.
fn on_fault(index: usize) {
    let Some(remapper) = UNITS[index].get() else { return };
    let Some(records) = &remapper.faults else { return };
    let mut recorded = [None; 8];
    let mut count = 0;
    let lost = records.lock_irq().read(|fault| {
        if count < recorded.len() {
            recorded[count] = Some(fault);
        }
        count += 1;
    });
    let now = time::now_ns();
    for fault in recorded.iter().flatten() {
        {
            let mut window = LOGGED.lock_irq();
            if now.saturating_sub(window.0) >= FAULT_WINDOW_NS {
                *window = (now, 0);
            }
            if window.1 >= FAULTS_LOGGED {
                break;
            }
            window.1 += 1;
        }
        let source = Address {
            segment: remapper.segment,
            bus: fault.bus(),
            device: fault.device(),
            function: fault.function(),
        };
        radix_klog::warn!(
            "IOMMU: {source} was stopped {} {:#x}: {}",
            if fault.write { "writing to" } else { "reading from" },
            fault.address,
            fault.why()
        );
    }
    if lost {
        radix_klog::warn!("IOMMU: unit {index} recorded more faults than it holds; some were lost");
    }
}
