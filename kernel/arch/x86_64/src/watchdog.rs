// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chipset's hardware watchdog (Phase 11, ADRs 0143 and 0167),
//! through `radix-watchdog`: QEMU's i6300esb, Intel's TCO timer from the
//! ICH9 on, or the watchdog of AMD's FCH.
//!
//! [`find`] gives the crate bus 0 of PCI segment 0, where chipsets put
//! them, and [`Chipset`]: device memory that is not RAM, and the I/O ports
//! of chipset functions and devices.

use radix_hal::Watchdog;
use radix_hw::Mmio;
use radix_hw::pci;
use radix_sync::Once;
use radix_watchdog::Platform;

use crate::io::{inb, inw, outb, outw};
use crate::{acpi, devices, memory};

/// The watchdog found.
static DEVICE: Once<radix_watchdog::Watchdog<Chipset>> = Once::new();

/// Finds the watchdog and stops it.
pub fn find() -> Option<Watchdog> {
    let mcfg = acpi::get()?.mcfg.as_ref()?;
    let window = radix_acpi::mcfg_entries(mcfg).find(|w| w.segment == 0 && w.start_bus == 0)?;
    let bus = devices::map(window.base, pci::BUS_SIZE)?;
    let (device, found) = radix_watchdog::find(&bus, Chipset, |why| {
        radix_klog::warn!("Watchdog: {why}; it is not used");
    })?;
    let _ = DEVICE.set(device);
    let radix_watchdog::Found { name, min_seconds, max_seconds, restarted } = found;
    Some(Watchdog { name, min_seconds, max_seconds, restarted })
}

/// Starts the watchdog to restart the machine unless petted within
/// `seconds`, and returns the timeout set.
pub fn start(seconds: u32) -> u32 {
    DEVICE.get().map_or(seconds, |device| device.start(seconds))
}

/// Pets the watchdog.
pub fn pet() {
    if let Some(device) = DEVICE.get() {
        device.pet();
    }
}

/// Stops the watchdog.
pub fn stop() {
    if let Some(device) = DEVICE.get() {
        device.stop();
    }
}

/// What the watchdog reaches, and the SMBus host controller (ADR 0206).
pub(crate) struct Chipset;

/// The FCH's power management index and data ports.
const FCH_PM_PORTS: [u16; 2] = [0xcd6, 0xcd7];
/// The PCI configuration ports.
const PCI_CONFIG_PORTS: core::ops::Range<u16> = 0xcf8..0xd00;

/// Whether the watchdog may reach `port`: the FCH's index and data ports,
/// or from 0x400 on, where chipsets put their functions' registers (the
/// ACPI and TCO bases) and PCI devices their I/O BARs, apart from PCI's
/// configuration ports. The legacy devices below 0x400 (the DMA
/// controllers among them) stay out of its reach.
fn reachable(port: u16) -> bool {
    FCH_PM_PORTS.contains(&port) || (port >= 0x400 && !PCI_CONFIG_PORTS.contains(&port))
}

impl Platform for Chipset {
    fn map(&self, phys: u64, len: usize) -> Option<Mmio> {
        let end = phys.checked_add(len as u64)?;
        if memory::overlaps_ram(phys..end) {
            return None;
        }
        devices::map(phys, len)
    }

    fn in8(&self, port: u16) -> u8 {
        if !reachable(port) {
            return 0xff;
        }
        // SAFETY: `reachable` keeps the access off the legacy DMA
        // controllers and PCI configuration, and `radix-watchdog` and
        // `radix-smbus` reach only the TCO and SMBus registers at the
        // bases the chipset decodes them at and the FCH's power management
        // registers, none of which moves memory.
        unsafe { inb(port) }
    }

    fn out8(&self, port: u16, value: u8) {
        if reachable(port) {
            // SAFETY: as in `in8`.
            unsafe { outb(port, value) }
        }
    }

    fn in16(&self, port: u16) -> u16 {
        if !reachable(port) {
            return 0xffff;
        }
        // SAFETY: as in `in8`.
        unsafe { inw(port) }
    }

    fn out16(&self, port: u16, value: u16) {
        if reachable(port) {
            // SAFETY: as in `in8`.
            unsafe { outw(port, value) }
        }
    }
}
