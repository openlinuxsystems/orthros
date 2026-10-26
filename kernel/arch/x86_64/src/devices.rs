// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Device support for drivers (`radix_hal::Devices`): mapping registers,
//! DMA memory, the page cache's pages and message-signalled interrupts.
//!
//! Vectors [`FIRST`] to [`LAST`] are for devices. [`allocate`] hands one
//! out with the function it calls; a device raises it by writing the
//! [`MsiMessage`] it returns: the local APIC's address with the APIC ID of
//! the CPU it is for as destination (the boot CPU's for most devices),
//! and the vector, edge-triggered with fixed delivery (Intel SDM volume 3,
//! "Message Signalled Interrupts"). The message has eight bits for the
//! destination, so a CPU whose APIC ID is above 255 gets the boot CPU's
//! interrupts instead: naming it would need interrupt remapping. The
//! dispatcher calls the function, then ends the interrupt. A handler is
//! read with acquire ordering after its argument was stored, so a vector
//! whose message a device already knows never runs a half-registered
//! handler; freeing waits for nothing, so a driver frees its vector only
//! after its device has stopped sending.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use radix_hw::{Dma, Mmio, MsiMessage, Page};
use radix_mm::{FRAMES, Flags, Owner};
use radix_sync::SpinLock;

use crate::memory::{self, PAGE};
use crate::{apic, paging, smp};

/// The first vector for devices.
pub const FIRST: u8 = 0x50;
/// The last vector for devices.
pub const LAST: u8 = 0xef;
const COUNT: usize = (LAST - FIRST) as usize + 1;

static HANDLERS: [AtomicUsize; COUNT] = [const { AtomicUsize::new(0) }; COUNT];
static ARGS: [AtomicUsize; COUNT] = [const { AtomicUsize::new(0) }; COUNT];
static COUNTS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];
/// Which device vectors are taken.
static TAKEN: SpinLock<[bool; COUNT]> = SpinLock::new([false; COUNT]);

/// Takes a free vector for `handler(arg)`, sent to CPU `cpu`.
pub fn allocate(cpu: usize, handler: fn(usize), arg: usize) -> Option<(u32, MsiMessage)> {
    let index = {
        let mut taken = TAKEN.lock_irq();
        let index = taken.iter().position(|taken| !taken)?;
        taken[index] = true;
        index
    };
    ARGS[index].store(arg, Ordering::Relaxed);
    HANDLERS[index].store(handler as usize, Ordering::Release);
    let vector = u32::from(FIRST) + index as u32;
    let destination = smp::info(cpu)
        .filter(|found| found.online && found.hardware_id <= 0xff)
        .or_else(|| smp::info(0))
        .map_or(0, |found| found.hardware_id & 0xff);
    let destination = u64::from(destination);
    let message = MsiMessage { address: 0xfee0_0000 | (destination << 12), data: vector };
    Some((vector, message))
}

/// Gives back a vector from [`allocate`].
pub fn free(vector: u32) {
    let Some(index) = index_of(vector) else { return };
    HANDLERS[index].store(0, Ordering::Release);
    TAKEN.lock_irq()[index] = false;
}

fn index_of(vector: u32) -> Option<usize> {
    let index = vector.checked_sub(u32::from(FIRST))? as usize;
    (index < COUNT).then_some(index)
}

/// Interrupts vector `vector` delivered.
pub fn count(vector: u32) -> u64 {
    index_of(vector).map_or(0, |index| COUNTS[index].load(Ordering::Relaxed))
}

/// Called by the interrupt dispatcher for a device vector.
pub fn dispatch(vector: u8) {
    if let Some(index) = index_of(u32::from(vector)) {
        COUNTS[index].fetch_add(1, Ordering::Relaxed);
        let handler = HANDLERS[index].load(Ordering::Acquire);
        if handler != 0 {
            let arg = ARGS[index].load(Ordering::Relaxed);
            // SAFETY: only `allocate` stores non-zero values here, and it
            // stores a `fn(usize)`.
            let handler: fn(usize) = unsafe { core::mem::transmute::<usize, fn(usize)>(handler) };
            handler(arg);
        } else {
            radix_klog::tracepoint!("irq.device.unclaimed", vector);
        }
    }
    apic::end_of_interrupt();
}

/// Maps device registers uncached.
pub fn map(phys: u64, len: usize) -> Option<Mmio> {
    let virt = paging::try_map_physical(
        phys,
        len as u64,
        paging::Access::ReadWrite,
        paging::Cache::Uncached,
    )?;
    // SAFETY: `try_map_physical` mapped `len` bytes at `virt` uncached and
    // readable and writable, in the mapping window, where nothing else is
    // mapped, and device mappings are never undone.
    Some(unsafe { Mmio::new(virt, len) })
}

fn release(_virt: usize, phys: u64, len: usize) {
    memory::free_pages(phys, (len as u64).div_ceil(PAGE), Owner::Kernel);
}

/// Allocates zeroed, physically contiguous memory for DMA.
pub fn alloc_dma(pages: usize, below_4g: bool) -> Option<Dma> {
    let pages = pages.max(1) as u64;
    let phys = memory::alloc_pages(pages, below_4g, Owner::Kernel)?;
    let virt = memory::phys_to_virt(phys);
    // SAFETY: the frame allocator gave these pages to us alone, zeroed;
    // the direct map maps all RAM writable at `phys_to_virt`, and the
    // block is contiguous. `release` gives them back exactly once.
    Some(unsafe { Dma::new(virt, phys, (pages * PAGE) as usize, release) })
}

fn release_cache_page(_virt: usize, phys: u64) {
    memory::free_pages(phys, 1, Owner::PageCache);
}

/// Allocates a zeroed page for the page cache, with an allocation that may
/// sleep, so that it never takes the reserves and runs reclaim first.
pub fn alloc_cache_page() -> Option<Page> {
    let phys = FRAMES.alloc(0, Flags::MAY_SLEEP.zeroed(), Owner::PageCache).ok()?;
    let virt = memory::phys_to_virt(phys);
    // SAFETY: the frame allocator gave this page to us alone, zeroed; the
    // direct map maps all RAM writable at `phys_to_virt`, and
    // `release_cache_page` gives it back exactly once, when the `Page` is
    // dropped.
    Some(unsafe { Page::new(virt, phys, release_cache_page) })
}
