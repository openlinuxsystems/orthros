// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the e1000 and igb families have in common: the registers both
//! keep where the 8254x had them, the PHY behind MDIC, the MAC address
//! in the first receive address register, and the link in STATUS.

use radix_hw::Mmio;
use radix_nic::{Link, usable_mac, wait_for};

/// The registers both families have, from the start of BAR 0.
pub mod reg {
    pub const CTRL: usize = 0x0000;
    pub const STATUS: usize = 0x0008;
    pub const EECD: usize = 0x0010;
    pub const EERD: usize = 0x0014;
    pub const CTRL_EXT: usize = 0x0018;
    pub const MDIC: usize = 0x0020;
    pub const ICR: usize = 0x00c0;
    pub const ITR: usize = 0x00c4;
    pub const IMS: usize = 0x00d0;
    pub const IMC: usize = 0x00d8;
    pub const RCTL: usize = 0x0100;
    pub const TCTL: usize = 0x0400;
    pub const MTA: usize = 0x5200;
    pub const RAL0: usize = 0x5400;
    pub const RAH0: usize = 0x5404;
    pub const SWSM: usize = 0x5b50;
}

/// CTRL's bits.
pub mod ctrl {
    pub const GIO_MASTER_DISABLE: u32 = 1 << 2;
    pub const LRST: u32 = 1 << 3;
    pub const ASDE: u32 = 1 << 5;
    pub const SLU: u32 = 1 << 6;
    pub const ILOS: u32 = 1 << 7;
    pub const FRCSPD: u32 = 1 << 11;
    pub const FRCDPX: u32 = 1 << 12;
    pub const RST: u32 = 1 << 26;
    pub const RFCE: u32 = 1 << 27;
    pub const TFCE: u32 = 1 << 28;
    pub const VME: u32 = 1 << 30;
    pub const PHY_RST: u32 = 1 << 31;
}

/// STATUS's bits.
pub mod status {
    pub const FD: u32 = 1 << 0;
    pub const LU: u32 = 1 << 1;
    pub const FUNC_SHIFT: u32 = 2;
    pub const SPEED_SHIFT: u32 = 6;
    pub const GIO_MASTER_ENABLE: u32 = 1 << 19;
}

/// RCTL's bits.
pub mod rctl {
    pub const EN: u32 = 1 << 1;
    pub const MPE: u32 = 1 << 4;
    pub const BAM: u32 = 1 << 15;
    /// Strips the frame check sequence. Buffers of 2048 bytes are BSIZE 0.
    pub const SECRC: u32 = 1 << 26;
}

/// TCTL's bits.
pub mod tctl {
    pub const EN: u32 = 1 << 1;
    pub const PSP: u32 = 1 << 3;
    pub const CT: u32 = 0x0f << 4;
    pub const COLD: u32 = 63 << 12;
    pub const RTLC: u32 = 1 << 24;
    pub const MULR: u32 = 1 << 28;
}

/// CTRL_EXT's bit that tells the card's firmware a driver has it, and
/// SWSM's on the 82573.
pub const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;
pub const SWSM_DRV_LOAD: u32 = 1 << 3;

/// EECD's bit that says the card has read its NVM.
pub const EECD_AUTO_RD: u32 = 1 << 9;

/// The interrupt causes the drivers have a card raise: transmit
/// descriptor written back, link status change, receive sequence error,
/// receive descriptor minimum threshold, receiver overrun and receive
/// timer.
pub const CAUSES: u32 = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7);

/// MDIC's bits, for the PHY's registers.
mod mdic {
    pub const PHY_SHIFT: u32 = 21;
    pub const REG_SHIFT: u32 = 16;
    pub const OP_WRITE: u32 = 1 << 26;
    pub const OP_READ: u32 = 2 << 26;
    pub const READY: u32 = 1 << 28;
    pub const ERROR: u32 = 1 << 30;
}

/// The PHY's control register (IEEE 802.3 clause 22) and its bits.
const PHY_CONTROL: u32 = 0;
const PHY_RESTART_AUTONEG: u16 = 1 << 9;
const PHY_POWER_DOWN: u16 = 1 << 11;
const PHY_AUTONEG: u16 = 1 << 12;
/// The internal PHY's address on MDIO, on the e1000 and igb families;
/// igc's is 0.
pub const INTERNAL_PHY: u32 = 1;
/// How long an MDIO access may take.
const MDIC_US: u32 = 10_000;

/// Runs one MDIO access, `command`, to the PHY at `phy`, and returns MDIC
/// as the card left it, if it finished without an error.
fn mdio(regs: &Mmio, phy: u32, command: u32, delay: &mut dyn FnMut(u32)) -> Option<u32> {
    regs.write32(reg::MDIC, command | (phy << mdic::PHY_SHIFT));
    let mut value = 0;
    let done = wait_for(delay, MDIC_US, 50, || {
        value = regs.read32(reg::MDIC);
        value & mdic::READY != 0
    });
    (done && value & mdic::ERROR == 0).then_some(value)
}

/// Reads register `register` of the PHY at `phy`.
fn phy_read(regs: &Mmio, phy: u32, register: u32, delay: &mut dyn FnMut(u32)) -> Option<u16> {
    let command = (register << mdic::REG_SHIFT) | mdic::OP_READ;
    mdio(regs, phy, command, delay).map(|value| value as u16)
}

/// Writes register `register` of the PHY at `phy`: whether the card took
/// it.
fn phy_write(regs: &Mmio, phy: u32, register: u32, data: u16, delay: &mut dyn FnMut(u32)) -> bool {
    let command = u32::from(data) | (register << mdic::REG_SHIFT) | mdic::OP_WRITE;
    mdio(regs, phy, command, delay).is_some()
}

/// Powers the PHY up, if a reset, the firmware or another system left it
/// down, and has it negotiate the link again unless the link is up; the
/// caller holds whatever the card shares MDIO with its firmware through.
/// (QEMU's igb, like a PHY reset, leaves the link down until it is
/// negotiated again, as Linux's and FreeBSD's drivers always have it be.)
/// A PHY that does not answer is left as it is: the link may still come
/// up. `phy` is its address on MDIO.
pub fn power_up_phy(regs: &Mmio, phy: u32, delay: &mut dyn FnMut(u32)) {
    let down = regs.read32(reg::STATUS) & status::LU == 0;
    if let Some(control) = phy_read(regs, phy, PHY_CONTROL, delay)
        && (down || control & PHY_POWER_DOWN != 0 || control & PHY_AUTONEG == 0)
    {
        let control = (control & !PHY_POWER_DOWN) | PHY_AUTONEG | PHY_RESTART_AUTONEG;
        let _ = phy_write(regs, phy, PHY_CONTROL, control, delay);
    }
}

/// Lets the PHY negotiate the link, and the MAC take its speed and
/// duplex from it, with flow control and VLAN tags off.
pub fn set_up_link(regs: &Mmio) {
    let mut control = regs.read32(reg::CTRL);
    control |= ctrl::SLU | ctrl::ASDE;
    control &= !(ctrl::LRST
        | ctrl::PHY_RST
        | ctrl::ILOS
        | ctrl::FRCSPD
        | ctrl::FRCDPX
        | ctrl::VME
        | ctrl::RFCE
        | ctrl::TFCE);
    regs.write32(reg::CTRL, control);
}

/// Stops the card's DMA before a reset, as PCI Express cards need: the
/// bus can hang on a request the reset cuts short.
pub fn stop_dma(regs: &Mmio, total_us: u32, delay: &mut dyn FnMut(u32)) -> bool {
    let control = regs.read32(reg::CTRL);
    regs.write32(reg::CTRL, control | ctrl::GIO_MASTER_DISABLE);
    wait_for(delay, total_us, 100, || regs.read32(reg::STATUS) & status::GIO_MASTER_ENABLE == 0)
}

/// The MAC address in the first receive address register, which the
/// card loads from its NVM at a reset, if it is one a card can have.
pub fn registers_mac(regs: &Mmio) -> Option<[u8; 6]> {
    let low = regs.read32(reg::RAL0).to_le_bytes();
    let high = regs.read32(reg::RAH0).to_le_bytes();
    let address = [low[0], low[1], low[2], low[3], high[0], high[1]];
    usable_mac(address).then_some(address)
}

/// Makes `address` the card's one unicast address, with the others off,
/// and no multicast filter: it takes every multicast frame.
pub fn set_address(regs: &Mmio, address: [u8; 6]) {
    let low = u32::from_le_bytes([address[0], address[1], address[2], address[3]]);
    let high = u32::from(u16::from_le_bytes([address[4], address[5]])) | (1 << 31);
    regs.write32(reg::RAL0, low);
    regs.write32(reg::RAH0, high);
    for entry in 1..16 {
        regs.write32(reg::RAL0 + 8 * entry, 0);
        regs.write32(reg::RAH0 + 8 * entry, 0);
    }
    for word in 0..128 {
        regs.write32(reg::MTA + 4 * word, 0);
    }
}

/// The link, as STATUS says: up, its speed and duplex.
pub fn link(regs: &Mmio) -> Link {
    let status = regs.read32(reg::STATUS);
    let up = status & status::LU != 0 && status != u32::MAX;
    let mbps = match (status >> status::SPEED_SHIFT) & 3 {
        0 => 10,
        1 => 100,
        _ => 1000,
    };
    Link { up, mbps: if up { mbps } else { 0 }, full_duplex: up && status & status::FD != 0 }
}

/// Sets `bits` in the register at `at`, and clears `clear`.
pub fn update(regs: &Mmio, at: usize, bits: u32, clear: u32) {
    let value = regs.read32(at);
    regs.write32(at, (value & !clear) | bits);
}
