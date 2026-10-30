// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel VT-d: the remapping units the ACPI DMAR table lists, through
//! `radix-vtd` (ADR 0164).
//!
//! A unit that translates only the display controller is left off, since
//! no graphics driver hands it memory yet. The memory the firmware
//! reserved for devices (RMRRs) is mapped for them before any unit
//! translates.

use core::fmt::Write as _;
use core::sync::atomic::AtomicU32;

use radix_acpi::{Dmar, DmarUnit, ScopeKind, Sdt};
use radix_hw::pci::Address;
use radix_klog::TextBuf;
use radix_sync::SpinLock;
use radix_vtd::{Caps, Unit};

use super::{Engine, Faults, MAX_UNITS, Remapper, Tables, UNITS, config};
use crate::devices;
use crate::memory::PAGE;

/// The most devices a unit names in its scopes.
const MAX_SCOPES: usize = 16;

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

/// What a VT-d unit translates: what its scopes name, or, with
/// `include_all`, every device of its segment no other unit names.
pub(super) struct Covers {
    pub(super) include_all: bool,
    scopes: [Option<Scope>; MAX_SCOPES],
}

impl Covers {
    pub(super) fn names(&self, source: Address) -> bool {
        let devfn = (source.device << 3) | source.function;
        self.scopes.iter().flatten().any(|scope| scope.names(source.bus, devfn))
    }
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

/// Takes over every unit the DMAR table `sdt` lists and turns translation
/// on; whether any unit translates.
pub(super) fn init(sdt: &Sdt<'_>) -> bool {
    let Ok(dmar) = Dmar::parse(sdt) else {
        radix_klog::warn!("IOMMU: the DMAR table is too short; DMA remapping is off");
        return false;
    };
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
        let Some(Engine::Vtd(unit)) = &remapper.engine else { continue };
        let (enabled, version, found_on, caps) = {
            let mut unit = unit.lock_irq();
            (unit.enable(), unit.version(), unit.found_on(), unit.caps())
        };
        if let Err(err) = enabled {
            radix_klog::warn!(
                "IOMMU: the unit at {:#x} would not translate: {err}",
                remapper.registers
            );
            continue;
        }
        on = true;
        log_unit(index, remapper, caps, version, found_on);
    }
    on
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
    let faults = unit.as_ref().map(|unit| Faults::Vtd(SpinLock::new(unit.fault_records())));
    let engine = unit.map(|mut unit| {
        if let Some((_, message)) = devices::allocate(0, super::on_fault, index) {
            unit.set_fault_interrupt(message.address, message.data);
        }
        Engine::Vtd(SpinLock::new(unit))
    });
    let remapper = Remapper {
        registers: found.registers,
        segment: found.segment,
        covers: super::Covers::Vtd(Covers { include_all: found.include_all, scopes }),
        engine,
        faults,
        domains: caps.domains(),
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
            super::reserved(source, region.base, region.limit);
        }
    }
}

/// Logs what unit `index` does, now that it translates.
fn log_unit(index: usize, remapper: &Remapper, caps: Caps, version: (u8, u8), found_on: bool) {
    let super::Covers::Vtd(covers) = &remapper.covers else { return };
    let mut what = TextBuf::<48>::new();
    let _ = if covers.include_all {
        write!(what, "every device on segment {}", remapper.segment)
    } else {
        write!(what, "{} devices", covers.scopes.iter().flatten().count())
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
