// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/kernel`: the kernel's view of the hardware and its settings, one
//! value per file. `/sys`, Linux's name for it, is a link to it.
//!
//! The layout follows Linux's where both have a thing, so that ported
//! tools find it where they look (lspci reads `/sys/bus/pci/devices`):
//!
//! - `/kernel/version`, `/kernel/cmdline`, `/kernel/cpus`;
//! - `/kernel/block/<name>/size` (in 512-byte sectors) and `ro` (0 or 1),
//!   for every disk and partition; for a partition, its number in its
//!   disk's table (`partition`) and its first sector (`start`); and for a
//!   disk, in its `queue`, the sizes and flags of radix-block(2)'s
//!   requests as Linux names them (`rotational`, `logical_block_size`,
//!   `discard_max_bytes` and the rest), and in its `device` the `model`,
//!   `serial` and `firmware_rev` it gives, for SATA and NVMe disks;
//! - `/kernel/bus/pci/devices/<address>/vendor`, `device`, `class`,
//!   `revision`, `subsystem_vendor` and `subsystem_device`, in
//!   hexadecimal, for every PCI function the scan found;
//! - `/kernel/firmware/memmap/<n>/start`, `end` (its last byte, in
//!   hexadecimal) and `type` (Linux's names: `System RAM`, `Reserved`,
//!   `ACPI Tables`...), for each range of the firmware's memory map
//!   (ADR 0144);
//! - `/kernel/class/watchdog/watchdog0`, when the kernel drives a hardware
//!   watchdog (ADR 0143): its `identity`, its `state` (`active` or
//!   `inactive`), its `timeout`, `min_timeout` and `max_timeout` in
//!   seconds, and its `bootstatus`, 32 (Linux's WDIOF_CARDRESET) if it
//!   restarted the machine at the end of the boot before, else 0.
//!
//! Every file ends with a newline. The tree is read-only.

use core::fmt::{self, Write};

use radix_block::DeviceRef;
use radix_hal::{MemoryKind, MemoryRegion, Watchdog};
use radix_klog::TextBuf;
use radix_sync::Once;
use radix_vfs::synthetic::{FixedDir, Generated, Generator, ListedDir, Listing, SyntheticFs};
use radix_vfs::{Error, FsKind, FsRef, KArc, Kind, NodeRef, Result, Text, mem, next_id};

use super::disk;

static COMMAND_LINE: Once<&'static str> = Once::new();
static ROOT: Once<KArc<FixedDir>> = Once::new();

/// Records the kernel's command line, for `/kernel/cmdline`.
pub fn set_command_line(line: &'static str) {
    let _ = COMMAND_LINE.set(line);
}

/// A `sys` filesystem: every mount shows the same tree.
pub fn make() -> Result<FsRef> {
    if let Some(root) = ROOT.get() {
        let root: NodeRef = root.clone();
        return SyntheticFs::make(FsKind::Sys, root);
    }
    let root = FixedDir::new(next_id())?;
    root.add("version", Generated::from_fn(next_id(), version)?)?;
    root.add("cmdline", Generated::from_fn(next_id(), cmdline)?)?;
    root.add("cpus", Generated::from_fn(next_id(), cpus)?)?;
    let block: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(Blocks)?;
    root.add("block", ListedDir::make(next_id(), block)?)?;
    let bus = root.add_dir("bus", next_id())?;
    let pci = bus.add_dir("pci", next_id())?;
    let devices: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(PciDevices)?;
    pci.add("devices", ListedDir::make(next_id(), devices)?)?;
    let firmware = root.add_dir("firmware", next_id())?;
    let memmap: radix_vfs::mem::KBox<dyn Listing> = mem::boxed(MemoryMap)?;
    firmware.add("memmap", ListedDir::make(next_id(), memmap)?)?;
    if let Some(found) = crate::watchdog::found() {
        let class = root.add_dir("class", next_id())?;
        let dir = class.add_dir("watchdog", next_id())?.add_dir("watchdog0", next_id())?;
        let Watchdog { name, min_seconds, max_seconds, restarted } = *found;
        dir.add("identity", value(move |text| writeln!(text, "{name}"))?)?;
        dir.add(
            "state",
            value(|text| {
                let active = crate::watchdog::state().0;
                writeln!(text, "{}", if active { "active" } else { "inactive" })
            })?,
        )?;
        dir.add("timeout", value(|text| writeln!(text, "{}", crate::watchdog::state().1))?)?;
        dir.add("min_timeout", value(move |text| writeln!(text, "{min_seconds}"))?)?;
        dir.add("max_timeout", value(move |text| writeln!(text, "{max_seconds}"))?)?;
        let status = if restarted { 32 } else { 0 };
        dir.add("bootstatus", value(move |text| writeln!(text, "{status}"))?)?;
    }
    // Another thread may have made one meanwhile; either will do.
    let root = match ROOT.set(root) {
        Ok(root) => root.clone(),
        Err(root) => root,
    };
    let root: NodeRef = root;
    SyntheticFs::make(FsKind::Sys, root)
}

fn version(text: &mut Text) -> fmt::Result {
    writeln!(text, "{}", crate::BANNER)
}

fn cmdline(text: &mut Text) -> fmt::Result {
    writeln!(text, "{}", COMMAND_LINE.get().copied().unwrap_or(""))
}

fn cpus(text: &mut Text) -> fmt::Result {
    writeln!(text, "{}", crate::sched::online().len())
}

/// A file of one value, written when it is opened.
fn value(generate: impl Fn(&mut Text) -> fmt::Result + Send + Sync + 'static) -> Result<NodeRef> {
    let generate: Generator = mem::boxed(generate)?;
    Generated::make(next_id(), generate)
}

/// `/kernel/block`: a directory per disk and partition.
struct Blocks;

impl Listing for Blocks {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        let mut more = true;
        disk::for_each(|entry| {
            if more {
                more = f(entry.name.as_str(), Kind::Directory);
            }
        });
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let mut found: Option<(DeviceRef, Option<disk::Place>, Option<disk::Ident>)> = None;
        disk::for_each(|entry| {
            if entry.name.as_str() == name {
                found = Some((entry.device.clone(), entry.place, entry.ident));
            }
        });
        let (device, place, ident) = found.ok_or(Error::NotFound)?;
        let dir = FixedDir::new(next_id())?;
        let info = device.info();
        dir.add("size", value(move |text| writeln!(text, "{}", info.sectors))?)?;
        dir.add("ro", value(move |text| writeln!(text, "{}", u8::from(info.read_only)))?)?;
        // A partition has its number and first sector, and its disk the
        // queue, as on Linux.
        if let Some(place) = place {
            dir.add("partition", value(move |text| writeln!(text, "{}", place.number))?)?;
            dir.add("start", value(move |text| writeln!(text, "{}", place.first))?)?;
            return Ok(dir);
        }
        // What the disk says it is, where Linux has an NVMe controller's.
        if let Some(ident) = ident {
            let device = dir.add_dir("device", next_id())?;
            device.add("model", value(move |text| writeln!(text, "{}", ident.model()))?)?;
            device.add("serial", value(move |text| writeln!(text, "{}", ident.serial()))?)?;
            let firmware = value(move |text| writeln!(text, "{}", ident.firmware()))?;
            device.add("firmware_rev", firmware)?;
        }
        let queue = dir.add_dir("queue", next_id())?;
        let (sector, logical) = (radix_block::SECTOR, u64::from(info.logical_block));
        for (name, number) in [
            ("logical_block_size", logical),
            ("physical_block_size", u64::from(info.block_size)),
            ("minimum_io_size", u64::from(info.block_size)),
            ("optimal_io_size", 0),
            ("rotational", u64::from(info.rotational)),
            ("discard_granularity", if info.discard_sectors > 0 { logical } else { 0 }),
            ("discard_max_bytes", info.discard_sectors.saturating_mul(sector)),
        ] {
            queue.add(name, value(move |text| writeln!(text, "{number}"))?)?;
        }
        Ok(dir)
    }
}

/// `/kernel/firmware/memmap`: a directory per range of the firmware's
/// memory map, numbered in its order.
struct MemoryMap;

/// The firmware's memory map, as the kernel was given it.
fn memory_map() -> &'static [MemoryRegion] {
    crate::boot_summary().map_or(&[], |boot| boot.memory_map)
}

/// Linux's name for a kind of memory, as its `/sys/firmware/memmap`
/// gives it.
fn linux_name(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Usable => "System RAM",
        MemoryKind::Reserved => "Reserved",
        MemoryKind::AcpiReclaimable => "ACPI Tables",
        MemoryKind::AcpiNvs => "ACPI Non-volatile Storage",
        MemoryKind::Defective => "Unusable memory",
    }
}

impl Listing for MemoryMap {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        for index in 0..memory_map().len() {
            let mut name = TextBuf::<8>::new();
            let _ = write!(name, "{index}");
            if !f(name.as_str(), Kind::Directory) {
                break;
            }
        }
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let index: usize = name.parse().map_err(|_| Error::NotFound)?;
        // "1" and not "01": one name for each.
        if name.len() > 1 && name.starts_with('0') {
            return Err(Error::NotFound);
        }
        let region = *memory_map().get(index).ok_or(Error::NotFound)?;
        let dir = FixedDir::new(next_id())?;
        dir.add("start", value(move |text| writeln!(text, "{:#x}", region.start))?)?;
        dir.add("end", value(move |text| writeln!(text, "{:#x}", region.end - 1))?)?;
        dir.add("type", value(move |text| writeln!(text, "{}", linux_name(region.kind)))?)?;
        Ok(dir)
    }
}

/// `/kernel/bus/pci/devices`: a directory per PCI function.
struct PciDevices;

fn address_of(function: &disk::PciFunction) -> TextBuf<16> {
    let mut name = TextBuf::<16>::new();
    let _ = write!(name, "{}", function.address);
    name
}

impl Listing for PciDevices {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        let mut more = true;
        disk::pci_functions(|function| {
            if more {
                more = f(address_of(function).as_str(), Kind::Directory);
            }
        });
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let mut found = None;
        disk::pci_functions(|function| {
            if address_of(function).as_str() == name {
                found = Some(*function);
            }
        });
        let function = found.ok_or(Error::NotFound)?;
        let dir = FixedDir::new(next_id())?;
        dir.add("vendor", value(move |text| writeln!(text, "{:#06x}", function.vendor))?)?;
        dir.add("device", value(move |text| writeln!(text, "{:#06x}", function.device))?)?;
        dir.add("class", value(move |text| writeln!(text, "{:#08x}", function.class))?)?;
        dir.add("revision", value(move |text| writeln!(text, "{:#04x}", function.revision))?)?;
        let (vendor, device) = function.subsystem;
        dir.add("subsystem_vendor", value(move |text| writeln!(text, "{vendor:#06x}"))?)?;
        dir.add("subsystem_device", value(move |text| writeln!(text, "{device:#06x}"))?)?;
        Ok(dir)
    }
}
