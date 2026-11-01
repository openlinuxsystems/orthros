// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The IOMMU: DMA remapping on Intel VT-d, through `radix-vtd` (ADR
//! 0164), and on AMD-Vi, through `radix-amdvi` (ADR 0165), their domains'
//! page tables `radix-iopt`'s.
//!
//! [`init`] takes over the units the ACPI DMAR table lists ([`vtd`]), or
//! the IOMMUs the IVRS lists ([`amd`]), at the end of `Platform::init`,
//! before any driver starts a device. Each PCI function that DMA memory
//! is allocated or mapped for gets its own domain, attached when the
//! function is first named, and reaches only its DMA memory, the pages
//! mapped for its requests in flight, and the memory the firmware
//! reserved for it, each at its physical address: a device address is a
//! physical address, behind a unit or not. A function whose DMA nothing
//! asked for reaches no memory at all.
//!
//! Invalidation is strict: when [`unmap`] returns, the device can no
//! longer reach what it gave back. `iommu=off` leaves every unit off.
//! Faults are logged, at most [`FAULTS_LOGGED`] in each
//! [`FAULT_WINDOW_NS`].
//!
//! Interrupts are remapped too (ADR 0166) where every unit can and every
//! I/O APIC is one a unit names: each interrupt vector a device or an I/O
//! APIC input is given ([`remap`], [`remap_ioapic`]) gets an interrupt
//! entry of the same number, which only that device's requests match,
//! and every other interrupt is blocked. A vector given back loses its
//! entry ([`unremap`]).
//!
//! Locks: a device's domain, then its unit, then the frame allocator's.
//! The fault interrupt takes only the lock of the unit's fault records or
//! event log, registers of their own, so it never waits for a change to
//! the tables; nothing is logged with a lock held.

mod amd;
mod vtd;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use radix_hw::MsiMessage;
use radix_hw::pci::Address;
use radix_mm::Owner;
use radix_sync::{Once, SpinLock};

use crate::memory;
use crate::{acpi, apic, cpu, devices, time};

/// The most units used.
const MAX_UNITS: usize = 8;
/// The most PCI functions with domains.
const MAX_DEVICES: usize = 128;
/// Faults logged in each window.
pub const FAULTS_LOGGED: u32 = 10;
/// The window, in nanoseconds.
pub const FAULT_WINDOW_NS: u64 = 5_000_000_000;

/// Table memory: kernel pages, reached through the direct map.
#[derive(Clone, Copy, Debug)]
struct Tables;

impl radix_iopt::Memory for Tables {
    fn alloc_page(&self) -> Option<u64> {
        memory::alloc_pages(1, false, Owner::Kernel)
    }

    fn free_page(&self, phys: u64) {
        memory::free_pages(phys, 1, Owner::Kernel);
    }

    fn entries(&self, phys: u64) -> &[AtomicU64; 512] {
        // SAFETY: the engines pass only pages `alloc_page` or `alloc_run`
        // gave them and have not given back: zeroed RAM, mapped by the
        // direct map at `phys_to_virt`, page-aligned, and touched only
        // through atomics.
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

impl radix_amdvi::Tables for Tables {
    fn alloc_run(&self, count: usize) -> Option<u64> {
        memory::alloc_pages(count as u64, false, Owner::Kernel)
    }

    fn free_run(&self, phys: u64, count: usize) {
        memory::free_pages(phys, count as u64, Owner::Kernel);
    }
}

/// A unit's engine.
enum Engine {
    Vtd(SpinLock<radix_vtd::Unit<Tables>>),
    Amd(SpinLock<radix_amdvi::Unit<Tables>>),
}

/// What a unit's fault interrupt reads.
enum Faults {
    Vtd(SpinLock<radix_vtd::FaultRecords>),
    Amd(SpinLock<radix_amdvi::EventLog<Tables>>),
}

/// A device's domain, in its unit's format.
enum Domain {
    Vtd(radix_vtd::Domain),
    Amd(radix_amdvi::Domain),
}

/// Why a unit stopped invalidating.
enum Stalled {
    Vtd(radix_vtd::Refused),
    Amd(radix_amdvi::Refused),
}

impl core::fmt::Display for Stalled {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Stalled::Vtd(err) => err.fmt(f),
            Stalled::Amd(err) => err.fmt(f),
        }
    }
}

/// Which devices a unit translates.
enum Covers {
    Vtd(vtd::Covers),
    Amd(amd::Covers),
}

/// A unit and what it translates.
struct Remapper {
    registers: u64,
    segment: u16,
    covers: Covers,
    /// `None` for a unit left off: its devices reach all memory.
    engine: Option<Engine>,
    /// What its fault interrupt reads.
    faults: Option<Faults>,
    /// How many domain numbers it has.
    domains: u32,
    /// The next domain number; 0 is never used, as VT-d's caching mode
    /// wants.
    next_domain: AtomicU32,
}

impl Remapper {
    /// Whether it names `source` itself, rather than as a catch-all.
    fn names(&self, source: Address) -> bool {
        self.segment == source.segment
            && match &self.covers {
                Covers::Vtd(covers) => covers.names(source),
                Covers::Amd(covers) => covers.names(source),
            }
    }

    /// Whether it translates every device of its segment no other names.
    fn catches_all(&self) -> bool {
        match &self.covers {
            Covers::Vtd(covers) => covers.include_all,
            Covers::Amd(_) => false,
        }
    }

    /// The requester ID `source`'s requests carry through it.
    fn requester(&self, source: Address) -> u16 {
        match &self.covers {
            Covers::Vtd(_) => requester_id(source),
            Covers::Amd(covers) => covers.requester(source),
        }
    }

    /// The requester ID the interrupts of the I/O APIC `id` carry, if it
    /// names that I/O APIC.
    fn ioapic(&self, id: u8) -> Option<u16> {
        match &self.covers {
            Covers::Vtd(covers) => covers.ioapic(id),
            Covers::Amd(covers) => covers.ioapic(id),
        }
    }
}

/// A function with DMA, and its domain.
struct Device {
    source: Address,
    /// The unit it is behind, if one translates it.
    remapper: Option<usize>,
    /// The requester ID its requests carry through that unit.
    requester: u16,
    /// The entry whose domain it shares, as its requests carry the same
    /// requester ID; then its own is `None`.
    shares: Option<usize>,
    domain: SpinLock<Option<Domain>>,
    /// A program drives it ([`isolate`]): no function added after it
    /// shares its domain.
    alone: AtomicBool,
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
/// The interrupt entries given out, by vector: [`REMAPPED`], the unit's
/// number from bit 16 and the requester ID in the low 16 bits; 0 for none.
static REMAPS: [AtomicU32; 256] = [const { AtomicU32::new(0) }; 256];
/// A vector of [`REMAPS`] has an entry.
const REMAPPED: u32 = 1 << 31;

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
    let window = radix_acpi::mcfg_entries(mcfg)
        .find(|w| w.segment == segment && (w.start_bus..=w.end_bus).contains(&bus))?;
    let offset = (u64::from(bus) << 20) | (u64::from(device) << 15) | (u64::from(function) << 12);
    let config = devices::map(window.base + offset, 4096)?;
    (config.read16(0) != 0xffff).then_some(config)
}

/// The PCI function a requester ID names on `segment`.
fn requester(segment: u16, id: u16) -> Address {
    Address {
        segment,
        bus: (id >> 8) as u8,
        device: (id >> 3) as u8 & 0x1f,
        function: id as u8 & 7,
    }
}

/// `source`'s requester ID: its bus, device and function.
fn requester_id(source: Address) -> u16 {
    (u16::from(source.bus) << 8) | (u16::from(source.device) << 3) | u16::from(source.function)
}

/// Takes over every unit the DMAR or IVRS table lists and turns
/// translation on, unless `off`. Logs what it did.
pub fn init(off: bool) {
    let Some(acpi) = acpi::get() else { return };
    let dmar = acpi.tables.find(&acpi::PhysicalMemory, b"DMAR");
    let ivrs = acpi.tables.find(&acpi::PhysicalMemory, b"IVRS");
    if dmar.is_none() && ivrs.is_none() {
        return;
    }
    if off {
        radix_klog::info!(
            "IOMMU: DMA remapping left off by the iommu=off flag; every device reaches all memory"
        );
        return;
    }
    let on = match (dmar, ivrs) {
        (Some(dmar), _) => vtd::init(&dmar),
        (None, Some(ivrs)) => amd::init(&ivrs),
        (None, None) => false,
    };
    ACTIVE.store(on, Ordering::Release);
}
/// Maps `base` to `limit`, memory the firmware reserved for `source`, for
/// it, and logs whether it could.
fn reserved(source: Address, base: u64, limit: u64) {
    if map_range(source, base, limit - base + 1) {
        radix_klog::info!(
            "IOMMU: {source} keeps reaching {base:#x}-{limit:#x}, which the firmware reserved for \
             it"
        );
    } else {
        radix_klog::warn!(
            "IOMMU: {source} could not be given {base:#x}-{limit:#x}, which the firmware reserved \
             for it"
        );
    }
}

/// The unit that translates `source`: one that names it, or else its
/// segment's catch-all.
fn remapper_of(source: Address) -> Option<usize> {
    let units = || UNITS.iter().map_while(Once::get).enumerate();
    units()
        .find(|(_, remapper)| !remapper.catches_all() && remapper.names(source))
        .or_else(|| {
            units()
                .find(|(_, remapper)| remapper.catches_all() && remapper.segment == source.segment)
        })
        .map(|(index, _)| index)
}

/// `source`'s entry, if it has one.
fn find(source: Address) -> Option<&'static Device> {
    DEVICES.iter().map_while(Once::get).find(|device| device.source == source)
}

/// A new domain numbered `number` on `engine`, attached to the requester
/// ID `requester`.
fn attach(engine: &Engine, number: u32, requester: u16) -> Option<Domain> {
    match engine {
        Engine::Vtd(unit) => {
            let mut unit = unit.lock_irq();
            let domain = unit.domain(number as u16)?;
            if unit.attach((requester >> 8) as u8, requester as u8, &domain).is_err() {
                domain.destroy(unit.memory());
                return None;
            }
            Some(Domain::Vtd(domain))
        }
        Engine::Amd(unit) => {
            let mut unit = unit.lock_irq();
            let domain = unit.domain(number as u16)?;
            if unit.attach(requester, &domain).is_err() {
                domain.destroy(unit.memory());
                return None;
            }
            Some(Domain::Amd(domain))
        }
    }
}

/// How a new entry reaches memory.
enum Added {
    /// Through a domain of its own, with this number.
    Domain(u32),
    /// Through the domain of this function.
    Shares(Address),
}

/// `source`'s entry, made with its domain, attached to its unit, if it
/// has none. `None` when there is no room or memory for it.
fn device(source: Address) -> Option<&'static Device> {
    if let Some(device) = find(source) {
        return Some(device);
    }
    let (device, how) = {
        let _adding = ADDING.lock_irq();
        if let Some(device) = find(source) {
            return Some(device);
        }
        let slot = DEVICES.iter().find(|slot| slot.get().is_none())?;
        let remapper = remapper_of(source);
        let unit = remapper.and_then(|index| UNITS[index].get());
        let requester = unit.map_or_else(|| requester_id(source), |unit| unit.requester(source));
        let shares = remapper.and_then(|index| {
            DEVICES.iter().map_while(Once::get).position(|other| {
                other.remapper == Some(index)
                    && other.requester == requester
                    && other.shares.is_none()
            })
        });
        let mut how = None;
        let domain = match (unit, shares) {
            (Some(Remapper { engine: Some(_), .. }), Some(at)) => {
                how = DEVICES[at].get().map(|other| Added::Shares(other.source));
                None
            }
            (Some(Remapper { engine: Some(engine), next_domain, domains, .. }), None) => {
                let number = next_domain.load(Ordering::Relaxed);
                if number >= *domains {
                    return None;
                }
                let domain = attach(engine, number, requester)?;
                next_domain.store(number + 1, Ordering::Relaxed);
                how = Some(Added::Domain(number));
                Some(domain)
            }
            _ => None,
        };
        // A function a program drives shares its domain with none.
        if let Some(owner) = shares.and_then(|at| DEVICES[at].get())
            && owner.alone.load(Ordering::Acquire)
        {
            radix_klog::warn!(
                "IOMMU: {source} is given no memory: its requests carry the ID of {}, which a \
                 program drives",
                owner.source
            );
            return None;
        }
        let shares = shares.filter(|_| how.is_some());
        let device = Device {
            source,
            remapper,
            requester,
            shares,
            domain: SpinLock::new(domain),
            alone: AtomicBool::new(false),
        };
        (slot.set(device).ok()?, how)
    };
    match how {
        Some(Added::Domain(id)) => {
            radix_klog::info!("IOMMU: {source} reaches only what is mapped for it (domain {id})");
        }
        Some(Added::Shares(other)) => radix_klog::info!(
            "IOMMU: {source} reaches what is mapped for {other}, as its requests carry the same ID"
        ),
        None => return Some(device),
    }
    amd::reserve_for(source);
    Some(device)
}

/// Gives `source` a domain no other function shares, as
/// `Devices::isolate` says: only if a unit translates its DMA through a
/// domain of its own and remaps its interrupts, and no function added so
/// far carries its requester ID. From then on [`device`] refuses a
/// function that would share it.
pub fn isolate(source: Address) -> bool {
    if !active() {
        return false;
    }
    let Some(device) = device(source) else { return false };
    let Some(index) = device.remapper else { return false };
    let Some(remapper) = UNITS[index].get() else { return false };
    let remaps = match &remapper.engine {
        Some(Engine::Vtd(unit)) => unit.lock_irq().remapping(),
        Some(Engine::Amd(unit)) => unit.lock_irq().remapping(),
        None => false,
    };
    if !remaps || device.shares.is_some() || device.domain.lock_irq().is_none() {
        return false;
    }
    let _adding = ADDING.lock_irq();
    let shared = DEVICES.iter().map_while(Once::get).any(|other| {
        other.source != source && other.remapper == Some(index) && other.requester == device.requester
    });
    if shared {
        return false;
    }
    device.alone.store(true, Ordering::Release);
    true
}

/// Tells the log, once, that a unit stopped invalidating.
fn stalled(err: &Stalled) {
    if !STALLED.swap(true, Ordering::Relaxed) {
        radix_klog::error!(
            "IOMMU: a unit stopped invalidating what it caches ({err}); devices may still reach \
             memory given back"
        );
    }
}

/// A change to a domain, whichever its unit's format: `map`, `map_range`
/// or `unmap` of `radix-iopt`.
enum Change<'a> {
    Map(&'a [(u64, u32)]),
    Range(u64, u64),
    Unmap(&'a [(u64, u32)]),
}

impl Change<'_> {
    fn apply<F: radix_iopt::Format, U: radix_iopt::Invalidate>(
        &self,
        domain: &mut radix_iopt::Domain<F>,
        unit: &mut U,
    ) -> Result<bool, U::Error> {
        match *self {
            Change::Map(frames) => radix_iopt::map(domain, unit, &Tables, frames),
            Change::Range(start, len) => radix_iopt::map_range(domain, unit, &Tables, start, len),
            Change::Unmap(frames) => {
                radix_iopt::unmap(domain, unit, &Tables, frames).map(|()| true)
            }
        }
    }
}

/// Makes `change` to `source`'s domain and has its unit see it. True if
/// it was made, or if `source` is translated by none: it reaches all
/// memory.
fn change(source: Address, change: &Change<'_>) -> bool {
    let Some(device) = device(source) else { return false };
    let Some(index) = device.remapper else { return true };
    let device = match device.shares.and_then(|at| DEVICES[at].get()) {
        Some(owner) => owner,
        None => device,
    };
    let Some(engine) = UNITS[index].get().and_then(|remapper| remapper.engine.as_ref()) else {
        return true;
    };
    let result = {
        let mut domain = device.domain.lock_irq();
        match (domain.as_mut(), engine) {
            (Some(Domain::Vtd(domain)), Engine::Vtd(unit)) => {
                change.apply(domain, &mut *unit.lock_irq()).map_err(Stalled::Vtd)
            }
            (Some(Domain::Amd(domain)), Engine::Amd(unit)) => {
                change.apply(domain, &mut *unit.lock_irq()).map_err(Stalled::Amd)
            }
            _ => Ok(false),
        }
    };
    result.unwrap_or_else(|err| {
        stalled(&err);
        false
    })
}

/// Maps the pages of `frames` for `source`, as `Devices::map_dma` says.
pub fn map(source: Address, frames: &[(u64, u32)]) -> bool {
    !active() || change(source, &Change::Map(frames))
}

/// Maps `len` bytes from `start` for `source`, as [`map`], whether or
/// not translation is on yet.
pub fn map_range(source: Address, start: u64, len: u64) -> bool {
    change(source, &Change::Range(start, len))
}

/// Unmaps what [`map`] mapped for `source`, as `Devices::unmap_dma` says.
pub fn unmap(source: Address, frames: &[(u64, u32)]) {
    if active() && find(source).is_some() {
        change(source, &Change::Unmap(frames));
    }
}

/// Whether every I/O APIC is one a unit with an engine names, so that its
/// inputs can be remapped; else the ID of one that is not.
fn ioapics_named() -> Result<(), u8> {
    let mut missing = None;
    apic::ioapics(|io| {
        let named = UNITS
            .iter()
            .map_while(Once::get)
            .any(|remapper| remapper.engine.is_some() && remapper.ioapic(io.id).is_some());
        if !named && missing.is_none() {
            missing = Some(io.id);
        }
    });
    missing.map_or(Ok(()), Err)
}

/// How a device raises an interrupt it is given.
pub enum Remap {
    /// As it would with no IOMMU: no unit remaps its interrupts.
    Off,
    /// Through its interrupt entry, with this message.
    To(MsiMessage),
    /// Its unit remaps interrupts and could not give it an entry, so it
    /// cannot raise the interrupt at all.
    Failed,
}

/// Gives `source` an interrupt entry for `vector`, sent to the CPU whose
/// APIC ID is `destination`, or to `fallback` if the entry cannot name
/// it, when its unit remaps interrupts.
pub fn remap(source: Address, vector: u8, destination: u32, fallback: u32) -> Remap {
    let Some(index) = remapper_of(source) else { return Remap::Off };
    let Some(remapper) = UNITS[index].get() else { return Remap::Off };
    let requester = remapper.requester(source);
    let result = match &remapper.engine {
        Some(Engine::Vtd(unit)) => {
            let mut unit = unit.lock_irq();
            if !unit.remapping() {
                return Remap::Off;
            }
            let to =
                if destination <= 0xff || unit.names_every_cpu() { destination } else { fallback };
            unit.remap(vector, vector, to, requester, false).map_err(Stalled::Vtd).map(|()| {
                let (address, data) = radix_vtd::remappable(vector);
                MsiMessage { address, data }
            })
        }
        Some(Engine::Amd(unit)) => {
            let mut unit = unit.lock_irq();
            if !unit.remapping() {
                return Remap::Off;
            }
            let to = if destination <= 0xff { destination } else { fallback } as u8;
            // The message is the one it would send untranslated: its data
            // picks the entry.
            unit.remap(requester, vector, to).map_err(Stalled::Amd).map(|()| MsiMessage {
                address: 0xfee0_0000 | (u64::from(to) << 12),
                data: u32::from(vector),
            })
        }
        None => return Remap::Off,
    };
    match result {
        Ok(message) => {
            let given = REMAPPED | ((index as u32) << 16) | u32::from(requester);
            REMAPS[usize::from(vector)].store(given, Ordering::Release);
            Remap::To(message)
        }
        Err(err) => {
            radix_klog::warn!("IOMMU: {source} could not be given interrupt {vector:#x}: {err}");
            Remap::Failed
        }
    }
}

/// How an I/O APIC's redirection entry raises an interrupt it is given.
pub enum IoApicEntry {
    /// As it would with no IOMMU, its destination in the high word.
    Plain,
    /// Through an interrupt entry, with this high word.
    Remapped(u32),
    /// Its unit remaps interrupts and could not give it an entry.
    Failed,
}

/// Gives the I/O APIC `id` an interrupt entry for `vector`, sent to the
/// CPU whose APIC ID is `destination` (else `fallback`, as [`remap`]),
/// level-triggered if `level`, when its unit remaps interrupts. Its
/// redirection entry keeps `vector` in its low word: on AMD-Vi that picks
/// the entry, and on VT-d the entry's handle is the vector too.
pub fn remap_ioapic(
    id: u8,
    vector: u8,
    destination: u32,
    fallback: u32,
    level: bool,
) -> IoApicEntry {
    let found = UNITS
        .iter()
        .map_while(Once::get)
        .enumerate()
        .find_map(|(index, remapper)| Some((index, remapper, remapper.ioapic(id)?)));
    let Some((index, remapper, requester)) = found else { return IoApicEntry::Plain };
    let result = match &remapper.engine {
        Some(Engine::Vtd(unit)) => {
            let mut unit = unit.lock_irq();
            if !unit.remapping() {
                return IoApicEntry::Plain;
            }
            let to =
                if destination <= 0xff || unit.names_every_cpu() { destination } else { fallback };
            // The remappable format (bit 48) with the handle from bit 49.
            let high = (1 << 16) | (u32::from(vector) << 17);
            unit.remap(vector, vector, to, requester, level).map(|()| high).map_err(Stalled::Vtd)
        }
        Some(Engine::Amd(unit)) => {
            let mut unit = unit.lock_irq();
            if !unit.remapping() {
                return IoApicEntry::Plain;
            }
            let to = if destination <= 0xff { destination } else { fallback } as u8;
            let high = u32::from(to) << 24;
            unit.remap(requester, vector, to).map(|()| high).map_err(Stalled::Amd)
        }
        None => return IoApicEntry::Plain,
    };
    match result {
        Ok(high) => {
            let given = REMAPPED | ((index as u32) << 16) | u32::from(requester);
            REMAPS[usize::from(vector)].store(given, Ordering::Release);
            IoApicEntry::Remapped(high)
        }
        Err(err) => {
            radix_klog::warn!(
                "IOMMU: I/O APIC {id} could not be given interrupt {vector:#x}: {err}"
            );
            IoApicEntry::Failed
        }
    }
}

/// Takes away the interrupt entry of `vector`, if it has one: once this
/// returns, the device it was given to can no longer raise it.
pub fn unremap(vector: u8) {
    let given = REMAPS[usize::from(vector)].swap(0, Ordering::AcqRel);
    if given & REMAPPED == 0 {
        return;
    }
    let index = ((given >> 16) & 0xff) as usize;
    let requester = given as u16;
    let Some(engine) = UNITS.get(index).and_then(Once::get).and_then(|r| r.engine.as_ref()) else {
        return;
    };
    let result = match engine {
        Engine::Vtd(unit) => unit.lock_irq().unremap(vector).map_err(Stalled::Vtd),
        Engine::Amd(unit) => unit.lock_irq().unremap(requester, vector).map_err(Stalled::Amd),
    };
    if let Err(err) = result {
        stalled(&err);
    }
}

/// A fault a unit reported: the device, what it tried, and why it was
/// stopped.
#[derive(Clone, Copy)]
struct Fault {
    source: Option<Address>,
    address: u64,
    write: bool,
    /// It raised an interrupt, rather than reaching memory.
    interrupt: bool,
    why: &'static str,
}

/// A unit's fault interrupt: logs and clears what it recorded.
fn on_fault(index: usize) {
    let Some(remapper) = UNITS[index].get() else { return };
    let mut recorded = [None; 8];
    let mut count = 0;
    let mut keep = |fault: Fault| {
        if count < recorded.len() {
            recorded[count] = Some(fault);
        }
        count += 1;
    };
    let lost = match &remapper.faults {
        Some(Faults::Vtd(records)) => records.lock_irq().read(|fault| {
            keep(Fault {
                source: Some(requester(remapper.segment, fault.source)),
                address: fault.address,
                write: fault.write,
                interrupt: fault.interrupt(),
                why: fault.why(),
            });
        }),
        Some(Faults::Amd(log)) => log.lock_irq().read(|event| {
            keep(Fault {
                source: event.device.map(|id| requester(remapper.segment, id)),
                address: event.address,
                write: event.write,
                interrupt: event.interrupt(),
                why: event.why(),
            });
        }),
        None => return,
    };
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
        match fault.source {
            Some(source) if fault.interrupt => {
                radix_klog::warn!(
                    "IOMMU: {source} was stopped raising an interrupt: {}",
                    fault.why
                );
            }
            Some(source) => radix_klog::warn!(
                "IOMMU: {source} was stopped {} {:#x}: {}",
                if fault.write { "writing to" } else { "reading from" },
                fault.address,
                fault.why
            ),
            None => {
                radix_klog::warn!("IOMMU: unit {index}: {} (at {:#x})", fault.why, fault.address);
            }
        }
    }
    if lost {
        radix_klog::warn!("IOMMU: unit {index} recorded more faults than it holds; some were lost");
    }
}
