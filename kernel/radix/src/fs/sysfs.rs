// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/sys`: the kernel's devices and settings, one value per file.
//!
//! The layout follows Linux's where both have a thing, so that ported
//! tools find it where they look (lspci reads `/sys/bus/pci/devices`):
//!
//! - `/sys/kernel/version`, `/sys/kernel/cmdline`, `/sys/kernel/cpus`;
//! - `/sys/block/<name>/size` (in 512-byte sectors) and `ro` (0 or 1),
//!   for every disk and partition;
//! - `/sys/bus/pci/devices/<address>/vendor`, `device` and `class`, in
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

/// Records the kernel's command line, for `/sys/kernel/cmdline`.
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
    let kernel = root.add_dir("kernel", next_id())?;
    kernel.add("version", Generated::from_fn(next_id(), version)?)?;
    kernel.add("cmdline", Generated::from_fn(next_id(), cmdline)?)?;
    kernel.add("cpus", Generated::from_fn(next_id(), cpus)?)?;
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
    writeln!(text, "Radix {} (Dukovo Phase 7)", env!("CARGO_PKG_VERSION"))
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

/// `/sys/block`: a directory per disk and partition.
struct Blocks;

impl Listing for Blocks {
    fn list(&self, f: &mut dyn FnMut(&str, Kind) -> bool) {
        let mut more = true;
        disk::for_each(|name, _| {
            if more {
                more = f(name, Kind::Directory);
            }
        });
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        let mut found: Option<DeviceRef> = None;
        disk::for_each(|n, device| {
            if n == name {
                found = Some(device.clone());
            }
        });
        let device = found.ok_or(Error::NotFound)?;
        let dir = FixedDir::new(next_id())?;
        let info = device.info();
        dir.add("size", value(move |text| writeln!(text, "{}", info.sectors))?)?;
        dir.add("ro", value(move |text| writeln!(text, "{}", u8::from(info.read_only)))?)?;
        Ok(dir)
    }
}

/// `/sys/bus/pci/devices`: a directory per PCI function.
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
