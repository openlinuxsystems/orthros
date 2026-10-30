// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! AMD-Vi: the IOMMUs the ACPI IVRS table lists, through `radix-amdvi`
//! (ADR 0165).
//!
//! An IOMMU is a PCI function of its own. The IVRS gives its registers,
//! its function, where the function's IOMMU capability block is, and the
//! device IDs it translates; the function's MSI capability carries the
//! interrupt for its event log. An IOMMU translates from when it is taken
//! over, every device blocked until its domain is attached. The memory
//! the firmware keeps for devices (IVMD blocks) is mapped for each device
//! it names when the device's domain is made, and the domains of the
//! devices a block names by ID are made right after the takeover.

use core::sync::atomic::AtomicU32;

use radix_acpi::{Ivrs, IvrsDevices, IvrsIommu, Sdt};
use radix_amdvi::{Invalidate as _, LEVELS, Setup, Unit};
use radix_hw::Mmio;
use radix_hw::pci::Address;
use radix_sync::{Once, SpinLock};

use super::{Engine, Faults, MAX_UNITS, Remapper, Tables, UNITS, config, requester, requester_id};
use crate::devices;

/// The bytes of an IOMMU's registers, its ring pointers among them.
const REGISTER_BYTES: usize = 0x4000;
/// The domain numbers an IOMMU has.
const DOMAINS: u32 = 1 << 16;
/// The capability ID of an IOMMU's capability block.
const CAP_IOMMU: u8 = 0x0f;
/// The capability ID of MSI.
const CAP_MSI: u8 = 0x05;
/// The capability header's NpCache bit: the IOMMU caches entries that
/// are not present.
const NP_CACHE: u32 = 1 << 26;
/// The most device IDs of one IVMD block whose domains are made at the
/// takeover; the others' are made when a driver first names them.
const EAGER_IDS: u32 = 64;

/// The IVRS, for the memory it keeps for devices.
static IVRS: Once<Ivrs<'static>> = Once::new();

/// What an IOMMU translates: the device IDs its IVHD block names.
pub(super) struct Covers {
    iommu: IvrsIommu<'static>,
}

impl Covers {
    pub(super) fn names(&self, source: Address) -> bool {
        self.iommu.translates(requester_id(source)).is_some()
    }

    /// The device ID `source`'s requests carry: its own, or the alias its
    /// IVHD block gives it, as for a device behind a PCIe-to-PCI bridge.
    pub(super) fn requester(&self, source: Address) -> u16 {
        let id = requester_id(source);
        self.iommu.translates(id).unwrap_or(id)
    }

    /// How many device IDs it names.
    fn count(&self) -> u32 {
        self.iommu
            .devices()
            .map(|devices| match devices {
                IvrsDevices::All => 1 << 16,
                IvrsDevices::Range { first, last, .. } => u32::from(last.saturating_sub(first)) + 1,
                IvrsDevices::Special { .. } | IvrsDevices::Namespace(_) => 0,
            })
            .sum()
    }
}

/// Takes over every IOMMU the IVRS table `sdt` lists, each translating
/// from then on with every device blocked, and gives the devices the
/// memory IVMD blocks keep for them; whether any IOMMU translates.
pub(super) fn init(sdt: &Sdt<'static>) -> bool {
    let Ok(ivrs) = Ivrs::parse(sdt) else {
        radix_klog::warn!("IOMMU: the IVRS table is too short; DMA remapping is off");
        return false;
    };
    let ivrs = *IVRS.call_once(|| ivrs);
    let mut count = 0;
    for found in ivrs.iommus() {
        if count == MAX_UNITS {
            radix_klog::warn!(
                "IOMMU: more than {MAX_UNITS} IOMMUs; the one at {:#x} is left off",
                found.registers
            );
            continue;
        }
        if take(found, count) {
            count += 1;
        }
    }
    reserve(&ivrs);
    let mut on = false;
    for (index, slot) in UNITS.iter().enumerate() {
        let Some(remapper) = slot.get() else { break };
        let Some(Engine::Amd(unit)) = &remapper.engine else { continue };
        let (found_on, caches_absent) = {
            let unit = unit.lock_irq();
            (unit.found_on(), unit.caches_absent())
        };
        on = true;
        log_unit(index, remapper, found_on, caches_absent);
    }
    on
}

/// Takes over the IOMMU `found` as unit `index`; false if it was not used.
fn take(found: IvrsIommu<'static>, index: usize) -> bool {
    let Some(regs) = devices::map(found.registers, REGISTER_BYTES) else {
        radix_klog::warn!(
            "IOMMU: the registers of the IOMMU at {:#x} could not be mapped",
            found.registers
        );
        return false;
    };
    let own = requester(found.segment, found.device_id);
    let config = config(own.segment, own.bus, own.device, own.function);
    let header = config.as_ref().map(|config| config.read32(usize::from(found.capability)));
    let unit = match header {
        Some(header) if header & 0xff == u32::from(CAP_IOMMU) => {
            let setup = Setup {
                flags: found.flags,
                last_device: found.last_device(),
                caches_absent: header & NP_CACHE != 0,
            };
            match Unit::new(regs, Tables, setup) {
                Ok(unit) => Some(unit),
                Err(err) => {
                    radix_klog::warn!(
                        "IOMMU: the IOMMU at {:#x} is not used: {err}",
                        found.registers
                    );
                    None
                }
            }
        }
        _ => {
            radix_klog::warn!(
                "IOMMU: the IOMMU at {:#x} has no capability block at {own}; it is not used",
                found.registers
            );
            None
        }
    };
    let faults = unit.as_ref().map(|unit| Faults::Amd(SpinLock::new(unit.event_log())));
    let engine = unit.map(|mut unit| {
        let msi = config.as_ref().and_then(|config| Some((config, msi(config).next()?)));
        if let Some((config, _)) = msi
            && let Some((_, message)) = devices::allocate(0, super::on_fault, index)
            && point_msi(config, message.address, message.data)
        {
            unit.interrupt_on_events();
        }
        Engine::Amd(SpinLock::new(unit))
    });
    let remapper = Remapper {
        registers: found.registers,
        segment: found.segment,
        covers: super::Covers::Amd(Covers { iommu: found }),
        engine,
        faults,
        domains: DOMAINS,
        next_domain: AtomicU32::new(1),
    };
    UNITS[index].set(remapper).is_ok()
}

/// The offsets of the MSI capabilities of the function `config`.
fn msi(config: &Mmio) -> impl Iterator<Item = usize> + '_ {
    let listed = config.read16(0x06) & (1 << 4) != 0;
    let mut at = if listed { usize::from(config.read8(0x34) & 0xfc) } else { 0 };
    // A list that loops ends after as many as configuration space holds.
    let mut left = 48;
    core::iter::from_fn(move || {
        while at >= 0x40 && left > 0 {
            left -= 1;
            let here = at;
            at = usize::from(config.read8(here + 1) & 0xfc);
            if config.read8(here) == CAP_MSI {
                return Some(here);
            }
        }
        None
    })
}

/// Points an MSI capability of the IOMMU's function `config` at
/// `address` and `data`, turns it on, and lets the function write
/// upstream; false if no capability takes it.
fn point_msi(config: &Mmio, address: u64, data: u32) -> bool {
    for at in msi(config) {
        let control = config.read16(at + 2);
        config.write32(at + 4, address as u32);
        if control & (1 << 7) != 0 {
            config.write32(at + 8, (address >> 32) as u32);
            config.write16(at + 12, data as u16);
        } else {
            config.write16(at + 8, data as u16);
        }
        // One message, on.
        config.write16(at + 2, (control & !(7 << 4)) | 1);
        if config.read16(at + 2) & 1 != 0 {
            // An MSI is a memory write, which bus mastering lets through.
            config.write16(0x04, config.read16(0x04) | (1 << 2));
            return true;
        }
    }
    false
}

/// Makes the domains of the devices that IVMD blocks name by ID, so that
/// each reaches its memory again right after the takeover.
fn reserve(ivrs: &Ivrs<'static>) {
    for region in ivrs.memory() {
        let Some((first, last)) = region.devices else { continue };
        if last < first || u32::from(last - first) >= EAGER_IDS {
            continue;
        }
        for id in first..=last {
            let source = requester(region.segment, id);
            if config(source.segment, source.bus, source.device, source.function).is_some() {
                // Making it maps its memory (`reserve_for`).
                let _ = super::device(source);
            }
        }
    }
}

/// Maps for `source`, whose domain was just made, the memory IVMD blocks
/// keep for it.
pub(super) fn reserve_for(source: Address) {
    let Some(ivrs) = IVRS.get() else { return };
    let id = requester_id(source);
    for region in ivrs.memory() {
        let names = region.devices.is_none_or(|(first, last)| {
            region.segment == source.segment && (first..=last).contains(&id)
        });
        let reaches = region.readable() || region.writable() || region.excluded();
        if names && reaches && region.len > 0 {
            super::reserved(source, region.base, region.base + (region.len - 1));
        }
    }
}

/// Logs what unit `index` does, now that it translates.
fn log_unit(index: usize, remapper: &Remapper, found_on: bool, caches_absent: bool) {
    let super::Covers::Amd(covers) = &remapper.covers else { return };
    radix_klog::info!(
        "IOMMU: AMD-Vi unit {index} at {:#x} translates {} device IDs: {LEVELS}-level page \
         tables, {DOMAINS} domains, command buffer invalidation{}{}",
        remapper.registers,
        covers.count(),
        if caches_absent { ", caching absent entries" } else { "" },
        if found_on { "; the firmware had left it on" } else { "" }
    );
}
