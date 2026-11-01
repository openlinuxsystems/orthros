// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel's Ethernet controllers (ADR 0197): the PCI cards and the chips
//! on motherboards that FreeBSD's em, igb, igc and ix drivers drive, and
//! Linux's e1000, e1000e, igb, igc and ixgbe.
//!
//! - [`em`]: the 8254x gigabit controllers (Linux's e1000), which QEMU,
//!   VirtualBox and VMware emulate as their `e1000`, and the PCI Express
//!   82571, 82572, 82573, 82574 and 82583 (e1000e's), which QEMU emulates
//!   as `e1000e`. Their descriptors are the legacy 16-byte ones.
//! - [`igb`]: the 82576 (which QEMU emulates as its `igb`), 82580, I350,
//!   I210 and I211, on servers' cards and many boards, with advanced
//!   descriptors.
//! - [`igc`]: the I225 and I226, at up to 2.5 Gb/s, on igb's code with
//!   the few registers that moved.
//! - [`ixgbe`]: the 10 gigabit 82599, X540 and X550, with registers of
//!   their own and igb's advanced descriptors.
//!
//! Each driver is written from Intel's datasheets, with FreeBSD's drivers
//! (BSD-3-Clause, Intel's shared code) as the reference for the sequences
//! and errata the datasheets leave to their specification updates
//! (credits/freebsd-network-drivers.md). Each card is driven with one
//! receive and one transmit queue, its frames copied through
//! [`radix_nic::Slots`], no offloads and one interrupt for everything,
//! or none, polled, when the kernel has none for it.
//!
//! [`identify`] says whether a PCI function is one of them, and [`start`]
//! resets it and sets it running.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod em;
pub mod igb;
pub mod igc;
pub mod ixgbe;
mod rings;
mod shared;

use radix_hw::{Dma, Mmio};
pub use radix_nic::Interrupt;
use radix_nic::{Error, Info, Link, Nic};

use crate::rings::Rings;

/// Intel's PCI vendor ID.
pub const VENDOR: u16 = 0x8086;

/// A card the drivers know, from its device ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// What Intel calls it.
    pub name: &'static str,
    /// Which driver drives it, and how.
    pub kind: Kind,
}

impl Model {
    /// Whether the kernel should give it an MSI, if it has one. The
    /// 8254x, on PCI, are left polled: Linux's e1000 never uses their
    /// MSI either, and only INTx is known to work on them.
    pub fn msi(&self) -> bool {
        match self.kind {
            Kind::Em(mac) => mac.pcie(),
            Kind::Igb(_) | Kind::Igc(_) | Kind::Ixgbe(_) => true,
        }
    }
}

/// The families of cards, each with its own driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The e1000 and e1000e family ([`em`]).
    Em(em::Mac),
    /// The igb family ([`igb`]).
    Igb(igb::Mac),
    /// The igc family ([`igc`]).
    Igc(igc::Mac),
    /// The ixgbe family ([`ixgbe`]).
    Ixgbe(ixgbe::Mac),
}

/// The card with Intel's device ID `device`, if a driver here knows it.
/// Only copper cards are taken from the gigabit families: their fiber and
/// SerDes ones set their links up differently, which nothing here does
/// yet. The 10 gigabit 82599's SFP+ cages are taken, their links as the
/// NVM has them ([`ixgbe`]).
pub fn identify(vendor: u16, device: u16) -> Option<Model> {
    if vendor != VENDOR {
        return None;
    }
    em::identify(device)
        .map(|(name, mac)| Model { name, kind: Kind::Em(mac) })
        .or_else(|| igb::identify(device).map(|(name, mac)| Model { name, kind: Kind::Igb(mac) }))
        .or_else(|| igc::identify(device).map(|(name, mac)| Model { name, kind: Kind::Igc(mac) }))
        .or_else(|| {
            ixgbe::identify(device).map(|(name, mac)| Model { name, kind: Kind::Ixgbe(mac) })
        })
}

/// Resets the card `model`, whose registers are `regs` (its first BAR),
/// and sets it running, with interrupts as `interrupt` says. `fallback_mac`
/// is its address if it has none that can be read; `alloc` gives zeroed
/// DMA memory in pages, and `delay` waits the given number of
/// microseconds.
pub fn start(
    model: Model,
    regs: Mmio,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
) -> Result<Card, Error> {
    match model.kind {
        Kind::Em(mac) => em::start(model.name, mac, regs, interrupt, fallback_mac, alloc, delay),
        Kind::Igb(mac) => igb::start(model.name, mac, regs, interrupt, fallback_mac, alloc, delay),
        Kind::Igc(_) => igc::start(model.name, regs, interrupt, fallback_mac, alloc, delay),
        Kind::Ixgbe(mac) => {
            ixgbe::start(model.name, mac, regs, interrupt, fallback_mac, alloc, delay)
        }
    }
}

/// What tells one family's running card from another's.
struct Ops {
    /// The interrupt cause register, which reading acknowledges.
    icr: usize,
    /// The link, as the card's registers say.
    link: fn(&Mmio) -> Link,
    /// Stops the card: its interrupts off, then its DMA.
    halt: fn(&Mmio),
}

/// A card set up and running: one queue pair, as every driver here
/// drives it.
pub struct Card {
    regs: Mmio,
    rings: Rings,
    info: Info,
    interrupt: Interrupt,
    ops: &'static Ops,
    /// Whether it was stopped.
    stopped: bool,
}

impl Card {
    fn new(
        model: &'static str,
        mac: [u8; 6],
        regs: Mmio,
        rings: Rings,
        interrupt: Interrupt,
        ops: &'static Ops,
    ) -> Card {
        let (receive, transmit) = rings.sizes();
        let info = Info {
            model,
            mac,
            // One receive descriptor is always the card's tail, and one
            // transmit descriptor tells a full ring from an empty one.
            receive_slots: (receive - 1) as u16,
            transmit_slots: (transmit - 1) as u16,
        };
        Card { regs, rings, info, interrupt, ops, stopped: false }
    }
}

impl Nic for Card {
    fn info(&self) -> Info {
        self.info
    }

    fn link(&self) -> Link {
        (self.ops.link)(&self.regs)
    }

    fn failed(&self) -> bool {
        // A card that is gone reads as all ones.
        self.regs.read32(shared::reg::STATUS) == u32::MAX
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.stopped {
            return None;
        }
        if let Some(len) = self.rings.receive(frame) {
            return Some(len);
        }
        if self.interrupt == Interrupt::Msi {
            // Acknowledge the interrupt, then look again for a frame that
            // came in between.
            let _ = self.regs.read32(self.ops.icr);
            if self.rings.received() {
                return self.rings.receive(frame);
            }
        }
        None
    }

    fn can_send(&mut self) -> bool {
        !self.stopped && self.rings.can_send()
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        !self.stopped && self.rings.send(frame)
    }

    fn flush(&mut self) {
        if !self.stopped {
            self.rings.flush();
        }
    }

    fn dropped(&self) -> u64 {
        self.rings.dropped()
    }

    fn stop(&mut self) {
        if !self.stopped {
            self.stopped = true;
            (self.ops.halt)(&self.regs);
        }
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
