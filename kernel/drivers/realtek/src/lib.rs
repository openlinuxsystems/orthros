// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Realtek's gigabit and fast Ethernet controllers (ADR 0197): the PCI
//! RTL8169 and RTL8110, and the PCI Express RTL8168 and RTL8111 and the
//! RTL810x at 10 and 100 Mb/s (the RTL8100E to RTL8107E), on most
//! desktop and laptop boards that are not Intel's, the Dell Inspiron 15
//! 5570's among them. FreeBSD's `re(4)` drives them, and Linux's
//! `r8169`.
//!
//! One PCI device ID stands for many chips: the driver tells them apart
//! by the revision in their transmit configuration register
//! ([`chip`]), and refuses one it does not know. It resets the chip,
//! takes it out of the sleeps its revision may be left in, powers its
//! PHY up and lets it negotiate the link, reads the MAC address the chip
//! loaded from its EEPROM, and drives it in C+ mode with one ring of
//! 256 descriptors each way ([`rings`]): the receive buffers are
//! [`SLOT`](radix_nic::SLOT) bytes, frames are padded to 60 bytes in
//! their slots, the card takes every multicast frame, which IPv6 and
//! mDNS need, and broadcasts, and there are no offloads and no VLAN tag
//! stripping. With an MSI the card raises it for frames received and
//! sent, for errors and for link changes; writing the causes back to its
//! interrupt status register, which the driver does once no frame is
//! left to take, acknowledges them. The PCI RTL8169s are polled, as
//! Linux leaves them on INTx, which the kernel does not give.
//!
//! The driver is written from FreeBSD's `re(4)` (BSD-4-Clause, Bill Paul)
//! as the reference for the registers, the revisions and their quirks
//! (credits/freebsd-network-drivers.md); no code is taken from it. It
//! loads no firmware and no PHY parameters, as FreeBSD's does not.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod chip;
mod rings;

use radix_hw::{Dma, Mmio};
pub use radix_nic::Interrupt;
use radix_nic::{Error, Info, Link, Nic, usable_mac, wait_for};

use crate::chip::{Chip, needs};
use crate::rings::Rings;

/// Realtek's PCI vendor ID.
pub const VENDOR: u16 = 0x10ec;
/// The RTL810x family's device ID, which the RTL8106EUS and RTL8107E
/// share with chips of the RTL8168's revisions.
pub const DEVICE_8101E: u16 = 0x8136;

/// The registers, from the start of the chip's first memory BAR.
mod reg {
    pub const IDR0: usize = 0x00;
    pub const IDR4: usize = 0x04;
    pub const MAR0: usize = 0x08;
    pub const MAR4: usize = 0x0c;
    pub const TXLIST_LO: usize = 0x20;
    pub const TXLIST_HI: usize = 0x24;
    pub const COMMAND: usize = 0x37;
    /// The gigabit chips' transmit poll register.
    pub const TXPOLL: usize = 0x38;
    pub const IMR: usize = 0x3c;
    pub const ISR: usize = 0x3e;
    pub const TXCFG: usize = 0x40;
    pub const RXCFG: usize = 0x44;
    pub const MISSED: usize = 0x4c;
    pub const EECMD: usize = 0x50;
    pub const CFG2: usize = 0x53;
    pub const CFG3: usize = 0x54;
    pub const CFG4: usize = 0x55;
    pub const TIMERINT: usize = 0x58;
    pub const PHYAR: usize = 0x60;
    pub const GMEDIASTAT: usize = 0x6c;
    pub const MACDBG: usize = 0x6d;
    pub const GPIO: usize = 0x6e;
    pub const PMCH: usize = 0x6f;
    pub const SC_MAGIC: usize = 0x7c;
    pub const MAC_RESET: usize = 0x82;
    pub const RTL8401E_PHY: usize = 0xd1;
    pub const MCU: usize = 0xd3;
    pub const MAXRXPKTLEN: usize = 0xda;
    pub const CPLUS: usize = 0xe0;
    pub const INTRMOD: usize = 0xe2;
    pub const RXLIST_LO: usize = 0xe4;
    pub const RXLIST_HI: usize = 0xe8;
    pub const EARLY_TX: usize = 0xec;
    pub const MISC: usize = 0xf0;
    /// The last register the driver touches, and its size.
    pub const END: usize = MISC + 4;
}

/// The command register's bits.
mod command {
    pub const TX_ENABLE: u8 = 0x04;
    pub const RX_ENABLE: u8 = 0x08;
    pub const RESET: u8 = 0x10;
    pub const STOP_REQUEST: u8 = 0x80;
}

/// The interrupt status and mask registers' bits.
mod isr {
    /// Received, a receive error, sent, a transmit error, out of receive
    /// descriptors, the link changed, the receive FIFO overflowed: what
    /// the MSI is raised for.
    pub const RAISED: u16 = 0x007f;
    /// A PCI error: the chip does nothing more until it is set up again.
    pub const SYSTEM_ERROR: u16 = 0x8000;
}

/// The receive configuration: no FIFO threshold, unlimited DMA bursts,
/// and frames to the card's address, to broadcast and to every multicast
/// group.
mod rxcfg {
    pub const BASE: u32 = 0xe000 | 0x0700 | 0x1800;
    pub const ALL: u32 = 0x01;
    pub const UNICAST: u32 = 0x02;
    pub const MULTICAST: u32 = 0x04;
    pub const BROADCAST: u32 = 0x08;
    pub const EARLY_OFF: u32 = 0x3800;
    pub const EARLY_OFF_V2: u32 = 0x0800;
}

/// The transmit configuration: the standard interframe gap and DMA
/// bursts of 2048 bytes.
const TXCFG: u32 = 0x0300_0000 | 0x0700;
/// The transmit configuration's bit that says its queue is empty, on the
/// RTL8168E-VL and later.
const TXCFG_QUEUE_EMPTY: u32 = 0x0800;
/// The EEPROM command register's values: configuration writes allowed,
/// and locked again.
const EECMD_WRITE_CONFIG: u8 = 0xc0;
const EECMD_OFF: u8 = 0x00;
/// The C+ command register's bits: C+ transmit and receive, and PCI
/// multiple reads and writes; the RTL8168B and later's MAC statistics
/// off.
const CPLUS_TX: u16 = 0x0001;
const CPLUS_RX: u16 = 0x0002;
const CPLUS_MULTIPLE: u16 = 0x0008;
const CPLUS_NO_MAC_STAT: u16 = 0x0080;
/// The interrupt moderation the vendor's drivers set.
const INTRMOD: u16 = 0x5100;
/// MISC's RXDV gate, on the RTL8168G and later.
const RXDV_GATE: u32 = 0x0008_0000;
/// The MCU register's bits: the transmit and receive FIFOs are empty.
const MCU_FIFOS_EMPTY: u8 = 0x20 | 0x10;
/// The longest frame the older gigabit chips are told to take: any,
/// in several buffers, which the driver drops.
const MAX_RX_ANY: u16 = 16383;

/// The PHY access register's bits: the register, its data, and the bit
/// that starts a write and says a read is done.
const PHYAR_FLAG: u32 = 0x8000_0000;
const PHYAR_DATA: u32 = 0xffff;
/// The PHY's registers this driver uses: its control, and the page
/// select and power register Realtek's PHYs have.
const PHY_CONTROL: u32 = 0x00;
const PHY_POWER: u32 = 0x0e;
const PHY_PAGE: u32 = 0x1f;
const PHY_8169S_FIX: u32 = 0x0b;
/// PHY control's bits: autonegotiation restarted, powered down,
/// autonegotiation on.
const PHY_RESTART_AUTONEG: u16 = 1 << 9;
const PHY_POWER_DOWN: u16 = 1 << 11;
const PHY_AUTONEG: u16 = 1 << 12;

/// GMEDIASTAT's bits.
mod media {
    pub const FULL_DUPLEX: u8 = 0x01;
    pub const LINK: u8 = 0x02;
    pub const MBPS_10: u8 = 0x04;
    pub const MBPS_100: u8 = 0x08;
    pub const MBPS_1000: u8 = 0x10;
}

/// How long things take.
const RESET_US: u32 = 10_000;
const PHY_US: u32 = 50_000;
const PHY_STEP_US: u32 = 25;
/// The time the chip needs between two PHY accesses.
const PHY_GAP_US: u32 = 20;
/// How many times a stopping chip is looked at before the driver goes
/// on: [`Nic::stop`] cannot sleep, so it waits by reading registers,
/// each a microsecond or so on a PCI Express bus.
const STOP_READS: u32 = 100_000;

/// The descriptors in each ring.
const RECEIVE: usize = 256;
const TRANSMIT: usize = 256;

/// A card the driver knows, from its PCI IDs: its family's name, as the
/// chip's own is known only once its registers are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// The family: "RTL8168", "RTL810xE".
    pub name: &'static str,
    /// Whether it is on PCI Express, where it has an MSI.
    pub pcie: bool,
}

impl Model {
    /// Whether the kernel should give it an MSI, if it has one: the PCI
    /// Express chips' MSIs work, and the PCI RTL8169's are left alone,
    /// as Linux's driver leaves them.
    pub fn msi(&self) -> bool {
        self.pcie
    }
}

/// Linksys's EG1032, whose third revision alone is a Realtek chip.
const LINKSYS_EG1032_REV3: u16 = 0x0024;

/// The cards of the chips this driver drives: Realtek's own, and other
/// makers' with them, as FreeBSD's `re(4)` lists them.
const CARDS: &[(u16, u16, &str, bool)] = &[
    (VENDOR, 0x8169, "RTL8169", false),
    (VENDOR, 0x8167, "RTL8169SC", false),
    (VENDOR, 0x8168, "RTL8168", true),
    (VENDOR, 0x8161, "RTL8168", true),
    (VENDOR, DEVICE_8101E, "RTL810xE", true),
    (VENDOR, 0x2600, "Killer E2600", true),
    // D-Link's DGE-528T and DGE-530T revision C.
    (0x1186, 0x4300, "RTL8169", false),
    (0x1186, 0x4302, "RTL8169", false),
    // TP-Link's TG-3468 version 2.
    (0x10ff, 0x8168, "RTL8168", true),
    // Corega's CG-LAPCIGT, Linksys's EG1032 and US Robotics' 997902.
    (0x1259, 0xc107, "RTL8169S", false),
    (0x1737, 0x1032, "RTL8169S", false),
    (0x16ec, 0x0116, "RTL8169S", false),
];

/// The card with these PCI IDs, if the driver knows it: `subsystem` is
/// its subsystem's device ID, which tells a Linksys EG1032 that is a
/// Realtek chip from one that is not.
pub fn identify(vendor: u16, device: u16, subsystem: u16) -> Option<Model> {
    if (vendor, device) == (0x1737, 0x1032) && subsystem != LINKSYS_EG1032_REV3 {
        return None;
    }
    CARDS
        .iter()
        .find(|&&(v, d, ..)| (v, d) == (vendor, device))
        .map(|&(_, _, name, pcie)| Model { name, pcie })
}

/// Resets the card of PCI device ID `device`, whose registers are `regs`
/// (its first memory BAR), and sets it running, with interrupts as
/// `interrupt` says. `fallback_mac` is its address if it
/// has none that can be read; `alloc` gives zeroed DMA memory in pages,
/// below 4 GiB, as the older chips cannot reach above it; and `delay`
/// waits the given number of microseconds.
pub fn start(
    device: u16,
    regs: Mmio,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
) -> Result<Card, Error> {
    if regs.len() < reg::END {
        return Err(Error::Registers);
    }
    let txcfg = regs.read32(reg::TXCFG);
    if txcfg == u32::MAX {
        return Err(Error::Device("its registers read as all ones"));
    }
    let chip =
        chip::identify(txcfg, device).ok_or(Error::Unsupported("this revision of the chip"))?;
    reset(&regs, &chip, delay)?;
    wake(&regs, &chip, delay);
    power_up_phy(&regs, delay);
    let mac = registers_mac(&regs).unwrap_or(fallback_mac);
    // Set up from here as FreeBSD's re_init_locked() does: stopped and
    // reset again, the rings, then the registers in their order.
    halt(&regs, &chip);
    reset(&regs, &chip, delay)?;
    if chip.needs(needs::JUMBO_V2) {
        jumbo_off(&regs, &chip);
    }
    let rings = Rings::new(regs.clone(), RECEIVE, TRANSMIT, alloc)?;
    let cplus = CPLUS_MULTIPLE
        | if chip.needs(needs::MAC_STAT) {
            CPLUS_NO_MAC_STAT | CPLUS_TX
        } else {
            CPLUS_RX | CPLUS_TX
        };
    regs.write16(reg::CPLUS, cplus);
    if matches!(chip.rev, chip::RTL8169_8110SC | chip::RTL8169_8110SCE) {
        let mut magic = 0x000f_ff00;
        if regs.read8(reg::CFG2) & 0x01 != 0 {
            // On a 66 MHz bus.
            magic |= 0xff;
        }
        if chip.rev == chip::RTL8169_8110SCE {
            magic |= 0x00f0_0000;
        }
        regs.write32(reg::SC_MAGIC, magic);
    }
    set_mac(&regs, mac);
    if chip.needs(needs::G_PLUS) {
        regs.write32(reg::MISC, regs.read32(reg::MISC) & !RXDV_GATE);
    } else {
        regs.write8(reg::COMMAND, command::TX_ENABLE | command::RX_ENABLE);
    }
    regs.write32(reg::TXCFG, TXCFG);
    regs.write8(reg::EARLY_TX, 16);
    regs.write32(reg::MAR0, u32::MAX);
    regs.write32(reg::MAR4, u32::MAX);
    let mut receive = rxcfg::BASE | rxcfg::UNICAST | rxcfg::BROADCAST | rxcfg::MULTICAST;
    if chip.needs(needs::EARLY_OFF) {
        receive |= rxcfg::EARLY_OFF;
    } else if chip.needs(needs::G_PLUS) {
        receive |= rxcfg::EARLY_OFF_V2;
    }
    regs.write32(reg::RXCFG, receive);
    regs.write16(reg::INTRMOD, INTRMOD);
    if chip.needs(needs::G_PLUS) {
        regs.write8(reg::COMMAND, command::TX_ENABLE | command::RX_ENABLE);
    }
    let mask = if interrupt == Interrupt::Msi { isr::RAISED } else { 0 };
    regs.write16(reg::IMR, mask);
    regs.write16(reg::ISR, u16::MAX);
    regs.write32(reg::MISSED, 0);
    regs.write32(reg::TIMERINT, 0);
    // Frames no longer than a slot where the chip can be told so; the
    // older gigabit chips take any, in several buffers.
    let longest =
        if chip.needs(needs::JUMBO_V2 | needs::FAST) { radix_nic::SLOT as u16 } else { MAX_RX_ANY };
    regs.write16(reg::MAXRXPKTLEN, longest);
    Ok(Card::new(chip, mac, regs, rings, interrupt))
}

/// Resets the chip, as its revision needs.
fn reset(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
    regs.write8(reg::COMMAND, command::RESET);
    let done = wait_for(delay, RESET_US, 10, || regs.read8(reg::COMMAND) & command::RESET == 0);
    if !done {
        return Err(Error::Timeout("reset"));
    }
    if chip.needs(needs::MAC_RESET) {
        regs.write8(reg::MAC_RESET, 1);
    }
    if chip.rev == chip::RTL8169S {
        let _ = phy_write(regs, PHY_8169S_FIX, 0, delay);
    }
    Ok(())
}

/// Takes the chip out of its deep sleep and its PHY out of power
/// management, as its revision needs.
fn wake(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    if chip.needs(needs::MAC_SLEEP) {
        let gpio = regs.read8(reg::GPIO);
        let gpio = if regs.read8(reg::MACDBG) & 0x80 != 0 { gpio | 0x01 } else { gpio & !0x01 };
        regs.write8(reg::GPIO, gpio);
    }
    if chip.needs(needs::PHY_WAKE_PM) {
        regs.write8(reg::PMCH, regs.read8(reg::PMCH) | 0x80);
        if chip.rev == chip::RTL8401E {
            regs.write8(reg::RTL8401E_PHY, regs.read8(reg::RTL8401E_PHY) & !0x08);
        }
    }
    if chip.needs(needs::PHY_WAKE) {
        let _ = phy_write(regs, PHY_PAGE, 0, delay);
        let _ = phy_write(regs, PHY_POWER, 0, delay);
    }
}

/// Turns jumbo frames off, on the chips that have them the RTL8168C's
/// way: the RTL8168E-VL's are off as long as nothing turns them on.
fn jumbo_off(regs: &Mmio, chip: &Chip) {
    if chip.rev == chip::RTL8168E_VL {
        return;
    }
    regs.write8(reg::EECMD, EECMD_WRITE_CONFIG);
    regs.write8(reg::CFG3, regs.read8(reg::CFG3) & !0x04);
    match chip.rev {
        chip::RTL8168DP => {}
        chip::RTL8168E => regs.write8(reg::CFG4, regs.read8(reg::CFG4) & !0x01),
        _ => regs.write8(reg::CFG4, regs.read8(reg::CFG4) & !0x02),
    }
    regs.write8(reg::EECMD, EECMD_OFF);
}

/// The MAC address the chip loaded from its EEPROM, if it is one a card
/// can have.
fn registers_mac(regs: &Mmio) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = regs.read8(reg::IDR0 + i);
    }
    usable_mac(mac).then_some(mac)
}

/// Programs `mac` as the card's address, which needs configuration
/// writes allowed.
fn set_mac(regs: &Mmio, mac: [u8; 6]) {
    regs.write8(reg::EECMD, EECMD_WRITE_CONFIG);
    regs.write32(reg::IDR0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
    regs.write32(reg::IDR4, u32::from(u16::from_le_bytes([mac[4], mac[5]])));
    regs.write8(reg::EECMD, EECMD_OFF);
}

/// Reads the PHY's register `phy`: the chip sets the flag once the data
/// is there.
fn phy_read(regs: &Mmio, phy: u32, delay: &mut dyn FnMut(u32)) -> Option<u16> {
    regs.write32(reg::PHYAR, phy << 16);
    let done = wait_for(delay, PHY_US, PHY_STEP_US, || regs.read32(reg::PHYAR) & PHYAR_FLAG != 0);
    let value = regs.read32(reg::PHYAR);
    delay(PHY_GAP_US);
    done.then_some((value & PHYAR_DATA) as u16)
}

/// Writes the PHY's register `phy`: the chip clears the flag once it is
/// written.
fn phy_write(regs: &Mmio, phy: u32, value: u16, delay: &mut dyn FnMut(u32)) -> bool {
    regs.write32(reg::PHYAR, PHYAR_FLAG | (phy << 16) | u32::from(value));
    let done = wait_for(delay, PHY_US, PHY_STEP_US, || regs.read32(reg::PHYAR) & PHYAR_FLAG == 0);
    delay(PHY_GAP_US);
    done
}

/// Powers the PHY up and has it negotiate the link, as it advertises
/// from its reset: on page 0, where its standard registers are.
fn power_up_phy(regs: &Mmio, delay: &mut dyn FnMut(u32)) {
    let down = regs.read8(reg::GMEDIASTAT) & media::LINK == 0;
    let _ = phy_write(regs, PHY_PAGE, 0, delay);
    if let Some(control) = phy_read(regs, PHY_CONTROL, delay)
        && (down || control & PHY_POWER_DOWN != 0 || control & PHY_AUTONEG == 0)
    {
        let control = (control & !PHY_POWER_DOWN) | PHY_AUTONEG | PHY_RESTART_AUTONEG;
        let _ = phy_write(regs, PHY_CONTROL, control, delay);
    }
}

/// Reads a register with `read` until `done` says true of what it read,
/// up to [`STOP_READS`] times.
fn spin<T>(read: impl Fn() -> T, done: impl Fn(T) -> bool) {
    for _ in 0..STOP_READS {
        if done(read()) {
            return;
        }
    }
}

/// Stops the chip: it takes no more frames, its transmitter and DMA
/// finish what they are doing, its interrupts go off, and it is reset,
/// which leaves it reaching no memory. FreeBSD's re_stop() is the
/// reference.
fn halt(regs: &Mmio, chip: &Chip) {
    let receive = regs.read32(reg::RXCFG);
    let accepts = rxcfg::ALL | rxcfg::UNICAST | rxcfg::MULTICAST | rxcfg::BROADCAST;
    regs.write32(reg::RXCFG, receive & !accepts);
    if chip.needs(needs::G_PLUS) {
        // The stop request would wedge these: the gate closed, the queue
        // and the FIFOs drained, then the MACs turned off.
        regs.write32(reg::MISC, regs.read32(reg::MISC) | RXDV_GATE);
        spin(|| regs.read32(reg::TXCFG), |txcfg| txcfg & TXCFG_QUEUE_EMPTY != 0);
        let on = regs.read8(reg::COMMAND);
        regs.write8(reg::COMMAND, on & !(command::TX_ENABLE | command::RX_ENABLE));
        spin(|| regs.read8(reg::MCU), |mcu| mcu & MCU_FIFOS_EMPTY == MCU_FIFOS_EMPTY);
    } else if chip.needs(needs::WAIT_TX_POLL) {
        spin(|| regs.read8(reg::TXPOLL), |poll| poll & 0x40 == 0);
        regs.write8(reg::COMMAND, 0);
    } else if chip.needs(needs::CMD_STOP) {
        let stop = command::STOP_REQUEST | command::TX_ENABLE | command::RX_ENABLE;
        regs.write8(reg::COMMAND, stop);
        if chip.needs(needs::CMD_STOP_WAIT_TXQ) {
            spin(|| regs.read32(reg::TXCFG), |txcfg| txcfg & TXCFG_QUEUE_EMPTY != 0);
        }
    } else {
        regs.write8(reg::COMMAND, 0);
    }
    regs.write16(reg::IMR, 0);
    regs.write16(reg::ISR, u16::MAX);
    regs.write8(reg::COMMAND, command::RESET);
    spin(|| regs.read8(reg::COMMAND), |command| command & command::RESET == 0);
}

/// A card set up and running.
pub struct Card {
    regs: Mmio,
    chip: Chip,
    rings: Rings,
    info: Info,
    interrupt: Interrupt,
    /// Whether it was stopped.
    stopped: bool,
}

impl Card {
    fn new(chip: Chip, mac: [u8; 6], regs: Mmio, rings: Rings, interrupt: Interrupt) -> Card {
        let (receive, transmit) = rings.sizes();
        let info = Info {
            model: chip.name,
            mac,
            receive_slots: receive as u16,
            // One transmit descriptor tells a full ring from an empty one.
            transmit_slots: (transmit - 1) as u16,
        };
        Card { regs, chip, rings, info, interrupt, stopped: false }
    }

    /// The chip's revision.
    pub fn chip(&self) -> Chip {
        self.chip
    }

    /// Acknowledges the causes of the card's interrupt, so that the next
    /// one raises it again: writing them back clears them, and the MSI
    /// is raised only when a cause comes with none left. A PCI error is
    /// left for [`Nic::failed`] to see.
    fn acknowledge(&self) {
        for _ in 0..4 {
            let causes = self.regs.read16(reg::ISR) & !isr::SYSTEM_ERROR;
            if causes == 0 {
                break;
            }
            self.regs.write16(reg::ISR, causes);
        }
    }
}

impl Nic for Card {
    fn info(&self) -> Info {
        self.info
    }

    fn link(&self) -> Link {
        let status = self.regs.read8(reg::GMEDIASTAT);
        if status == u8::MAX || status & media::LINK == 0 {
            return Link::default();
        }
        let mbps = if status & media::MBPS_1000 != 0 {
            1000
        } else if status & media::MBPS_100 != 0 {
            100
        } else if status & media::MBPS_10 != 0 {
            10
        } else {
            0
        };
        Link { up: true, mbps, full_duplex: status & media::FULL_DUPLEX != 0 }
    }

    fn failed(&self) -> bool {
        // A card that is gone reads as all ones.
        let causes = self.regs.read16(reg::ISR);
        causes == u16::MAX || causes & isr::SYSTEM_ERROR != 0
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
            halt(&self.regs, &self.chip);
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
