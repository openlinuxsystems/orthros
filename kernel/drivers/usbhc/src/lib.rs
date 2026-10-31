// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The USB host controllers of PCs from before xHCI, and beside it (Phase
//! 11g, ADR 0243): EHCI for USB 2.0's high speed, and the OHCI and UHCI
//! controllers that carry low and full speed, on their own or as an EHCI
//! controller's companions.
//!
//! - [`ehci::Ehci`]: Intel's *Enhanced Host Controller Interface
//!   Specification for Universal Serial Bus*, revision 1.0, PCI class
//!   0c/03/20.
//! - [`intel`]: the switch on Intel's 7, 8 and 9 Series chipsets and Bay
//!   Trail that moves their USB 2.0 ports from the EHCI controllers to the
//!   xHCI one.
//!
//! Each implements `radix_usb::host::Hcd`, which the kernel's thread for
//! the controller drives: the kernel addresses the devices itself, and
//! the drivers carry their transfers. The crate is a compartment, with no
//! `unsafe` code: it reaches a controller through the [`Mmio`] window and
//! the [`Dma`] memory that `radix-hw` gives it, and its PCI configuration
//! space through [`radix_hw::pci::Function`]. Nothing here sleeps itself or
//! takes a lock. The tests run each driver against a simulated controller
//! on the host, which walks the structures in memory as the specification
//! says.
//!
//! [`Mmio`]: radix_hw::Mmio

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod ehci;
pub mod intel;

#[cfg(test)]
mod tests;

use radix_hw::Dma;
use radix_usb::host::Waiter;

/// How often a driver looks while it waits.
const POLL_US: u32 = 100;

/// Waits until `done` holds or `limit_us` has passed; returns whether it
/// held.
fn wait_for(waiter: &mut dyn Waiter, limit_us: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = waiter.now_us().saturating_add(limit_us);
    loop {
        if done() {
            return true;
        }
        if waiter.now_us() >= deadline {
            return done();
        }
        waiter.wait(POLL_US);
    }
}

/// How the firmware let go of a controller it drove, for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handoff {
    /// The controller has no way to ask.
    None,
    /// The firmware did not hold it.
    Free,
    /// The firmware let go when asked.
    Given,
    /// The firmware did not let go in time, and was overruled.
    Taken,
}

/// How a controller could not be started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// It is not one the driver can run.
    Unsupported(&'static str),
    /// It did not halt, reset or start in time.
    Unresponsive(&'static str),
    /// There was no memory for its structures.
    NoMemory,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Unsupported(why) | Error::Unresponsive(why) => f.write_str(why),
            Error::NoMemory => f.write_str("out of memory"),
        }
    }
}

/// Memory for a controller's structures (its queue heads and transfer
/// descriptors), in slots of [`SLOT`] bytes, each aligned to its size.
struct Pool {
    memory: Dma,
    /// A bit for each slot in use.
    used: [u64; POOL_WORDS],
    slots: usize,
}

/// The bytes of a slot: room for EHCI's largest structure (a queue head
/// with 64-bit buffer pointers, 68 bytes), aligned as every controller's
/// structures need (at most 32 bytes).
const SLOT: usize = 128;
/// The pool's pages, and the words of its bitmap.
const POOL_PAGES: usize = 8;
const POOL_WORDS: usize = POOL_PAGES * radix_hw::PAGE_SIZE / SLOT / 64;

impl Pool {
    fn new(alloc: &mut dyn FnMut(usize, bool) -> Option<Dma>) -> Option<Pool> {
        let memory = alloc(POOL_PAGES, true)?;
        memory.zero();
        let slots = (memory.len() / SLOT).min(POOL_WORDS * 64);
        Some(Pool { memory, used: [0; POOL_WORDS], slots })
    }

    /// A free slot, zeroed.
    fn take(&mut self) -> Option<Slot> {
        let index = (0..self.slots).find(|&i| self.used[i / 64] & (1 << (i % 64)) == 0)?;
        self.used[index / 64] |= 1 << (index % 64);
        let slot = Slot(index as u16);
        self.memory.write(slot.offset(), &[0; SLOT]);
        Some(slot)
    }

    /// Gives `slot` back.
    fn give(&mut self, slot: Slot) {
        let index = usize::from(slot.0);
        self.used[index / 64] &= !(1 << (index % 64));
    }

    /// The physical address of `slot`.
    fn phys(&self, slot: Slot) -> u32 {
        // The pool is below 4 GiB.
        (self.memory.phys() + slot.offset() as u64) as u32
    }

    /// The slot at physical address `phys`, if it is one of the pool's.
    fn slot_at(&self, phys: u32) -> Option<Slot> {
        let offset = phys.wrapping_sub(self.memory.phys() as u32) as usize;
        (offset < self.slots * SLOT && offset.is_multiple_of(SLOT))
            .then_some(Slot((offset / SLOT) as u16))
    }

    fn read32(&self, slot: Slot, offset: usize) -> u32 {
        self.memory.read32(slot.offset() + offset)
    }

    fn write32(&self, slot: Slot, offset: usize, value: u32) {
        self.memory.write32(slot.offset() + offset, value);
    }
}

/// A slot of a [`Pool`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot(u16);

impl Slot {
    fn offset(self) -> usize {
        usize::from(self.0) * SLOT
    }
}
