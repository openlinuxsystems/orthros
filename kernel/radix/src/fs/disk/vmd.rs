// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel VMDs (`radix_vmd`, Phase 11g, ADR 0249): the PCI domain a
//! laptop's firmware keeps its NVMe disks in when its storage is set to
//! "RST" or "RAID".
//!
//! [`bind`] maps the VMD's configuration space (CFGBAR), reads where its
//! buses start, and has [`radix_vmd::Plan`] number them and place every
//! BAR and bridge window in its two windows of memory. The domain is then
//! walked as a segment of its own, numbered from [`FIRST_SEGMENT`], each
//! function logged and listed as the firmware's are; its NVMe controllers
//! are bound polled, their DMA tagged with the VMD's own address, as the
//! IOMMU sees it. Other functions behind it, as an AHCI controller the
//! VMD also hides, are named and left alone.

use core::sync::atomic::{AtomicU16, Ordering};

use radix_hal::Devices;
use radix_hw::Mmio;
use radix_hw::pci::{self, Bar, Function};
use radix_klog::{info, warn};
use radix_sync::Mutex;
use radix_vmd::{Config, Domain, Location, Plan, Range};

use super::{describe, nvme};
use crate::Arch;

/// The segment number of the first VMD's domain; each next VMD's is one
/// more. Past any segment the firmware's MCFG numbers.
pub const FIRST_SEGMENT: u16 = 0x1000;

/// The most buses of a domain mapped and walked: a laptop's has three or
/// four, a server's a few dozen.
const MAX_BUSES: u8 = 32;

/// The domains set up.
static DOMAINS: AtomicU16 = AtomicU16::new(0);

/// The walk's state, a few kilobytes, kept off the stack; VMDs are set up
/// one at a time, at boot.
static PLAN: Mutex<Plan> = Mutex::new(Plan::new());

/// Whether `function` is a VMD.
pub fn is_vmd(function: &Function) -> bool {
    radix_vmd::model(function.vendor_id(), function.device_id()).is_some()
}

/// The domain's configuration space: CFGBAR, from its first bus.
struct Space {
    config: Mmio,
    first_bus: u8,
}

impl Space {
    /// Where `offset` of `at`'s registers is in CFGBAR, as ECAM lays it
    /// out; `None` past what is mapped.
    fn offset(&self, at: Location, offset: u16, len: usize) -> Option<usize> {
        let bus = at.bus.checked_sub(self.first_bus)?;
        let place = (usize::from(bus) << 20)
            | (usize::from(at.device) << 15)
            | (usize::from(at.function) << 12)
            | usize::from(offset);
        (place + len <= self.config.len()).then_some(place)
    }
}

impl Config for Space {
    fn read32(&mut self, at: Location, offset: u16) -> u32 {
        self.offset(at, offset, 4).map_or(u32::MAX, |place| self.config.read32(place))
    }

    fn write32(&mut self, at: Location, offset: u16, value: u32) {
        if let Some(place) = self.offset(at, offset, 4) {
            self.config.write32(place, value);
        }
    }

    fn write16(&mut self, at: Location, offset: u16, value: u16) {
        if let Some(place) = self.offset(at, offset, 2) {
            self.config.write16(place, value);
        }
    }
}

/// A memory BAR of the VMD's, as a range.
fn window(function: &Function, index: usize) -> Option<Range> {
    match function.bar(index)? {
        Bar::Memory { address, size, .. } => Some(Range { base: address, size }),
        Bar::Io { .. } => None,
    }
}

/// Sets up the VMD `function` and the domain behind it, and binds the
/// NVMe controllers there, counting them in `controllers` and every
/// function found in `functions`. Kept out of the frame of the scan.
#[inline(never)]
pub fn bind(function: &Function, controllers: &mut usize, functions: &mut usize) {
    if let Err(why) = try_bind(function, controllers, functions) {
        warn!(
            "Disk: could not set up the Intel VMD at {}: {why}; the disks behind it are not seen \
             (setting the firmware's storage to AHCI shows them)",
            function.address()
        );
    }
}

/// Why a VMD is not used.
enum Why {
    Text(&'static str),
    Vmd(radix_vmd::Error),
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Why::Text(text) => f.write_str(text),
            Why::Vmd(err) => write!(f, "{err}"),
        }
    }
}

impl From<radix_vmd::Error> for Why {
    fn from(err: radix_vmd::Error) -> Why {
        Why::Vmd(err)
    }
}

impl From<&'static str> for Why {
    fn from(text: &'static str) -> Why {
        Why::Text(text)
    }
}

fn try_bind(
    function: &Function,
    controllers: &mut usize,
    functions: &mut usize,
) -> Result<(), Why> {
    let restricted = radix_vmd::model(function.vendor_id(), function.device_id()).unwrap_or(false);
    let number = DOMAINS.load(Ordering::Relaxed);
    let segment = FIRST_SEGMENT.checked_add(number).ok_or("too many VMDs")?;
    function.enable_memory_and_dma();
    let config = window(function, radix_vmd::CFGBAR).ok_or("its BAR 0 is not in memory")?;
    let membar1 = window(function, radix_vmd::MEMBAR1).ok_or("its BAR 2 is not in memory")?;
    let membar2 = window(function, radix_vmd::MEMBAR2).unwrap_or_default();
    let vmcap = function.read32(radix_vmd::VMCAP);
    let vmconfig = function.read32(radix_vmd::VMCONFIG);
    let first_bus = radix_vmd::first_bus(restricted, vmcap, vmconfig)?;
    let mut domain = Domain::new(first_bus, config.size, membar1, membar2)?;
    domain.last_bus = domain.last_bus.min(first_bus.saturating_add(MAX_BUSES - 1));
    let buses = usize::from(domain.last_bus - first_bus) + 1;
    let mapped = <Arch as Devices>::map_device(config.base, buses << 20)
        .ok_or("its configuration space could not be mapped")?;
    let mut space = Space { config: mapped, first_bus };
    let summary = PLAN.lock().configure(&mut space, &domain)?;
    DOMAINS.store(number + 1, Ordering::Relaxed);
    let plural = |n: usize, one: &'static str, many: &'static str| if n == 1 { one } else { many };
    info!(
        "Disk: the Intel VMD at {} keeps PCI segment {segment:04x}, buses {first_bus} to {}: {} \
         {}, {} {}, in {} KiB of its memory and {} KiB of its prefetchable memory",
        function.address(),
        summary.last_bus,
        summary.bridges,
        plural(summary.bridges, "bridge", "bridges"),
        summary.endpoints,
        plural(summary.endpoints, "device", "devices"),
        summary.memory_used.div_ceil(1024),
        summary.prefetchable_used.div_ceil(1024),
    );
    if summary.io_bars > 0 {
        info!(
            "Disk: the VMD's domain has no I/O space: {} I/O {} behind it left off",
            summary.io_bars,
            plural(summary.io_bars, "BAR", "BARs")
        );
    }
    if summary.unnumbered > 0 {
        warn!(
            "Disk: {} {} behind the VMD at {} given no buses (too deep, or past its last): what \
             is behind them is not seen",
            summary.unnumbered,
            plural(summary.unnumbered, "bridge", "bridges"),
            function.address()
        );
    }
    let vmd = function.address();
    let mut map_bus = |bus: u8| {
        let offset = usize::from(bus.checked_sub(first_bus)?) << 20;
        space.config.window(offset, pci::BUS_SIZE)
    };
    pci::scan(segment, first_bus, summary.last_bus, &mut map_bus, &mut |child| {
        *functions += 1;
        describe(&child);
        if child.class() == radix_nvme::PCI_CLASS {
            nvme::bind_behind(&child, controllers, vmd);
        } else if child.header_type() & 0x7f == 0 {
            info!("Disk: {} behind the VMD is not used yet", child.address());
        }
    });
    Ok(())
}
