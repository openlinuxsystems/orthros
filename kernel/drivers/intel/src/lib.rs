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
mod legacy;

use radix_hw::{Dma, Mmio};
use radix_nic::{Error, Info, Link, Nic};

/// Intel's PCI vendor ID.
pub const VENDOR: u16 = 0x8086;

/// The interrupt the kernel set up for a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interrupt {
    /// None: the kernel looks at the card from time to time, and the
    /// card raises nothing.
    Polled,
    /// A message-signalled interrupt (MSI), which the card raises for
    /// every cause it has.
    Msi,
}

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
        }
    }
}

/// The families of cards, each with its own driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The e1000 and e1000e family ([`em`]).
    Em(em::Mac),
}

/// The card with Intel's device ID `device`, if a driver here knows it.
/// Only copper cards are taken: the fiber and SerDes ones set their links
/// up differently, which nothing here does yet.
pub fn identify(vendor: u16, device: u16) -> Option<Model> {
    if vendor != VENDOR {
        return None;
    }
    em::identify(device).map(|(name, mac)| Model { name, kind: Kind::Em(mac) })
}

/// A card set up and running.
pub enum Card {
    /// An e1000 or e1000e.
    Em(em::Em),
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
        Kind::Em(mac) => {
            em::Em::start(model.name, mac, regs, interrupt, fallback_mac, alloc, delay)
                .map(Card::Em)
        }
    }
}

impl Card {
    fn nic(&self) -> &dyn Nic {
        match self {
            Card::Em(card) => card,
        }
    }

    fn nic_mut(&mut self) -> &mut dyn Nic {
        match self {
            Card::Em(card) => card,
        }
    }
}

impl Nic for Card {
    fn info(&self) -> Info {
        self.nic().info()
    }

    fn link(&self) -> Link {
        self.nic().link()
    }

    fn failed(&self) -> bool {
        self.nic().failed()
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        self.nic_mut().receive(frame)
    }

    fn can_send(&mut self) -> bool {
        self.nic_mut().can_send()
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        self.nic_mut().send(frame)
    }

    fn flush(&mut self) {
        self.nic_mut().flush();
    }

    fn dropped(&self) -> u64 {
        self.nic().dropped()
    }

    fn stop(&mut self) {
        self.nic_mut().stop();
    }
}

#[cfg(test)]
mod tests;
