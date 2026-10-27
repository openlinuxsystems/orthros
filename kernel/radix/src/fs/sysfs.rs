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
//! - `/kernel/bus/pci/devices/<address>/vendor`, `device` and `class`, in
//!   hexadecimal, for every PCI function the scan found.
//!
//! Every file ends with a newline. The tree is read-only.

use core::fmt::{self, Write};

use radix_block::DeviceRef;
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
        let sector = radix_block::SECTOR;
        for (name, number) in [
            ("logical_block_size", sector),
            ("physical_block_size", u64::from(info.block_size)),
            ("minimum_io_size", u64::from(info.block_size)),
            ("optimal_io_size", 0),
            ("rotational", u64::from(info.rotational)),
            ("discard_granularity", if info.discard_sectors > 0 { sector } else { 0 }),
            ("discard_max_bytes", info.discard_sectors.saturating_mul(sector)),
        ] {
            queue.add(name, value(move |text| writeln!(text, "{number}"))?)?;
        }
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
        Ok(dir)
    }
}
