// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! virtio block devices (`radix-virtio`), named `vda`, `vdb` and on.
//!
//! [`bind`] maps a device's registers, turns on bus mastering, gives it
//! one MSI-X interrupt for its queue, and sets it up; a device with no
//! MSI-X vector to spare is polled by the threads waiting for it. A
//! request's descriptor chain is matched to its slot by the chain's head,
//! which may be reused as soon as the chain is handed back, so under the
//! queue's lock at that moment.

use alloc::sync::Arc;
use core::fmt::Write;

use radix_block::{DeviceRef, Error, Info};
use radix_hal::Devices;
use radix_hw::pci::{Bar, Function, MsiX};
use radix_klog::{TextBuf, info};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::Once;
use radix_virtio::blk::{self, Blk};
use radix_virtio::{NO_VECTOR, Transport};

use super::queue::{Controller, Disk, Op, Outcome};
use super::{Name, Size, partitions, register};
use crate::Arch;

/// The most virtio disks.
const MAX_DISKS: usize = 16;

/// A virtio block device as the queue drives it.
pub struct Virtio {
    blk: Blk,
    /// Which slot each descriptor chain in flight belongs to, by head.
    owner: [u8; radix_virtio::queue::MAX_SIZE as usize],
    /// Its interrupt vector, if it has one.
    vector: Option<u32>,
}

impl Controller for Virtio {
    fn room(&self, _op: Op, segments: usize) -> bool {
        // A discard's range takes a buffer of its own.
        self.blk.room(segments.max(1)) > 0
    }

    fn submit(&mut self, slot: u8, op: Op, sector: u64, data: &[(u64, u32)]) -> Result<(), Error> {
        let op = match op {
            Op::Read => blk::Op::Read,
            // Never asked for: the disk does not say it writes through.
            Op::Write | Op::WriteThrough => blk::Op::Write,
            Op::Flush => blk::Op::Flush,
            Op::Discard(count) => {
                blk::Op::Discard(u32::try_from(count).map_err(|_| Error::OutOfRange)?)
            }
        };
        let head = self.blk.submit(op, sector, data).map_err(|err| match err {
            radix_virtio::Error::BadRequest => Error::OutOfRange,
            _ => Error::Io,
        })?;
        let len = self.owner.len();
        self.owner[usize::from(head) % len] = slot;
        Ok(())
    }

    fn complete(&mut self) -> Option<(u8, Outcome)> {
        let (head, outcome) = self.blk.complete()?;
        let slot = self.owner[usize::from(head) % self.owner.len()];
        let outcome = if outcome == blk::Outcome::Done { Outcome::Done } else { Outcome::Failed };
        Some((slot, outcome))
    }
}

/// Only a disk whose setup failed is dropped: the device stops using
/// what it was given, and its interrupt is let go.
impl Drop for Virtio {
    fn drop(&mut self) {
        self.blk.reset();
        if let Some(vector) = self.vector {
            <Arch as Devices>::free_interrupt(vector);
        }
    }
}

/// The disks, for their interrupt handlers: set once each.
static DISKS: [Once<Arc<Disk<Virtio>, Kmalloc>>; MAX_DISKS] = [const { Once::new() }; MAX_DISKS];

fn on_interrupt(index: usize) {
    if let Some(disk) = DISKS.get(index).and_then(Once::get) {
        disk.collect();
    }
}

/// Sets up the virtio block device `function` as disk number `index`.
/// Its frame holds the disk before it goes on the heap: kept out of
/// `init`'s (tools/xtask/src/frames.rs allows it).
#[inline(never)]
pub fn bind(function: &Function, index: usize) -> Result<(), &'static str> {
    if index >= MAX_DISKS {
        return Err("too many disks");
    }
    function.enable_memory_and_dma();
    let mut map_bar = |_: usize, bar: Bar| match bar {
        Bar::Memory { address, size, .. } => {
            <Arch as Devices>::map_device(address, usize::try_from(size).ok()?)
        }
        Bar::Io { .. } => None,
    };
    let transport = Transport::find(function, &mut map_bar).map_err(|_| "no modern transport")?;
    // One MSI-X entry, for the queue, if the device and the kernel have
    // them; otherwise the device is polled.
    let interrupt = MsiX::find(function).and_then(|msix| {
        let (bar, offset) = msix.table_location();
        let Some(Bar::Memory { address, .. }) = function.bar(bar) else { return None };
        let table = <Arch as Devices>::map_device(address + offset, msix.table_len())?;
        let (vector, message) = <Arch as Devices>::allocate_interrupt(on_interrupt, index)?;
        msix.enable_masked(function);
        msix.set_entry(&table, 0, message);
        msix.unmask_all(function);
        Some(vector)
    });
    let entry = if interrupt.is_some() { 0 } else { NO_VECTOR };
    let mut alloc = |pages: usize| <Arch as Devices>::alloc_dma(pages, false);
    let Ok(blk) = Blk::new(transport, entry, &mut alloc) else {
        if let Some(vector) = interrupt {
            <Arch as Devices>::free_interrupt(vector);
        }
        return Err("the device refused to be set up");
    };
    let found = blk.info();
    // From here a failure drops the driver, which lets the device go.
    let controller =
        Virtio { blk, owner: [0; radix_virtio::queue::MAX_SIZE as usize], vector: interrupt };
    if found.max_segment_size != 0 && (found.max_segment_size as usize) < radix_hw::PAGE_SIZE {
        return Err("its segments are smaller than a page");
    }
    let mut name = Name::new();
    let _ = write!(name, "vd{}", (b'a' + index as u8) as char);
    let info = Info {
        sectors: found.sectors,
        block_size: found.block_size,
        read_only: found.read_only,
        max_segments: found.max_segments as usize,
        discard_sectors: u64::from(found.discard_sectors),
        // Linux's default for a disk that does not say.
        rotational: true,
    };
    let disk = Disk::new(controller, info, found.flush, false, interrupt.is_some(), name.clone());
    let disk = Arc::try_new_in(disk, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")?;
    let _ = DISKS[index].set(disk.clone());
    let size = Size(info.sectors * radix_block::SECTOR);
    let mut how = TextBuf::<24>::new();
    let _ = match interrupt {
        Some(vector) => write!(how, "MSI-X vector {vector:#x}"),
        None => how.write_str("polled"),
    };
    info!(
        "Disk: {name} is a virtio block device of {size} at {}, {} bytes a block{}{}{}, {how}",
        function.address(),
        info.block_size,
        if info.read_only { ", read-only" } else { "" },
        if found.flush { ", write cache" } else { "" },
        if info.discard_sectors > 0 { ", discards" } else { "" },
    );
    let device: DeviceRef = disk;
    register(&name, device.clone());
    partitions(&name, &device);
    Ok(())
}
