// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The igb family: Intel's 82576, 82580, I350, I210 and I211 gigabit
//! controllers on PCI Express (their datasheets), copper ones only: the
//! 82576 is what QEMU emulates as its `igb`, the I350 is on many
//! servers' cards, and the I210 and I211 on many boards.
//!
//! The driver stops the card's DMA and resets it, waits for it to read
//! its NVM and its firmware to configure it, refuses one whose link is
//! not copper, and tells the firmware a driver has it. It powers the PHY
//! up, holding the PHY's semaphore the firmware shares (SW_FW_SYNC),
//! takes the MAC address the card loaded into its first receive address
//! register, and programs it as its one unicast address, taking every
//! multicast frame and broadcasts. It drives queue pair 0, the one
//! every member has at the 8254x's addresses, with advanced descriptors
//! of one buffer each ([`rings`](crate::rings)), each written back as
//! soon as it is done. With an MSI, the card is left out of its MSI-X
//! mode and raises it, as the e1000 family does, for frames received and
//! sent and for link changes, at most 20,000 times a second.

use radix_hw::{Dma, Mmio};
use radix_nic::{Error, wait_for};

use crate::rings::{self, Format, Rings};
use crate::shared::{self, CAUSES, EECD_AUTO_RD, ctrl, rctl, status, tctl};
use crate::{Card, Interrupt, Ops};

/// The registers, from the start of BAR 0.
mod reg {
    pub use crate::shared::reg::*;
    pub const EEMNGCTL: usize = 0x1010;
    pub const GPIE: usize = 0x1514;
    pub const EITR0: usize = 0x1680;
    pub const IVAR0: usize = 0x1700;
    pub const SRRCTL0: usize = 0x280c;
    pub const RXDCTL0: usize = 0x2828;
    pub const TXDCTL0: usize = 0x3828;
    pub const SW_FW_SYNC: usize = 0x5b5c;
    /// The last register the driver touches, and its size.
    pub const END: usize = SW_FW_SYNC + 4;
}

/// CTRL_EXT's link mode: the PHY inside, on copper, is 0.
const LINK_MODE: u32 = 3 << 22;
/// EEMNGCTL's bit that says the firmware configured port 0; port 1's is
/// the next, and so on. The I210 and I211 have one port.
const CFG_DONE_PORT_0: u32 = 1 << 18;
/// SWSM's bits: software's semaphore, and the one it shares with the
/// firmware.
const SWSM_SMBI: u32 = 1 << 0;
const SWSM_SWESMBI: u32 = 1 << 1;
/// SW_FW_SYNC's bit for each port's PHY, software's; the firmware's is
/// 16 bits higher.
const PHY_SEMAPHORES: [u32; 4] = [0x02, 0x04, 0x20, 0x40];

/// SRRCTL: advanced descriptors of one buffer, of 2 KiB (in KiB), with
/// the header size Linux gives it (in 64 bytes, from bit 8), which one
/// buffer does not use.
const SRRCTL: u32 = 0x0200_0000 | (4 << 8) | 2;
/// RXDCTL's and TXDCTL's queue enable bit, and the thresholds: prefetch
/// when 8 descriptors are left, at least 8 (1 to send) at a time, and
/// write each back once it is done (WTHRESH 1), so that a polled card's
/// frames do not wait for more.
const QUEUE_ENABLE: u32 = 1 << 25;
const RXDCTL: u32 = QUEUE_ENABLE | 8 | (8 << 8) | (1 << 16);
const TXDCTL: u32 = QUEUE_ENABLE | 8 | (1 << 8) | (1 << 16);
/// At most 20,000 interrupts a second: EITR's interval, in microseconds
/// from bit 2, with its counter left alone when it is written.
const EITR: u32 = (50 << 2) | (1 << 31);
/// IVAR0: queue 0's receive and transmit causes raise the interrupt.
const IVAR0: u32 = 0x8080;

/// How long things take.
const MASTER_US: u32 = 80_000;
const AUTO_READ_US: u32 = 10_000;
const CFG_DONE_US: u32 = 100_000;
const SEMAPHORE_US: u32 = 10_000;
const QUEUE_US: u32 = 10_000;
const STEP_US: u32 = 10;

/// The descriptors in each ring.
const RECEIVE: usize = 256;
const TRANSMIT: usize = 256;

/// Which controller a card is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mac {
    /// The 82576, which QEMU emulates as its igb.
    I82576,
    /// The 82580.
    I82580,
    /// The I350.
    I350,
    /// The I210.
    I210,
    /// The I211.
    I211,
}

/// The copper cards of the family, by device ID.
pub fn identify(device: u16) -> Option<(&'static str, Mac)> {
    Some(match device {
        0x10c9 | 0x10e8 | 0x1526 => ("82576", Mac::I82576),
        0x150a => ("82576NS", Mac::I82576),
        0x150e | 0x1516 => ("82580", Mac::I82580),
        0x1521 => ("I350", Mac::I350),
        0x1533 | 0x1534 | 0x1535 | 0x157b => ("I210", Mac::I210),
        0x1539 => ("I211", Mac::I211),
        _ => return None,
    })
}

/// Resets the card `mac`, a `model`, and sets it running: see
/// [`crate::start`].
pub fn start(
    model: &'static str,
    mac: Mac,
    regs: Mmio,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
) -> Result<Card, Error> {
    if regs.len() < reg::END {
        return Err(Error::Registers);
    }
    if regs.read32(reg::STATUS) == u32::MAX {
        return Err(Error::Device("its registers read as all ones"));
    }
    reset(&regs, mac, delay);
    if regs.read32(reg::CTRL_EXT) & LINK_MODE != 0 {
        return Err(Error::Unsupported("links other than copper"));
    }
    // The card's firmware leaves it to the driver from here.
    shared::update(&regs, reg::CTRL_EXT, shared::CTRL_EXT_DRV_LOAD, 0);
    shared::set_up_link(&regs);
    let port = ((regs.read32(reg::STATUS) >> status::FUNC_SHIFT) & 3) as usize;
    let phy = PHY_SEMAPHORES[port];
    if take_semaphore(&regs, phy, delay) {
        shared::power_up_phy(&regs, delay);
        give_semaphore(&regs, phy, delay);
    }
    let address = shared::registers_mac(&regs).unwrap_or(fallback_mac);
    shared::set_address(&regs, address);
    // The queues off while their rings are set up, then on.
    regs.write32(reg::RXDCTL0, 0);
    regs.write32(reg::TXDCTL0, 0);
    let mut rings =
        Rings::new(regs.clone(), rings::EM, Format::Advanced, RECEIVE, TRANSMIT, alloc)?;
    regs.write32(reg::SRRCTL0, SRRCTL);
    regs.write32(reg::RXDCTL0, RXDCTL);
    regs.write32(reg::TXDCTL0, TXDCTL);
    let mut on = |at| wait_for(delay, QUEUE_US, STEP_US, || regs.read32(at) & QUEUE_ENABLE != 0);
    if !on(reg::RXDCTL0) || !on(reg::TXDCTL0) {
        halt(&regs);
        return Err(Error::Timeout("turning its queues on"));
    }
    rings.hand_over();
    if interrupt == Interrupt::Msi {
        regs.write32(reg::GPIE, 0);
        regs.write32(reg::IVAR0, IVAR0);
        regs.write32(reg::EITR0, EITR);
    }
    regs.write32(reg::TCTL, tctl::EN | tctl::PSP | tctl::CT | tctl::COLD | tctl::RTLC);
    regs.write32(reg::RCTL, rctl::EN | rctl::BAM | rctl::MPE | rctl::SECRC);
    let _ = regs.read32(reg::ICR);
    if interrupt == Interrupt::Msi {
        regs.write32(reg::IMS, CAUSES);
    }
    Ok(Card::new(model, address, regs, rings, interrupt, &OPS))
}

static OPS: Ops = Ops { icr: reg::ICR, link: shared::link, halt };

/// Stops the card: interrupts and queues off, the card given back to its
/// firmware, then a reset, which stops its DMA.
fn halt(regs: &Mmio) {
    regs.write32(reg::IMC, u32::MAX);
    regs.write32(reg::RCTL, 0);
    regs.write32(reg::TCTL, 0);
    regs.write32(reg::RXDCTL0, 0);
    regs.write32(reg::TXDCTL0, 0);
    shared::update(regs, reg::CTRL_EXT, 0, shared::CTRL_EXT_DRV_LOAD);
    shared::update(regs, reg::CTRL, ctrl::RST, 0);
}

/// Resets the card, its interrupts off and its DMA stopped first, and
/// waits for it to read its NVM and for its firmware to configure it.
/// None of these is waited for in vain: a card without an NVM, or whose
/// firmware does not say, goes on, as FreeBSD's and Linux's drivers do.
fn reset(regs: &Mmio, mac: Mac, delay: &mut dyn FnMut(u32)) {
    let _ = shared::stop_dma(regs, MASTER_US, delay);
    regs.write32(reg::IMC, u32::MAX);
    regs.write32(reg::RCTL, 0);
    regs.write32(reg::TCTL, tctl::PSP);
    let _ = regs.read32(reg::STATUS);
    delay(10_000);
    shared::update(regs, reg::CTRL, ctrl::RST, 0);
    // The reset takes a few microseconds; the card must not be touched
    // meanwhile.
    delay(5_000);
    let _ = wait_for(delay, AUTO_READ_US, STEP_US, || regs.read32(reg::EECD) & EECD_AUTO_RD != 0);
    let port = match mac {
        Mac::I210 | Mac::I211 => 0,
        _ => (regs.read32(reg::STATUS) >> status::FUNC_SHIFT) & 3,
    };
    let done = CFG_DONE_PORT_0 << port;
    let _ = wait_for(delay, CFG_DONE_US, 1_000, || regs.read32(reg::EEMNGCTL) & done != 0);
    regs.write32(reg::IMC, u32::MAX);
    let _ = regs.read32(reg::ICR);
}

/// Takes the hardware semaphore that guards SW_FW_SYNC: software's bit,
/// which reading sets, then the one shared with the firmware.
fn take_hardware_semaphore(regs: &Mmio, delay: &mut dyn FnMut(u32)) -> bool {
    if !wait_for(delay, SEMAPHORE_US, 50, || regs.read32(reg::SWSM) & SWSM_SMBI == 0) {
        return false;
    }
    let had = wait_for(delay, SEMAPHORE_US, 50, || {
        shared::update(regs, reg::SWSM, SWSM_SWESMBI, 0);
        regs.read32(reg::SWSM) & SWSM_SWESMBI != 0
    });
    if !had {
        give_hardware_semaphore(regs);
    }
    had
}

fn give_hardware_semaphore(regs: &Mmio) {
    shared::update(regs, reg::SWSM, 0, SWSM_SMBI | SWSM_SWESMBI);
}

/// Takes `mask`'s resource from the firmware, through SW_FW_SYNC:
/// whether it was had.
fn take_semaphore(regs: &Mmio, mask: u32, delay: &mut dyn FnMut(u32)) -> bool {
    for _ in 0..200 {
        if !take_hardware_semaphore(regs, delay) {
            return false;
        }
        let sync = regs.read32(reg::SW_FW_SYNC);
        if sync & (mask | (mask << 16)) == 0 {
            regs.write32(reg::SW_FW_SYNC, sync | mask);
            give_hardware_semaphore(regs);
            return true;
        }
        give_hardware_semaphore(regs);
        delay(5_000);
    }
    false
}

/// Gives `mask`'s resource back to the firmware.
fn give_semaphore(regs: &Mmio, mask: u32, delay: &mut dyn FnMut(u32)) {
    // Taken even if it cannot be had: the bit must go.
    let had = take_hardware_semaphore(regs, delay);
    shared::update(regs, reg::SW_FW_SYNC, 0, mask);
    if had {
        give_hardware_semaphore(regs);
    }
}
