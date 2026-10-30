// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `selftest=iommu`: QEMU's `edu` device, behind its IOMMU, copies by DMA
//! from and to a page given to it, and is stopped copying into one that
//! was never given to it or was taken back (ADR 0164). The unit logs each
//! stopped copy as a fault.
//!
//! edu's registers (QEMU's docs/specs/edu.rst): 0x00 reads `0x010000ed`;
//! a DMA copies 0x90 bytes from 0x80 to 0x88 when bit 0 of 0x98 is set,
//! from memory into the device's own buffer at 0x40000, or the other way
//! with bit 1, and bit 0 clears when it is done.

use radix_hal::{Devices, PciSegment};
use radix_hw::pci::{self, Bar, Function};
use radix_hw::{Dma, Mmio};
use radix_klog::{error, info};

use crate::{Arch, sched};

const VENDOR: u16 = 0x1234;
const DEVICE: u16 = 0x11e8;
const SOURCE: usize = 0x80;
const DESTINATION: usize = 0x88;
const COUNT: usize = 0x90;
const COMMAND: usize = 0x98;
const START: u64 = 1 << 0;
const TO_MEMORY: u64 = 1 << 1;
/// The device's buffer, at its address for DMA.
const BUFFER: u64 = 0x40000;
/// The bytes each copy moves.
const BYTES: u64 = 256;

pub fn run() {
    match check() {
        Ok(()) => info!(
            "selftest iommu: passed; the edu device reached only the pages given to it, and lost \
             one taken back"
        ),
        Err(what) => error!("selftest iommu: failed; {what}"),
    }
}

/// The edu function, from the PCI scan.
fn edu() -> Option<Function> {
    let mut address = None;
    crate::fs::disk::pci_functions(|found| {
        if found.vendor == VENDOR && found.device == DEVICE && address.is_none() {
            address = Some(found.address);
        }
    });
    let address = address?;
    let mut segment = None;
    <Arch as Devices>::pci_segments(&mut |found: PciSegment| {
        if found.segment == address.segment {
            segment = Some(found);
        }
    });
    let base = segment?.base + (u64::from(address.bus) << 20) + address.offset_in_bus() as u64;
    let config = <Arch as Devices>::map_device(base, pci::CONFIG_SIZE)?;
    Function::probe(address, config)
}

/// Has edu copy [`BYTES`] bytes between its buffer and `memory`, and waits
/// until it has.
fn copy(regs: &Mmio, memory: u64, to_memory: bool) -> Result<(), &'static str> {
    let (from, to) = if to_memory { (BUFFER, memory) } else { (memory, BUFFER) };
    regs.write64(SOURCE, from);
    regs.write64(DESTINATION, to);
    regs.write64(COUNT, BYTES);
    regs.write64(COMMAND, START | if to_memory { TO_MEMORY } else { 0 });
    let deadline = sched::now_ns() + 1_000_000_000;
    while regs.read64(COMMAND) & START != 0 {
        if sched::now_ns() > deadline {
            return Err("edu did not finish a copy within a second");
        }
        sched::sleep_ns(1_000_000);
    }
    Ok(())
}

fn check() -> Result<(), &'static str> {
    let function = edu().ok_or("there is no edu device (QEMU's -device edu)")?;
    let source = function.address();
    let Some(Bar::Memory { address, size, .. }) = function.bar(0) else {
        return Err("edu's registers (BAR 0) are not in memory");
    };
    let regs = usize::try_from(size)
        .ok()
        .and_then(|size| <Arch as Devices>::map_device(address, size))
        .ok_or("edu's registers could not be mapped")?;
    if regs.read32(0) != 0x0100_00ed {
        return Err("edu's identification register is wrong");
    }
    function.enable_memory_and_dma();
    let wanted = |at: usize| (at as u8).wrapping_mul(7).wrapping_add(1);
    // Its DMA memory: what it copies from.
    let given: Dma = <Arch as Devices>::alloc_dma(source, 1, true).ok_or("out of DMA memory")?;
    for at in 0..BYTES as usize {
        given.write8(at, wanted(at));
    }
    copy(&regs, given.phys(), false)?;
    // A page no device was given.
    let mut other = <Arch as Devices>::alloc_cache_page().ok_or("out of memory")?;
    let page = other.phys();
    if page >> 32 != 0 {
        return Err("the test page is above 4 GiB, past what edu reaches");
    }
    let holds_copy = |page: &radix_hw::Page| {
        page.bytes()[..BYTES as usize].iter().enumerate().all(|(at, &byte)| byte == wanted(at))
    };
    let untouched = |page: &radix_hw::Page| page.bytes()[..BYTES as usize].iter().all(|&b| b == 0);
    copy(&regs, page, true)?;
    if !untouched(&other) {
        return Err("edu wrote to a page that was never given to it");
    }
    let frames = [(page, 4096)];
    if !<Arch as Devices>::map_dma(source, &frames) {
        return Err("the page could not be mapped for edu");
    }
    copy(&regs, page, true)?;
    let copied = holds_copy(&other);
    <Arch as Devices>::unmap_dma(source, &frames);
    if !copied {
        return Err("edu could not write to a page mapped for it");
    }
    other.bytes_mut().fill(0);
    copy(&regs, page, true)?;
    if !untouched(&other) {
        return Err("edu still wrote to a page after it was taken back");
    }
    Ok(())
}
