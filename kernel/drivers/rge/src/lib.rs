// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Realtek's 2.5 and 5 gigabit Ethernet controllers (ADR 0197): the
//! RTL8125A, RTL8125B and RTL8125D at 2.5 Gb/s, and the RTL8126A at
//! 5 Gb/s, on desktop boards, cards and laptops, among them Killer's
//! E3000 and E5000 and the Framework Desktop's port. OpenBSD's and
//! FreeBSD's `rge(4)` drive them, and Linux's `r8169`.
//!
//! Two PCI device IDs, and Killer's two, stand for the chips, which the
//! revision in their transmit configuration register tells apart
//! ([`Kind`]); the driver refuses a revision it does not know, and the
//! RTL8127, whose 10 gigabit chip it leaves out. It sets the chip up as
//! OpenBSD's does ([`setup`], translated from OpenBSD's ISC-licensed
//! code), without the microcode Realtek ships for the chips' MAC and PHY
//! processors, which is firmware under Realtek's own licence: the chips
//! then run on the code in their ROMs, as Linux runs them without its
//! `rtl_nic` files. The registers behind the chip's own, the MAC's and
//! the PHY's, are reached through [`ocp`].
//!
//! Frames are copied through 256 slots each way ([`rings`]), with no
//! offloads, no VLAN tag stripping, and every multicast frame taken,
//! which IPv6 and mDNS need. With an MSI the chip raises it for frames
//! received and sent, for errors and for link changes; writing the
//! causes back to its interrupt status register, which the driver does
//! once no frame is left to take, acknowledges them. The link is what
//! the chip's PHY status register says. A chip that holds frames for
//! five seconds without sending any, as OpenBSD's watchdog finds, or
//! says it had a PCI error, has failed.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod ocp;
mod reg;
mod rings;
mod setup;

use radix_hw::{Dma, Mmio};
pub use radix_nic::Interrupt;
use radix_nic::{Error, Info, Link, Nic, usable_mac};

use crate::ocp::Ocp;
use crate::reg::{isr, phystat};
use crate::rings::Rings;

/// Realtek's PCI vendor ID, which Killer's cards have too.
pub const VENDOR: u16 = 0x10ec;

/// The descriptors in each ring.
const RECEIVE: usize = 256;
const TRANSMIT: usize = 256;

/// How long frames may wait to be sent before the chip is taken to have
/// stopped: the five seconds of OpenBSD's watchdog (its `if_timer`).
const STALL_NS: u64 = 5_000_000_000;

/// The chip, by the revision in its transmit configuration register:
/// FreeBSD's and OpenBSD's `MAC_R25`, `MAC_R25B`, `MAC_R25D_1`... types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The RTL8125A.
    R25,
    /// The RTL8125B.
    R25B,
    /// The RTL8125D's first revision.
    R25D1,
    /// The RTL8125D's second.
    R25D2,
    /// The RTL8126A's first revision.
    R26_1,
    /// The RTL8126A's second.
    R26_2,
}

impl Kind {
    /// The chip whose transmit configuration register reads `txcfg`.
    pub fn from_txcfg(txcfg: u32) -> Option<Kind> {
        match txcfg & reg::TXCFG_HWREV {
            0x6090_0000 => Some(Kind::R25),
            0x6410_0000 => Some(Kind::R25B),
            0x6880_0000 => Some(Kind::R25D1),
            0x6890_0000 => Some(Kind::R25D2),
            0x6490_0000 => Some(Kind::R26_1),
            0x64a0_0000 => Some(Kind::R26_2),
            _ => None,
        }
    }

    /// Its name, as Realtek's.
    pub fn name(self) -> &'static str {
        match self {
            Kind::R25 => "Realtek RTL8125A",
            Kind::R25B => "Realtek RTL8125B",
            Kind::R25D1 | Kind::R25D2 => "Realtek RTL8125D",
            Kind::R26_1 | Kind::R26_2 => "Realtek RTL8126A",
        }
    }

    /// Whether it is an RTL8125D: `RGE_TYPE_R25D`.
    fn r25d(self) -> bool {
        matches!(self, Kind::R25D1 | Kind::R25D2)
    }

    /// Whether it is an RTL8126A, at 5 Gb/s: `RGE_TYPE_R26`.
    fn r26(self) -> bool {
        matches!(self, Kind::R26_1 | Kind::R26_2)
    }
}

/// A card this driver knows, by its PCI device ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// Its maker's name for it, when it is not the chip's: Killer's.
    pub name: Option<&'static str>,
}

/// The cards: Realtek's two device IDs, and Killer's.
const CARDS: &[(u16, Option<&str>)] = &[
    (0x8125, None),
    (0x8126, None),
    (0x3000, Some("Killer E3000")),
    (0x5000, Some("Killer E5000")),
];

/// The model of the card with PCI IDs `vendor` and `device`, if this
/// driver knows it.
pub fn identify(vendor: u16, device: u16) -> Option<Model> {
    if vendor != VENDOR {
        return None;
    }
    CARDS.iter().find(|&&(id, _)| id == device).map(|&(_, name)| Model { name })
}

impl Model {
    /// Whether it should have an MSI: all of these do.
    pub fn msi(&self) -> bool {
        true
    }
}

/// Waits `us` microseconds by `clock`, the time in nanoseconds.
fn spin(clock: fn() -> u64) -> impl FnMut(u32) {
    move |us| {
        let until = clock().saturating_add(u64::from(us) * 1_000);
        while clock() < until {
            core::hint::spin_loop();
        }
    }
}

/// Resets the card of `model` and sets it running, with interrupts as
/// `interrupt` says. `regs` is its first memory BAR, which must reach
/// 64 KiB, and `express` where its PCI Express capability is in its
/// configuration space, if it has one. `fallback_mac` is its address if
/// it has none that can be read; `alloc` gives zeroed DMA memory in
/// pages; `delay` waits the given number of microseconds; and `clock` is
/// the time in nanoseconds, by which the running card stops and finds
/// that it does not send.
#[expect(clippy::too_many_arguments, reason = "what the kernel knows of the card")]
pub fn start(
    model: Model,
    regs: Mmio,
    express: Option<usize>,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
    clock: fn() -> u64,
) -> Result<Card, Error> {
    if regs.len() < reg::END {
        return Err(Error::Registers);
    }
    let txcfg = regs.read32(reg::TXCFG);
    if txcfg == u32::MAX {
        return Err(Error::Device("its registers read as all ones"));
    }
    let kind = Kind::from_txcfg(txcfg).ok_or(Error::Unsupported("this revision of the chip"))?;
    let mut c = Ocp::new(&regs, kind, delay);
    // As OpenBSD's rge_attach() and rge_attachhook() do.
    if let Some(express) = express {
        setup::disable_aspm(&mut c, express);
    }
    let mac = setup::mac_address(&c);
    let mac = if usable_mac(mac) { mac } else { fallback_mac };
    setup::set_mac_address(&mut c, mac);
    setup::chip_init(&mut c)?;
    // Then as its rge_init() does: stopped, the rings, the chip set up
    // again, the PHY configured, the MAC set up and started.
    setup::stop(&mut c);
    setup::set_mac_address(&mut c, mac);
    let rings = Rings::new(regs.clone(), RECEIVE, TRANSMIT, alloc)?;
    setup::chip_init(&mut c)?;
    setup::phy_config(&mut c)?;
    let (receive, transmit) = rings.addresses();
    setup::init(&mut c, receive, transmit);
    // The interrupts, without moderation (rge_setup_intr() with
    // RGE_IMTYPE_NONE), or none.
    if interrupt == Interrupt::Msi {
        regs.write32(reg::IMR, isr::RAISED);
        regs.write32(reg::TIMERINT0, 0);
        regs.write16(reg::IM, 0);
    } else {
        regs.write32(reg::IMR, 0);
    }
    let name = model.name.unwrap_or(kind.name());
    Ok(Card::new(kind, name, mac, regs, rings, interrupt, clock))
}

/// A card set up and running.
pub struct Card {
    regs: Mmio,
    kind: Kind,
    rings: Rings,
    info: Info,
    interrupt: Interrupt,
    clock: fn() -> u64,
    /// Since when frames have waited to be sent with none sent, and how
    /// many had been sent then.
    stalled_since: Option<u64>,
    sent: u64,
    stopped: bool,
}

impl Card {
    fn new(
        kind: Kind,
        name: &'static str,
        mac: [u8; 6],
        regs: Mmio,
        rings: Rings,
        interrupt: Interrupt,
        clock: fn() -> u64,
    ) -> Card {
        let (receive, transmit) = rings.sizes();
        let info = Info {
            model: name,
            mac,
            receive_slots: receive as u16,
            // One transmit descriptor tells a full ring from an empty one.
            transmit_slots: (transmit - 1) as u16,
        };
        Card {
            regs,
            kind,
            rings,
            info,
            interrupt,
            clock,
            stalled_since: None,
            sent: 0,
            stopped: false,
        }
    }

    /// The chip.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Acknowledges the causes of the chip's interrupt, so that the next
    /// one raises it again: writing them back clears them, and the MSI is
    /// raised only when a cause comes with none left. A PCI error is left
    /// for [`Nic::failed`] to see.
    fn acknowledge(&self) {
        for _ in 0..4 {
            let causes = self.regs.read32(reg::ISR);
            if causes == u32::MAX {
                break;
            }
            let causes = causes & !isr::SYSTEM_ERR;
            if causes == 0 {
                break;
            }
            self.regs.write32(reg::ISR, causes);
        }
    }

    /// Notes whether the chip sends: frames that wait with none sent
    /// start the clock [`Nic::failed`] reads, as OpenBSD's watchdog does.
    fn watch_sending(&mut self) {
        let (sent, waiting) = self.rings.progress();
        if sent != self.sent || !waiting {
            self.stalled_since = None;
        } else if self.stalled_since.is_none() {
            self.stalled_since = Some((self.clock)());
        }
        self.sent = sent;
    }
}

impl Nic for Card {
    fn info(&self) -> Info {
        self.info
    }

    fn link(&self) -> Link {
        let status = self.regs.read16(reg::PHYSTAT);
        if status == u16::MAX || status & phystat::LINK == 0 {
            return Link::default();
        }
        let mbps = if status & phystat::MBPS_5000 != 0 {
            5000
        } else if status & phystat::MBPS_2500 != 0 {
            2500
        } else if status & phystat::MBPS_1000 != 0 {
            1000
        } else if status & phystat::MBPS_100 != 0 {
            100
        } else if status & phystat::MBPS_10 != 0 {
            10
        } else {
            0
        };
        Link { up: true, mbps, full_duplex: status & phystat::FDX != 0 }
    }

    fn failed(&self) -> bool {
        // A card that is gone reads as all ones.
        let causes = self.regs.read32(reg::ISR);
        if causes == u32::MAX || causes & isr::SYSTEM_ERR != 0 {
            return true;
        }
        self.stalled_since.is_some_and(|since| (self.clock)().wrapping_sub(since) >= STALL_NS)
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.stopped {
            return None;
        }
        if let Some(len) = self.rings.receive(frame) {
            return Some(len);
        }
        self.watch_sending();
        if self.interrupt == Interrupt::Msi {
            // Acknowledge the interrupt, then look again for a frame that
            // came in between.
            self.acknowledge();
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
            let mut delay = spin(self.clock);
            setup::stop(&mut Ocp::new(&self.regs, self.kind, &mut delay));
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
