// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The e1000 and e1000e family: Intel's 8254x PCI and PCI-X gigabit
//! controllers (the 8254x Family of Gigabit Ethernet Controllers
//! Software Developer's Manual) and the 82571, 82572, 82573, 82574 and
//! 82583 on PCI Express (the PCIe GbE Controllers Open Source Software
//! Developer's Manual and the 82574 datasheet), copper ones only.
//!
//! The driver resets the card (stopping its DMA first on PCI Express),
//! applies the errata settings the 82571 family needs, tells the card's
//! firmware a driver has it (on PCI Express), powers the PHY up and lets
//! it negotiate the link's speed, reads the card's MAC address and
//! programs it as its one unicast address, and takes every multicast
//! frame, which IPv6 and mDNS need, and broadcasts. It drives one queue
//! pair of legacy descriptors ([`rings`](crate::rings)): the receive
//! buffers are [`SLOT`](radix_nic::SLOT) bytes, the frame check sequence
//! is stripped, and short frames are padded. With an MSI the card raises
//! it for frames received and sent and for link changes, at most 20,000
//! times a second; reading the interrupt cause register, which the
//! driver does once no frame is left to take, acknowledges it.

use radix_hw::{Dma, Mmio};
use radix_nic::{Error, usable_mac, wait_for};

use crate::rings::{self, Format, Rings};
use crate::shared::{self, CAUSES, EECD_AUTO_RD, ctrl, status, tctl};
use crate::{Card, Interrupt, Ops};

/// The registers, from the start of BAR 0.
mod reg {
    pub use crate::shared::reg::*;
    pub const TIPG: usize = 0x0410;
    pub const EXTCNF_CTRL: usize = 0x0f00;
    pub const PBA_ECC: usize = 0x1100;
    pub const RDTR: usize = 0x2820;
    pub const TIDV: usize = 0x3820;
    pub const TXDCTL0: usize = 0x3828;
    pub const TXDCTL1: usize = 0x3928;
    pub const TARC0: usize = 0x3840;
    pub const TARC1: usize = 0x3940;
    pub const RFCTL: usize = 0x5008;
    pub const GCR: usize = 0x5b00;
    pub const GCR2: usize = 0x5b64;
    /// The last register the driver touches, and its size.
    pub const END: usize = GCR2 + 4;
}

/// CTRL's bit the 82573, 82574 and 82583 clear, as their errata say.
const CTRL_BIT_29: u32 = 1 << 29;
/// STATUS's bit that says the card is the second port of two.
const STATUS_FUNC_1: u32 = 1 << status::FUNC_SHIFT;

/// At most 20,000 interrupts a second: ITR counts 256 ns.
const ITR: u32 = 1_000_000_000 / (20_000 * 256);
/// The interpacket gap for copper: IPGT 8, IPGR1 8, IPGR2 6.
const TIPG: u32 = 8 | (8 << 10) | (6 << 20);

/// How long things take.
const RESET_US: u32 = 10_000;
const MASTER_US: u32 = 80_000;
const AUTO_READ_US: u32 = 10_000;
const NVM_US: u32 = 10_000;
const SEMAPHORE_US: u32 = 100_000;
const STEP_US: u32 = 10;

/// The descriptors in each ring.
const RECEIVE: usize = 256;
const TRANSMIT: usize = 256;

/// Which controller a card is: what decides its quirks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mac {
    /// The 82540EM and 82540EP, which QEMU and VirtualBox emulate.
    I82540,
    /// The 82541EI, ER and GI.
    I82541,
    /// The 82545EM and GM, which VMware emulates.
    I82545,
    /// The dual-port 82546EB and GB.
    I82546,
    /// The 82547EI and GI.
    I82547,
    /// The 82571EB and PT, on PCI Express.
    I82571,
    /// The 82572EI.
    I82572,
    /// The 82573E and L.
    I82573,
    /// The 82574L, which QEMU emulates as its e1000e.
    I82574,
    /// The 82583V.
    I82583,
}

impl Mac {
    /// Whether it is on PCI Express: the 82571 and later.
    pub fn pcie(self) -> bool {
        self >= Mac::I82571
    }

    /// Where EERD has its done bit and its address: the 82540, 82545 and
    /// 82546 at bit 4 and from bit 8; the 82541, 82547 and those on PCI
    /// Express at bit 1 and from bit 2.
    fn eerd(self) -> (u32, u32) {
        match self {
            Mac::I82540 | Mac::I82545 | Mac::I82546 => (1 << 4, 8),
            _ => (1 << 1, 2),
        }
    }

    /// Whether its MAC and PHY share MDIO through EXTCNF_CTRL's
    /// software ownership bit.
    fn mdio_semaphore(self) -> bool {
        matches!(self, Mac::I82573 | Mac::I82574 | Mac::I82583)
    }
}

/// The copper cards of the family, by device ID.
pub fn identify(device: u16) -> Option<(&'static str, Mac)> {
    Some(match device {
        0x100e | 0x1015 => ("82540EM", Mac::I82540),
        0x1016 | 0x1017 | 0x101e => ("82540EP", Mac::I82540),
        0x1013 | 0x1018 => ("82541EI", Mac::I82541),
        0x1014 | 0x1078 => ("82541ER", Mac::I82541),
        0x1076 | 0x1077 | 0x107c => ("82541GI", Mac::I82541),
        0x100f => ("82545EM", Mac::I82545),
        0x1026 => ("82545GM", Mac::I82545),
        0x1010 | 0x101d => ("82546EB", Mac::I82546),
        0x1079 | 0x1099 | 0x10b5 => ("82546GB", Mac::I82546),
        0x1019 | 0x101a => ("82547EI", Mac::I82547),
        0x1075 => ("82547GI", Mac::I82547),
        0x105e | 0x10a4 | 0x10bc => ("82571EB", Mac::I82571),
        0x10d5 => ("82571PT", Mac::I82571),
        0x107d | 0x10b9 => ("82572EI", Mac::I82572),
        0x108b | 0x108c => ("82573E", Mac::I82573),
        0x109a => ("82573L", Mac::I82573),
        0x10d3 => ("82574L", Mac::I82574),
        0x10f6 => ("82574LA", Mac::I82574),
        0x150c => ("82583V", Mac::I82583),
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
    reset(&regs, mac, delay)?;
    let ops = if mac.pcie() {
        errata(&regs, mac);
        // The card's firmware leaves it to the driver from here.
        if mac == Mac::I82573 {
            shared::update(&regs, reg::SWSM, shared::SWSM_DRV_LOAD, 0);
            &OPS_82573
        } else {
            shared::update(&regs, reg::CTRL_EXT, shared::CTRL_EXT_DRV_LOAD, 0);
            &OPS_PCIE
        }
    } else {
        &OPS
    };
    shared::set_up_link(&regs);
    power_up_phy(&regs, mac, delay);
    let address = read_mac(&regs, mac, delay).unwrap_or(fallback_mac);
    shared::set_address(&regs, address);
    let mut rings = Rings::new(regs.clone(), rings::EM, Format::Legacy, RECEIVE, TRANSMIT, alloc)?;
    rings.hand_over();
    // No delays on receive or transmit interrupts: the throttle spaces
    // them.
    regs.write32(reg::RDTR, 0);
    regs.write32(reg::TIDV, 0);
    regs.write32(reg::ITR, ITR);
    regs.write32(reg::TIPG, TIPG);
    let mut transmit = tctl::EN | tctl::PSP | tctl::CT | tctl::COLD | tctl::RTLC;
    if mac.pcie() {
        transmit |= tctl::MULR;
    }
    regs.write32(reg::TCTL, transmit);
    let receive = shared::rctl::EN | shared::rctl::BAM | shared::rctl::MPE | shared::rctl::SECRC;
    regs.write32(reg::RCTL, receive);
    let _ = regs.read32(reg::ICR);
    if interrupt == Interrupt::Msi {
        regs.write32(reg::IMS, CAUSES);
    }
    Ok(Card::new(model, address, regs, rings, interrupt, ops))
}

/// How a card of the family is stopped, by its kind: the 8254x, the
/// 82573, and the others on PCI Express.
static OPS: Ops = Ops { icr: reg::ICR, link: shared::link, halt: |regs| halt(regs, None) };
static OPS_82573: Ops = Ops {
    icr: reg::ICR,
    link: shared::link,
    halt: |regs| halt(regs, Some((reg::SWSM, shared::SWSM_DRV_LOAD))),
};
static OPS_PCIE: Ops = Ops {
    icr: reg::ICR,
    link: shared::link,
    halt: |regs| halt(regs, Some((reg::CTRL_EXT, shared::CTRL_EXT_DRV_LOAD))),
};

/// Stops the card: interrupts off, the card given back to its firmware
/// (`firmware`: the register and bit that told it a driver had it), then
/// a reset, which stops its DMA.
fn halt(regs: &Mmio, firmware: Option<(usize, u32)>) {
    regs.write32(reg::IMC, u32::MAX);
    regs.write32(reg::RCTL, 0);
    regs.write32(reg::TCTL, 0);
    if let Some((at, bit)) = firmware {
        shared::update(regs, at, 0, bit);
    }
    shared::update(regs, reg::CTRL, ctrl::RST, 0);
}

/// Resets the card, its interrupts off. On PCI Express, its DMA is
/// stopped first, and the reset is done with MDIO held, as the 82573,
/// 82574 and 82583 share it with their firmware.
fn reset(regs: &Mmio, mac: Mac, delay: &mut dyn FnMut(u32)) -> Result<(), Error> {
    regs.write32(reg::IMC, u32::MAX);
    if mac.pcie() && !shared::stop_dma(regs, MASTER_US, delay) {
        return Err(Error::Timeout("stopping its DMA before a reset"));
    }
    regs.write32(reg::RCTL, 0);
    regs.write32(reg::TCTL, tctl::PSP);
    let _ = regs.read32(reg::STATUS);
    delay(10_000);
    let held = mac.mdio_semaphore() && take_mdio(regs, delay);
    let control = regs.read32(reg::CTRL);
    regs.write32(reg::CTRL, control | ctrl::RST);
    // The reset takes a few microseconds; the card must not be touched
    // meanwhile.
    delay(20);
    let reset_done = wait_for(delay, RESET_US, STEP_US, || regs.read32(reg::CTRL) & ctrl::RST == 0);
    if held {
        give_mdio(regs);
    }
    if !reset_done {
        return Err(Error::Timeout("a reset"));
    }
    if mac.pcie() {
        // It reads its NVM again after the reset.
        if !wait_for(delay, AUTO_READ_US, STEP_US, || regs.read32(reg::EECD) & EECD_AUTO_RD != 0) {
            return Err(Error::Timeout("reading its NVM after a reset"));
        }
        match mac {
            Mac::I82571 | Mac::I82572 => {
                // Let go of the NVM, which the reset may leave requested.
                let eecd = regs.read32(reg::EECD);
                regs.write32(reg::EECD, eecd & !((1 << 6) | (1 << 7)));
            }
            _ => delay(25_000),
        }
    } else {
        delay(5_000);
    }
    regs.write32(reg::IMC, u32::MAX);
    let _ = regs.read32(reg::ICR);
    Ok(())
}

/// The settings the 82571 family's specification updates ask for, as
/// FreeBSD's e1000_initialize_hw_bits_82571 applies them.
fn errata(regs: &Mmio, mac: Mac) {
    let set = |at: usize, on: u32, off: u32| shared::update(regs, at, on, off);
    set(reg::TXDCTL0, 1 << 22, 0);
    set(reg::TXDCTL1, 1 << 22, 0);
    match mac {
        Mac::I82571 | Mac::I82572 => {
            set(reg::TARC0, (1 << 23) | (1 << 24) | (1 << 25) | (1 << 26), 0xf << 27);
            // The transmitter is set up with MULR, so bit 28 is off.
            set(reg::TARC1, (1 << 22) | (1 << 24) | (1 << 25) | (1 << 26), (1 << 28) | (3 << 29));
            // DMA dynamic clock gating off.
            set(reg::CTRL_EXT, 0, 1 << 19);
        }
        Mac::I82574 | Mac::I82583 => set(reg::TARC0, 1 << 26, 0xf << 27),
        _ => set(reg::TARC0, 0, 0xf << 27),
    }
    if mac.mdio_semaphore() {
        set(reg::CTRL, 0, CTRL_BIT_29);
        set(reg::CTRL_EXT, 1 << 22, 1 << 23);
    }
    if mac == Mac::I82571 {
        set(reg::PBA_ECC, 1, 0);
    }
    if mac <= Mac::I82573 {
        // Malformed IPv6 extension headers can hang the receiver.
        set(reg::RFCTL, (1 << 16) | (1 << 17), 0);
    }
    if matches!(mac, Mac::I82574 | Mac::I82583) {
        set(reg::GCR, 1 << 22, 0);
        set(reg::GCR2, 1, 0);
    }
}

/// Takes MDIO from the card's firmware: whether it was had.
fn take_mdio(regs: &Mmio, delay: &mut dyn FnMut(u32)) -> bool {
    const OWNED: u32 = 1 << 5;
    wait_for(delay, SEMAPHORE_US, 1_000, || {
        let value = regs.read32(reg::EXTCNF_CTRL);
        regs.write32(reg::EXTCNF_CTRL, value | OWNED);
        regs.read32(reg::EXTCNF_CTRL) & OWNED != 0
    })
}

fn give_mdio(regs: &Mmio) {
    let value = regs.read32(reg::EXTCNF_CTRL);
    regs.write32(reg::EXTCNF_CTRL, value & !(1 << 5));
}

/// Powers the PHY up and has it negotiate the link, with MDIO held from
/// the firmware where the card shares it.
fn power_up_phy(regs: &Mmio, mac: Mac, delay: &mut dyn FnMut(u32)) {
    if !mac.mdio_semaphore() {
        shared::power_up_phy(regs, shared::INTERNAL_PHY, delay);
    } else if take_mdio(regs, delay) {
        shared::power_up_phy(regs, shared::INTERNAL_PHY, delay);
        give_mdio(regs);
    }
}

/// Reads word `word` of the card's NVM through EERD.
fn nvm_word(regs: &Mmio, mac: Mac, word: u32, delay: &mut dyn FnMut(u32)) -> Option<u16> {
    let (done, shift) = mac.eerd();
    regs.write32(reg::EERD, (word << shift) | 1);
    let mut value = 0;
    wait_for(delay, NVM_US, STEP_US, || {
        value = regs.read32(reg::EERD);
        value & done != 0
    })
    .then_some((value >> 16) as u16)
}

/// The card's MAC address: on the 8254x from its NVM's first three words
/// (emulated 8254x cards do not always load it into RAL0 and RAH0), with
/// the second port of a dual-port card one more; on PCI Express from
/// RAL0 and RAH0, which the card loaded from its NVM. Each falls back on
/// the other; `None` when neither has an address.
fn read_mac(regs: &Mmio, mac: Mac, delay: &mut dyn FnMut(u32)) -> Option<[u8; 6]> {
    let from_registers = || shared::registers_mac(regs);
    let mut from_nvm = || {
        let mut address = [0u8; 6];
        for word in 0..3 {
            let value = nvm_word(regs, mac, word, delay)?.to_le_bytes();
            address[2 * word as usize] = value[0];
            address[2 * word as usize + 1] = value[1];
        }
        if mac == Mac::I82546 && regs.read32(reg::STATUS) & STATUS_FUNC_1 != 0 {
            address[5] ^= 1;
        }
        usable_mac(address).then_some(address)
    };
    if mac.pcie() { from_registers().or_else(from_nvm) } else { from_nvm().or_else(from_registers) }
}
