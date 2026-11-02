// SPDX-FileCopyrightText: 2019, 2020, 2023-2026 Kevin Lo <kevlo@openbsd.org>
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: ISC
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! How the chips are set up, translated from OpenBSD's `rge(4)`
//! (`sys/dev/pci/if_rge.c` revision 1.48 and `if_rgereg.h`, by Kevin Lo,
//! with the hardware programming Realtek gave), function by function in
//! its order, with its names in the comments: the chip taken out of its
//! out-of-band mode and reset, its PHY powered up, configured as each
//! chip needs and set negotiating, and the MAC set up for one queue each
//! way.
//!
//! What this leaves out of OpenBSD's: Realtek's microcode for the MAC's
//! and the PHY's processors, which OpenBSD loads from the firmware files
//! `rge-rtl8125`... under Realtek's own licence, and Dukovo does not ship
//! (the chips run on the code in their ROMs, as Linux runs them without
//! its `rtl_nic` files); the RTL8127; receive side scaling, VLAN tag
//! stripping, checksum and segmentation offloads; the simulated
//! interrupt moderation; and Wake on LAN. Frames are no longer than a
//! slot, where OpenBSD takes jumbo frames, and every multicast frame is
//! taken.

use radix_nic::{Error, SLOT};

use crate::Kind;
use crate::ocp::Ocp;
use crate::reg::{self, cmd, int_cfg0, mcucmd, mii, rxcfg};

/// OpenBSD's `RGE_TIMEOUT`: how many looks most waits take.
const TIMEOUT: u32 = 100;

/// What the chip's EEE transmit idle timer is set to: OpenBSD's
/// `RGE_JUMBO_MTU + ETHER_HDR_LEN + 32`, its longest frame's payload
/// (9216 bytes less the header, the frame check sequence and a VLAN tag)
/// with the header and 32 bytes.
const EEE_TXIDLE: u16 = 9216 - 14 - 4 - 4 + 14 + 32;

/// The receive configuration register's bit OpenBSD sets in the longest
/// frame the RTL8125D and RTL8126 take: `RGE_RXCFG_VLANPHYS`.
const RXMAXSIZE_VLANPHYS: u16 = 0x8000;

/// The PCI Express link control register, from the start of the
/// capability, and its bits: ASPM's L0s and L1, and clock power
/// management (ECPM).
const LINK_CONTROL: u32 = 0x10;
const ASPM_L0S: u32 = 0x0001;
const ASPM_L1: u32 = 0x0002;
const ECPM: u32 = 0x0100;

/// The PCI Express PHY's settings, by register, for the RTL8125A
/// (`mac_r25_ephy`) and the RTL8125B (`mac_r25b_ephy`).
const MAC_R25_EPHY: [(u16, u16); 12] = [
    (0x0004, 0xd000),
    (0x000a, 0x8653),
    (0x0023, 0xab66),
    (0x0020, 0x9455),
    (0x0021, 0x99ff),
    (0x0029, 0xfe04),
    (0x0044, 0xd000),
    (0x004a, 0x8653),
    (0x0063, 0xab66),
    (0x0060, 0x9455),
    (0x0061, 0x99ff),
    (0x0069, 0xfe04),
];
const MAC_R25B_EPHY: [(u16, u16); 10] = [
    (0x000b, 0xa908),
    (0x001e, 0x20eb),
    (0x0022, 0x0023),
    (0x0002, 0x60c2),
    (0x0029, 0xff00),
    (0x004b, 0xa908),
    (0x005e, 0x28eb),
    (0x0062, 0x0023),
    (0x0042, 0x60c2),
    (0x0069, 0xff00),
];

impl Ocp<'_> {
    /// Has the PHY's processor stop for its patches to be written
    /// (`rge_patch_phy_mcu`), or go on, and waits up to 100 ms for it to
    /// say it did: whether it did.
    pub fn patch_phy_mcu(&mut self, set: bool) -> bool {
        if set {
            self.phy_set(0xb820, 0x0010);
        } else {
            self.phy_clear(0xb820, 0x0010);
        }
        for _ in 0..1000 {
            if (self.phy_read(0xb800) & 0x0040 != 0) == set {
                return true;
            }
            self.delay(100);
        }
        false
    }
}

/// Stops the chip taking frames and resets it (`rge_reset`): whether the
/// reset finished.
pub fn reset(c: &mut Ocp<'_>) -> Result<(), Error> {
    c.clear32(reg::RXCFG, rxcfg::FILTERS);
    // Enable RXDV gate.
    c.set8(reg::PPSW, 0x08);
    c.set8(reg::CMD, cmd::STOPREQ);
    c.delay(200);
    let empty = mcucmd::RXFIFO_EMPTY | mcucmd::TXFIFO_EMPTY;
    for _ in 0..3000 {
        c.delay(50);
        if c.regs.read8(reg::MCUCMD) & empty == empty {
            break;
        }
    }
    if c.kind != Kind::R25 {
        for _ in 0..3000 {
            c.delay(50);
            if c.regs.read16(reg::IM) & 0x0103 == 0x0103 {
                break;
            }
        }
    }
    let on = c.regs.read8(reg::CMD) & (cmd::TXENB | cmd::RXENB);
    c.regs.write8(reg::CMD, on);
    // Soft reset.
    c.regs.write8(reg::CMD, cmd::RESET);
    for _ in 0..TIMEOUT {
        c.delay(100);
        if c.regs.read8(reg::CMD) & cmd::RESET == 0 {
            return Ok(());
        }
    }
    Err(Error::Timeout("the chip's reset"))
}

/// Powers the chip and its PHY up and resets it (`rge_chipinit`).
pub fn chip_init(c: &mut Ocp<'_>) -> Result<(), Error> {
    exit_oob(c)?;
    power_up_phy(c);
    hw_init(c);
    hw_reset(c);
    Ok(())
}

/// Powers the PHY up and waits for it to say it is (`rge_set_phy_power`
/// on).
fn power_up_phy(c: &mut Ocp<'_>) {
    c.set8(reg::PMCH, 0xc0);
    c.mii_write(mii::BMCR, mii::BMCR_AUTOEN);
    for _ in 0..TIMEOUT {
        if c.phy_read(0xa420) & 0x0007 == 3 {
            break;
        }
        c.delay(1000);
    }
}

/// Sets the MAC up and turns off what OpenBSD's driver does not use
/// (`rge_hw_init`).
fn hw_init(c: &mut Ocp<'_>) {
    disable_aspm_clkreq(c);
    c.clear8(0xf1, 0x80);
    // Disable UPS.
    c.mac_clear(0xd40a, 0x0010);
    // Disable MAC MCU.
    disable_aspm_clkreq(c);
    c.mac_write(0xfc48, 0);
    for reg in (0xfc28..0xfc48).step_by(2) {
        c.mac_write(reg, 0);
    }
    c.delay(3000);
    c.mac_write(0xfc26, 0);
    // OpenBSD loads Realtek's MAC MCU patches here
    // (rge_mac_config_mcu, rge_mac_config_ext_mcu): firmware this driver
    // leaves out, which leaves the MCU on its ROM's code.
    // Disable PHY power saving.
    if c.kind == Kind::R25 {
        disable_phy_ocp_pwrsave(c);
    }
    // Set PCIe uncorrectable error status.
    let status = c.csi_read(0x108);
    c.csi_write(0x108, status | 0x0010_0000);
}

/// Masks and clears the chip's interrupts and resets it
/// (`rge_hw_reset`), whose failure OpenBSD leaves unchecked here.
pub fn hw_reset(c: &mut Ocp<'_>) {
    // Disable interrupts.
    c.regs.write32(reg::IMR, 0);
    c.regs.write32(reg::ISR, c.regs.read32(reg::ISR));
    // Clear timer interrupts.
    c.regs.write32(reg::TIMERINT0, 0);
    c.regs.write32(reg::TIMERINT1, 0);
    c.regs.write32(reg::TIMERINT2, 0);
    c.regs.write32(reg::TIMERINT3, 0);
    let _ = reset(c);
}

/// `rge_disable_phy_ocp_pwrsave`.
fn disable_phy_ocp_pwrsave(c: &mut Ocp<'_>) {
    if c.phy_read(0xc416) != 0x0500 {
        c.patch_phy_mcu(true);
        c.phy_write(0xc416, 0);
        c.phy_write(0xc416, 0x0500);
        c.patch_phy_mcu(false);
    }
}

/// Turns ASPM's clock requests off (`rge_disable_aspm_clkreq`), with
/// configuration writes allowed for it if they were not.
fn disable_aspm_clkreq(c: &mut Ocp<'_>) {
    let unlock = c.regs.read8(reg::EECMD) & reg::EECMD_WRITECFG != reg::EECMD_WRITECFG;
    if unlock {
        c.set8(reg::EECMD, reg::EECMD_WRITECFG);
    }
    if c.kind.r26() {
        c.clear8(reg::INT_CFG0, int_cfg0::CLKREQ_8126);
    } else {
        c.clear8(reg::CFG2, reg::CFG2_CLKREQ_EN);
    }
    c.clear8(reg::CFG5, reg::CFG5_PME_STS);
    if unlock {
        c.clear8(reg::EECMD, reg::EECMD_WRITECFG);
    }
}

/// Turns the PCI Express link's power states and clock power management
/// off, in the capability at `express` of the configuration space, as
/// OpenBSD's `rge_attach()` does: through the chip's CSI registers here.
pub fn disable_aspm(c: &mut Ocp<'_>, express: usize) {
    let at = express as u32 + LINK_CONTROL;
    let control = c.csi_read(at);
    c.csi_write(at, control & !(ASPM_L0S | ASPM_L1 | ECPM));
}

/// Takes the chip out of its out-of-band mode, which a management
/// controller or Wake on LAN may have left it in (`rge_exit_oob`).
fn exit_oob(c: &mut Ocp<'_>) -> Result<(), Error> {
    // Disable RealWoW.
    c.mac_write(0xc0bc, 0x00ff);
    reset(c)?;
    // Disable OOB.
    c.clear8(reg::MCUCMD, mcucmd::IS_OOB);
    c.mac_clear(0xe8de, 0x4000);
    wait_twicmd(c);
    c.mac_write(0xc0aa, 0x07d0);
    c.mac_write(0xc0a6, 0x01b5);
    c.mac_write(0xc01e, 0x5555);
    wait_twicmd(c);
    if c.mac_read(0xd42c) & 0x0100 != 0 {
        for _ in 0..TIMEOUT {
            if c.phy_read(0xa420) & 0x0007 == 2 {
                break;
            }
            c.delay(1000);
        }
        c.mac_clear(0xd42c, 0x0100);
        if c.kind != Kind::R25 {
            c.phy_clear(0xa466, 0x0001);
        }
        c.phy_clear(0xa468, 0x000a);
    }
    Ok(())
}

/// Waits up to a millisecond for the chip to say its FIFOs are shared
/// out.
fn wait_twicmd(c: &mut Ocp<'_>) {
    for _ in 0..10 {
        c.delay(100);
        if c.regs.read16(reg::TWICMD) & 0x0200 != 0 {
            break;
        }
    }
}

/// The MAC address the chip's bootcode left for it (`rge_get_macaddr`).
pub fn mac_address(c: &Ocp<'_>) -> [u8; 6] {
    let [a, b, cc, d] = c.regs.read32(reg::ADDR0).to_le_bytes();
    let [e, f] = c.regs.read16(reg::ADDR1).to_le_bytes();
    [a, b, cc, d, e, f]
}

/// Programs `mac` as the chip's address (`rge_set_macaddr`).
pub fn set_mac_address(c: &mut Ocp<'_>, mac: [u8; 6]) {
    c.set8(reg::EECMD, reg::EECMD_WRITECFG);
    c.regs.write32(reg::MAC0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
    c.regs.write32(reg::MAC4, u32::from(u16::from_le_bytes([mac[4], mac[5]])));
    c.clear8(reg::EECMD, reg::EECMD_WRITECFG);
}

/// Sets the PCI Express PHY up (`rge_ephy_config`): the RTL8125A's and
/// RTL8125B's; the others need nothing.
fn ephy_config(c: &mut Ocp<'_>) {
    match c.kind {
        Kind::R25 => {
            for (reg, value) in MAC_R25_EPHY {
                c.ephy_write(reg, value);
            }
            c.ephy_modify(0x002a, 0x7000, 0x3000);
            c.ephy_clear(0x0019, 0x0040);
            c.ephy_set(0x001b, 0x0e00);
            c.ephy_clear(0x001b, 0x7000);
            c.ephy_write(0x0002, 0x6042);
            c.ephy_write(0x0006, 0x0014);
            c.ephy_modify(0x006a, 0x7000, 0x3000);
            c.ephy_clear(0x0059, 0x0040);
            c.ephy_set(0x005b, 0x0e00);
            c.ephy_clear(0x005b, 0x7000);
            c.ephy_write(0x0042, 0x6042);
            c.ephy_write(0x0046, 0x0014);
        }
        Kind::R25B => {
            for (reg, value) in MAC_R25B_EPHY {
                c.ephy_write(reg, value);
            }
        }
        _ => {}
    }
}

/// The PHY's advertisements of the speeds above a gigabit the chip has.
fn fast_speeds(kind: Kind) -> u16 {
    if kind.r26() { reg::ADV_5000TFDX | reg::ADV_2500TFDX } else { reg::ADV_2500TFDX }
}

/// Resets the PHY and configures it as the chip needs, with its energy
/// efficient Ethernet off (`rge_phy_config`): whether the PHY's reset
/// finished.
pub fn phy_config(c: &mut Ocp<'_>) -> Result<(), Error> {
    ephy_config(c);
    // PHY reset.
    let anar =
        c.mii_read(mii::ANAR) & !(mii::ANAR_TX_FD | mii::ANAR_TX | mii::ANAR_10_FD | mii::ANAR_10);
    c.mii_write(mii::ANAR, anar);
    let gtcr = c.mii_read(mii::GTCR) & !(mii::GTCR_ADV_1000TFDX | mii::GTCR_ADV_1000THDX);
    c.mii_write(mii::GTCR, gtcr);
    c.phy_clear(0xa5d4, fast_speeds(c.kind));
    c.mii_write(mii::BMCR, mii::BMCR_RESET | mii::BMCR_AUTOEN | mii::BMCR_STARTNEG);
    let mut reset = false;
    for _ in 0..2500 {
        if c.mii_read(mii::BMCR) & mii::BMCR_RESET == 0 {
            reset = true;
            break;
        }
        c.delay(1000);
    }
    if !reset {
        return Err(Error::Timeout("the PHY's reset"));
    }
    // OpenBSD reads the PHY's RAM code version here, to load Realtek's
    // PHY MCU patches (rge_phy_config_mcu) when it is not theirs:
    // firmware this driver leaves out.
    match c.kind {
        Kind::R25 => r25(c),
        Kind::R25B => r25b(c),
        Kind::R25D1 => r25d_1(c),
        Kind::R25D2 => r25d_2(c),
        Kind::R26_1 => r26_1(c),
        Kind::R26_2 => r26_2(c),
    }
    c.phy_clear(0xa5b4, 0x8000);
    // Disable EEE.
    c.mac_clear(0xe040, 0x0003);
    if c.kind == Kind::R25 {
        c.mac_clear(0xeb62, 0x0006);
        c.phy_clear(0xa432, 0x0010);
    } else if c.kind == Kind::R25B || c.kind.r25d() {
        c.phy_set(0xa432, 0x0010);
    }
    c.phy_clear(0xa5d0, 0x0006);
    if c.kind.r26() {
        c.phy_clear(0xa6d4, 0x0003);
    } else {
        c.phy_clear(0xa6d4, 0x0001);
    }
    c.phy_clear(0xa6d8, 0x0010);
    c.phy_clear(0xa428, 0x0080);
    c.phy_clear(0xa4a2, 0x0200);
    // Disable advanced EEE.
    c.mac_clear(0xe052, 0x0001);
    c.phy_clear(0xa442, 0x3000);
    c.phy_clear(0xa430, 0x8000);
    Ok(())
}

/// Sets the MAC up and starts it, as `rge_init()` does after the PHY's
/// configuration, with the receive ring at `receive` and the transmit
/// ring at `transmit`, and the PHY set negotiating every speed it has.
pub fn init(c: &mut Ocp<'_>, receive: u64, transmit: u64) {
    let kind = c.kind;
    c.set8(reg::EECMD, reg::EECMD_WRITECFG);
    c.clear8(0xf1, 0x80);
    disable_aspm_clkreq(c);
    c.regs.write16(reg::EEE_TXIDLE_TIMER, EEE_TXIDLE);
    // Load the addresses of the RX and TX lists into the chip.
    c.regs.write32(reg::RXDESC_ADDR_LO, receive as u32);
    c.regs.write32(reg::RXDESC_ADDR_HI, (receive >> 32) as u32);
    c.regs.write32(reg::TXDESC_ADDR_LO, transmit as u32);
    c.regs.write32(reg::TXDESC_ADDR_HI, (transmit >> 32) as u32);
    // Set the initial RX and TX configurations.
    let rxconf = match kind {
        Kind::R25 => rxcfg::CONFIG,
        Kind::R25B => rxcfg::CONFIG_8125B,
        Kind::R25D1 | Kind::R25D2 => rxcfg::CONFIG_8125D,
        Kind::R26_1 | Kind::R26_2 => rxcfg::CONFIG_8126,
    };
    c.regs.write32(reg::RXCFG, rxconf);
    c.regs.write32(reg::TXCFG, reg::TXCFG_CONFIG);
    let value = c.csi_read(0x70c) & !0x3f00_0000;
    c.csi_write(0x70c, value | 0x2700_0000);
    if kind.r26() {
        // Disable L1 timeout.
        let value = c.csi_read(0x890) & !0x0000_0001;
        c.csi_write(0x890, value);
    } else if !kind.r25d() {
        c.regs.write16(0x0382, 0x221b);
    }
    c.regs.write32(reg::RSS_CTRL, 0);
    // One receive queue.
    let value = c.regs.read16(reg::RXQUEUE_CTRL) & !0x001c;
    c.regs.write16(reg::RXQUEUE_CTRL, value);
    c.clear8(reg::CFG1, reg::CFG1_SPEED_DOWN);
    c.mac_write(0xc140, 0xffff);
    c.mac_write(0xc142, 0xffff);
    c.mac_set(0xeb58, 0x0001);
    if kind.r26() {
        c.clear8(0xd8, 0x02);
    }
    let value = c.mac_read(0xe614) & !0x0700;
    let value = match kind {
        Kind::R25B => value | 0x0200,
        _ => value | 0x0300,
    };
    c.mac_write(0xe614, value);
    // One transmit queue.
    c.mac_modify(0xe63e, 0x0c00, 0);
    c.mac_modify(0xe63e, 0x0030, 0x0020);
    c.mac_clear(0xc0b4, 0x0001);
    c.mac_set(0xc0b4, 0x0001);
    c.mac_set(0xc0b4, 0x000c);
    c.mac_modify(0xeb6a, 0x00ff, 0x0033);
    c.mac_modify(0xeb50, 0x03e0, 0x0040);
    c.mac_clear(0xe056, 0x00f0);
    c.regs.write8(reg::TDFNR, 0x10);
    c.mac_clear(0xe040, 0x1000);
    c.mac_modify(0xea1c, 0x0003, 0x0001);
    if kind.r25d() {
        c.mac_write(0xe0c0, 0x4403);
    } else {
        c.mac_write(0xe0c0, 0x4000);
    }
    c.mac_set(0xe052, 0x0060);
    c.mac_clear(0xe052, 0x0088);
    c.mac_modify(0xd430, 0x0fff, 0x045f);
    c.set8(reg::DLLPR, reg::DLLPR_PFM_EN | reg::DLLPR_TX_10M_PS_EN);
    c.set8(reg::PPSW, 0x40);
    if kind == Kind::R25 {
        c.set8(reg::MCUCMD, 0x01);
    }
    if !kind.r25d() {
        // Disable EEE plus.
        c.mac_clear(0xe080, 0x0002);
    }
    if kind.r26() {
        c.mac_clear(0xea1c, 0x0304);
    } else {
        c.mac_clear(0xea1c, 0x0004);
    }
    // Clear tcam entries.
    c.mac_set(0xeb54, 0x0001);
    c.delay(1);
    c.mac_clear(0xeb54, 0x0001);
    c.clear16(0x1880, 0x0030);
    // Config interrupt type.
    if kind != Kind::R25 {
        c.clear8(reg::INT_CFG0, int_cfg0::EN);
    }
    // Clear timer interrupts.
    c.regs.write32(reg::TIMERINT0, 0);
    c.regs.write32(reg::TIMERINT1, 0);
    c.regs.write32(reg::TIMERINT2, 0);
    c.regs.write32(reg::TIMERINT3, 0);
    // Clear interrupt moderation timer.
    let timers = if kind == Kind::R25B || kind.r26() { 32 } else { 64 };
    for timer in 0..timers {
        c.regs.write32(reg::INTMITI + timer * 4, 0);
    }
    if kind.r26() {
        c.clear8(
            reg::INT_CFG0,
            int_cfg0::TIMEOUT_BYPASS | int_cfg0::RDU_BYPASS_8126 | int_cfg0::MITIGATION_BYPASS,
        );
        c.regs.write16(reg::INT_CFG1, 0);
    }
    c.mac_set(0xc0ac, 0x1f80);
    c.mac_write(0xe098, 0xc302);
    c.mac_clear(0xe032, 0x0003);
    let value = c.csi_read(0x98) & !0x0000_ff00;
    c.csi_write(0x98, value);
    c.delay(20);
    if kind.r25d() {
        c.mac_modify(0xe092, 0x00ff, 0x0008);
    } else {
        c.mac_clear(0xe092, 0x00ff);
    }
    c.set16(reg::CPLUSCMD, reg::CPLUSCMD_RXCSUM);
    let _ = c.regs.read16(reg::CPLUSCMD);
    // Set Maximum frame size: a slot, where OpenBSD's is 9216 bytes.
    if matches!(kind, Kind::R25 | Kind::R25B) {
        c.regs.write16(reg::RXMAXSIZE, SLOT as u16);
    } else {
        c.regs.write16(reg::RXMAXSIZE, SLOT as u16 | RXMAXSIZE_VLANPHYS);
    }
    // Disable RXDV gate.
    c.clear8(reg::PPSW, 0x08);
    // Program the filters (rge_iff): frames to the chip's address, every
    // broadcast and every multicast frame.
    let filters = c.regs.read32(reg::RXCFG) & !(rxcfg::ALLPHYS | rxcfg::MULTI);
    c.regs.write32(reg::RXCFG, filters | rxcfg::INDIV | rxcfg::BROAD | rxcfg::MULTI);
    c.regs.write32(reg::MAR0, u32::MAX);
    c.regs.write32(reg::MAR4, u32::MAX);
    disable_aspm_clkreq(c);
    c.clear8(reg::EECMD, reg::EECMD_WRITECFG);
    c.delay(10);
    negotiate(c);
    // Enable transmit and receive.
    c.regs.write8(reg::CMD, cmd::TXENB | cmd::RXENB);
}

/// Has the PHY negotiate every speed the chip has, with pause frames
/// (`rge_ifmedia_upd` for `IFM_AUTO`).
fn negotiate(c: &mut Ocp<'_>) {
    // Disable Gigabit Lite.
    c.phy_clear(0xa428, 0x0200);
    if c.kind.r26() {
        c.phy_clear(0xa5ea, 0x0007);
    } else {
        c.phy_clear(0xa5ea, 0x0001);
    }
    let fast = fast_speeds(c.kind);
    let advertised = c.phy_read(0xa5d4) & !fast;
    let anar = mii::ANAR_TX_FD | mii::ANAR_TX | mii::ANAR_10_FD | mii::ANAR_10;
    let gig = mii::GTCR_ADV_1000TFDX | mii::GTCR_ADV_1000THDX;
    c.mii_write(mii::ANAR, anar | mii::ANAR_PAUSE_ASYM | mii::ANAR_FC);
    c.mii_write(mii::GTCR, gig);
    c.phy_write(0xa5d4, advertised | fast);
    c.mii_write(mii::BMCR, mii::BMCR_RESET | mii::BMCR_AUTOEN | mii::BMCR_STARTNEG);
}

/// Stops the chip (`rge_stop`): it takes no more frames, its interrupts
/// go off, and it is reset, which leaves it reaching no memory.
pub fn stop(c: &mut Ocp<'_>) {
    c.clear32(reg::RXCFG, rxcfg::FILTERS);
    hw_reset(c);
    c.mac_clear(0xc0ac, 0x1f80);
}

// The PHY's configuration for each chip, after its reset
// (rge_phy_config_mac_r25 and the others): what Realtek's drivers write,
// with the PHY MCU patches OpenBSD loads first left out.

fn r25(c: &mut Ocp<'_>) {
    const MAC_CFG3_A438_VALUE: [u16; 15] = [
        0x0043, 0x00a7, 0x00d6, 0x00ec, 0x00f6, 0x00fb, 0x00fd, 0x00ff, 0x00bb, 0x0058, 0x0029,
        0x0013, 0x0009, 0x0004, 0x0002,
    ];
    const MAC_CFG3_B88E_VALUE: [u16; 48] = [
        0xc091, 0x6e12, 0xc092, 0x1214, 0xc094, 0x1516, 0xc096, 0x171b, 0xc098, 0x1b1c, 0xc09a,
        0x1f1f, 0xc09c, 0x2021, 0xc09e, 0x2224, 0xc0a0, 0x2424, 0xc0a2, 0x2424, 0xc0a4, 0x2424,
        0xc018, 0x0af2, 0xc01a, 0x0d4a, 0xc01c, 0x0f26, 0xc01e, 0x118d, 0xc020, 0x14f3, 0xc022,
        0x175a, 0xc024, 0x19c0, 0xc026, 0x1c26, 0xc089, 0x6050, 0xc08a, 0x5f6e, 0xc08c, 0x6e6e,
        0xc08e, 0x6e6e, 0xc090, 0x6e12,
    ];
    c.phy_set(0xad4e, 0x0010);
    c.phy_modify(0xad16, 0x03ff, 0x03ff);
    c.phy_modify(0xad32, 0x003f, 0x0006);
    c.phy_clear(0xac08, 0x1000);
    c.phy_clear(0xac08, 0x0100);
    c.phy_modify(0xacc0, 0x0003, 0x0002);
    c.phy_modify(0xad40, 0x00e0, 0x0040);
    c.phy_modify(0xad40, 0x0007, 0x0004);
    c.phy_clear(0xac14, 0x0080);
    c.phy_clear(0xac80, 0x0300);
    c.phy_modify(0xac5e, 0x0007, 0x0002);
    c.phy_write(0xad4c, 0x00a8);
    c.phy_write(0xac5c, 0x01ff);
    c.phy_modify(0xac8a, 0x00f0, 0x0030);
    c.phy_write(0xb87c, 0x8157);
    c.phy_modify(0xb87e, 0xff00, 0x0500);
    c.phy_write(0xb87c, 0x8159);
    c.phy_modify(0xb87e, 0xff00, 0x0700);
    c.phy_write(0xb87c, 0x80a2);
    c.phy_write(0xb87e, 0x0153);
    c.phy_write(0xb87c, 0x809c);
    c.phy_write(0xb87e, 0x0153);
    c.phy_write(0xa436, 0x81b3);
    for value in MAC_CFG3_A438_VALUE {
        c.phy_write(0xa438, value);
    }
    for _ in 0..26 {
        c.phy_write(0xa438, 0);
    }
    c.phy_write(0xa436, 0x8257);
    c.phy_write(0xa438, 0x020f);
    c.phy_write(0xa436, 0x80ea);
    c.phy_write(0xa438, 0x7843);
    c.patch_phy_mcu(true);
    c.phy_clear(0xb896, 0x0001);
    c.phy_clear(0xb892, 0xff00);
    for &[at, value] in MAC_CFG3_B88E_VALUE.as_chunks::<2>().0 {
        c.phy_write(0xb88e, at);
        c.phy_write(0xb890, value);
    }
    c.phy_set(0xb896, 0x0001);
    c.patch_phy_mcu(false);
    c.phy_set(0xd068, 0x2000);
    c.phy_write(0xa436, 0x81a2);
    c.phy_set(0xa438, 0x0100);
    c.phy_modify(0xb54c, 0xff00, 0xdb00);
    c.phy_clear(0xa454, 0x0001);
    c.phy_set(0xa5d4, 0x0020);
    c.phy_clear(0xad4e, 0x0010);
    c.phy_clear(0xa86a, 0x0001);
    c.phy_set(0xa442, 0x0800);
    c.phy_set(0xa424, 0x0008);
}

fn r25b(c: &mut Ocp<'_>) {
    c.phy_set(0xa442, 0x0800);
    c.phy_modify(0xac46, 0x00f0, 0x0090);
    c.phy_modify(0xad30, 0x0003, 0x0001);
    c.phy_write(0xb87c, 0x80f5);
    c.phy_write(0xb87e, 0x760e);
    c.phy_write(0xb87c, 0x8107);
    c.phy_write(0xb87e, 0x360e);
    c.phy_write(0xb87c, 0x8551);
    c.phy_modify(0xb87e, 0xff00, 0x0800);
    c.phy_modify(0xbf00, 0xe000, 0xa000);
    c.phy_modify(0xbf46, 0x0f00, 0x0300);
    for i in 0..10 {
        c.phy_write(0xa436, 0x8044 + i * 6);
        c.phy_write(0xa438, 0x2417);
    }
    c.phy_set(0xa4ca, 0x0040);
    c.phy_modify(0xbf84, 0xe000, 0xa000);
    c.phy_write(0xa436, 0x8170);
    c.phy_modify(0xa438, 0x2700, 0xd800);
    c.phy_set(0xa424, 0x0008);
}

fn r25d_1(c: &mut Ocp<'_>) {
    c.phy_set(0xa442, 0x0800);
    c.patch_phy_mcu(true);
    c.phy_set(0xbf96, 0x8000);
    c.phy_modify(0xbf94, 0x0007, 0x0005);
    c.phy_modify(0xbf8e, 0x3c00, 0x2800);
    c.phy_modify(0xbcd8, 0xc000, 0x4000);
    c.phy_set(0xbcd8, 0xc000);
    c.phy_modify(0xbcd8, 0xc000, 0x4000);
    c.phy_modify(0xbc80, 0x001f, 0x0004);
    c.phy_set(0xbc82, 0xe000);
    c.phy_set(0xbc82, 0x1c00);
    c.phy_modify(0xbc80, 0x001f, 0x0005);
    c.phy_modify(0xbc82, 0x00e0, 0x0040);
    c.phy_set(0xbc82, 0x001c);
    c.phy_clear(0xbcd8, 0xc000);
    c.phy_modify(0xbcd8, 0xc000, 0x8000);
    c.phy_clear(0xbcd8, 0xc000);
    c.phy_clear(0xbd70, 0x0100);
    c.phy_set(0xa466, 0x0002);
    c.phy_write(0xa436, 0x836a);
    c.phy_clear(0xa438, 0xff00);
    c.patch_phy_mcu(false);
    c.phy_write(0xb87c, 0x832c);
    c.phy_modify(0xb87e, 0xff00, 0x0500);
    c.phy_modify(0xb106, 0x0700, 0x0100);
    c.phy_modify(0xb206, 0x0700, 0x0200);
    c.phy_modify(0xb306, 0x0700, 0x0300);
    c.phy_write(0xb87c, 0x80cb);
    c.phy_modify(0xb87e, 0xff00, 0x0300);
    c.phy_write(0xbcf4, 0x0000);
    c.phy_write(0xbcf6, 0x0000);
    c.phy_write(0xbc12, 0x0000);
    c.phy_write(0xb87c, 0x844d);
    c.phy_modify(0xb87e, 0xff00, 0x0200);
    c.phy_write(0xb87c, 0x8feb);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8fe9);
    c.phy_modify(0xb87e, 0xff00, 0x0600);
    c.phy_modify(0xac7e, 0x01fc, 0x00b4);
    c.phy_write(0xb87c, 0x8105);
    c.phy_modify(0xb87e, 0xff00, 0x7a00);
    c.phy_write(0xb87c, 0x8117);
    c.phy_modify(0xb87e, 0xff00, 0x3a00);
    c.phy_write(0xb87c, 0x8103);
    c.phy_modify(0xb87e, 0xff00, 0x7400);
    c.phy_write(0xb87c, 0x8115);
    c.phy_modify(0xb87e, 0xff00, 0x3400);
    c.phy_clear(0xad40, 0x0030);
    c.phy_modify(0xad66, 0x000f, 0x0007);
    c.phy_modify(0xad68, 0xf000, 0x8000);
    c.phy_modify(0xad68, 0x0f00, 0x0500);
    c.phy_modify(0xad68, 0x000f, 0x0002);
    c.phy_modify(0xad6a, 0xf000, 0x7000);
    c.phy_write(0xac50, 0x01e8);
    c.phy_write(0xa436, 0x81fa);
    c.phy_modify(0xa438, 0xff00, 0x5400);
    c.phy_modify(0xa864, 0x00f0, 0x00c0);
    c.phy_modify(0xa42c, 0x00ff, 0x0002);
    c.phy_write(0xa436, 0x80e1);
    c.phy_modify(0xa438, 0xff00, 0x0f00);
    c.phy_write(0xa436, 0x80de);
    c.phy_modify(0xa438, 0xf000, 0x0700);
    c.phy_set(0xa846, 0x0080);
    c.phy_write(0xa436, 0x80ba);
    c.phy_write(0xa438, 0x8a04);
    c.phy_write(0xa436, 0x80bd);
    c.phy_modify(0xa438, 0xff00, 0xca00);
    c.phy_write(0xa436, 0x80b7);
    c.phy_modify(0xa438, 0xff00, 0xb300);
    c.phy_write(0xa436, 0x80ce);
    c.phy_write(0xa438, 0x8a04);
    c.phy_write(0xa436, 0x80d1);
    c.phy_modify(0xa438, 0xff00, 0xca00);
    c.phy_write(0xa436, 0x80cb);
    c.phy_modify(0xa438, 0xff00, 0xbb00);
    c.phy_write(0xa436, 0x80a6);
    c.phy_write(0xa438, 0x4909);
    c.phy_write(0xa436, 0x80a8);
    c.phy_write(0xa438, 0x05b8);
    c.phy_write(0xa436, 0x8200);
    c.phy_modify(0xa438, 0xff00, 0x5800);
    c.phy_write(0xa436, 0x8ff1);
    c.phy_write(0xa438, 0x7078);
    c.phy_write(0xa436, 0x8ff3);
    c.phy_write(0xa438, 0x5d78);
    c.phy_write(0xa436, 0x8ff5);
    c.phy_write(0xa438, 0x7862);
    c.phy_write(0xa436, 0x8ff7);
    c.phy_modify(0xa438, 0xff00, 0x1400);
    c.phy_write(0xa436, 0x814c);
    c.phy_write(0xa438, 0x8455);
    c.phy_write(0xa436, 0x814e);
    c.phy_write(0xa438, 0x84a6);
    c.phy_write(0xa436, 0x8163);
    c.phy_modify(0xa438, 0xff00, 0x0600);
    c.phy_write(0xa436, 0x816a);
    c.phy_modify(0xa438, 0xff00, 0x0500);
    c.phy_write(0xa436, 0x8171);
    c.phy_modify(0xa438, 0xff00, 0x1f00);
    c.phy_modify(0xbc3a, 0x000f, 0x0006);
    for i in 0..10 {
        c.phy_write(0xa436, 0x8064 + i * 3);
        c.phy_clear(0xa438, 0x0700);
    }
    c.phy_modify(0xbfa0, 0xff70, 0x5500);
    c.phy_write(0xbfa2, 0x9d00);
    c.phy_write(0xa436, 0x8165);
    c.phy_modify(0xa438, 0x0700, 0x0200);
    c.phy_write(0xa436, 0x8019);
    c.phy_set(0xa438, 0x0100);
    c.phy_write(0xa436, 0x8fe3);
    c.phy_write(0xa438, 0x0005);
    c.phy_write(0xa438, 0x0000);
    c.phy_write(0xa438, 0x00ed);
    c.phy_write(0xa438, 0x0502);
    c.phy_write(0xa438, 0x0b00);
    c.phy_write(0xa438, 0xd401);
    c.phy_modify(0xa438, 0xff00, 0x2900);
    c.phy_write(0xa436, 0x8018);
    c.phy_modify(0xa438, 0xff00, 0x1700);
    c.phy_write(0xa436, 0x815b);
    c.phy_modify(0xa438, 0xff00, 0x1700);
    c.phy_clear(0xa4e0, 0x8000);
    c.phy_clear(0xa5d4, 0x0020);
    c.phy_clear(0xa654, 0x0800);
    c.phy_set(0xa430, 0x1001);
    c.phy_set(0xa442, 0x0080);
}

fn r25d_2(c: &mut Ocp<'_>) {
    c.phy_set(0xa442, 0x0800);
    c.patch_phy_mcu(true);
    c.phy_modify(0xbcd8, 0xc000, 0x4000);
    c.phy_set(0xbcd8, 0xc000);
    c.phy_modify(0xbcd8, 0xc000, 0x4000);
    c.phy_modify(0xbc80, 0x001f, 0x0004);
    c.phy_set(0xbc82, 0xe000);
    c.phy_set(0xbc82, 0x1c00);
    c.phy_modify(0xbc80, 0x001f, 0x0005);
    c.phy_modify(0xbc82, 0x00e0, 0x0040);
    c.phy_set(0xbc82, 0x001c);
    c.phy_clear(0xbcd8, 0xc000);
    c.phy_modify(0xbcd8, 0xc000, 0x8000);
    c.phy_clear(0xbcd8, 0xc000);
    c.patch_phy_mcu(false);
    c.phy_modify(0xac7e, 0x01fc, 0x00b4);
    c.phy_write(0xb87c, 0x8105);
    c.phy_modify(0xb87e, 0xff00, 0x7a00);
    c.phy_write(0xb87c, 0x8117);
    c.phy_modify(0xb87e, 0xff00, 0x3a00);
    c.phy_write(0xb87c, 0x8103);
    c.phy_modify(0xb87e, 0xff00, 0x7400);
    c.phy_write(0xb87c, 0x8115);
    c.phy_modify(0xb87e, 0xff00, 0x3400);
    c.phy_write(0xb87c, 0x8feb);
    c.phy_modify(0xb87e, 0xff00, 0x0500);
    c.phy_write(0xb87c, 0x8fea);
    c.phy_modify(0xb87e, 0xff00, 0x0700);
    c.phy_write(0xb87c, 0x80d6);
    c.phy_modify(0xb87e, 0xff00, 0xef00);
    c.phy_clear(0xa5d4, 0x0020);
    c.phy_clear(0xa654, 0x0800);
    c.phy_clear(0xa448, 0x0400);
    c.phy_clear(0xa586, 0x0400);
    c.phy_set(0xa430, 0x1001);
    c.phy_set(0xa442, 0x0080);
}

fn r26_1(c: &mut Ocp<'_>) {
    const MAC_CFG2_A438_VALUE: [u16; 20] = [
        0x0044, 0x00a8, 0x00d6, 0x00ec, 0x00f6, 0x00fc, 0x00fe, 0x00fe, 0x00bc, 0x0058, 0x002a,
        0x003f, 0x3f02, 0x023c, 0x3b0a, 0x1c00, 0x0000, 0x0000, 0x0000, 0x0000,
    ];
    const MAC_CFG2_B87E_VALUE: [u16; 40] = [
        0x03ed, 0x03ff, 0x0009, 0x03fe, 0x000b, 0x0021, 0x03f7, 0x03b8, 0x03e0, 0x0049, 0x0049,
        0x03e0, 0x03b8, 0x03f7, 0x0021, 0x000b, 0x03fe, 0x0009, 0x03ff, 0x03ed, 0x000e, 0x03fe,
        0x03ed, 0x0006, 0x001a, 0x03f1, 0x03d8, 0x0023, 0x0054, 0x0322, 0x00dd, 0x03ab, 0x03dc,
        0x0027, 0x000e, 0x03e5, 0x03f9, 0x0012, 0x0001, 0x03f1,
    ];
    c.phy_set(0xa442, 0x0800);
    c.phy_write(0xa436, 0x80bf);
    c.phy_modify(0xa438, 0xff00, 0xed00);
    c.phy_write(0xa436, 0x80cd);
    c.phy_modify(0xa438, 0xff00, 0x1000);
    c.phy_write(0xa436, 0x80d1);
    c.phy_modify(0xa438, 0xff00, 0xc800);
    c.phy_write(0xa436, 0x80d4);
    c.phy_modify(0xa438, 0xff00, 0xc800);
    c.phy_write(0xa436, 0x80e1);
    c.phy_write(0xa438, 0x10cc);
    c.phy_write(0xa436, 0x80e5);
    c.phy_write(0xa438, 0x4f0c);
    c.phy_write(0xa436, 0x8387);
    c.phy_modify(0xa438, 0xff00, 0x4700);
    c.phy_modify(0xa80c, 0x00c0, 0x0080);
    c.phy_clear(0xac90, 0x0010);
    c.phy_clear(0xad2c, 0x8000);
    c.phy_write(0xb87c, 0x8321);
    c.phy_modify(0xb87e, 0xff00, 0x1100);
    c.phy_set(0xacf8, 0x000c);
    c.phy_write(0xa436, 0x8183);
    c.phy_modify(0xa438, 0xff00, 0x5900);
    c.phy_set(0xad94, 0x0020);
    c.phy_clear(0xa654, 0x0800);
    c.phy_set(0xb648, 0x4000);
    c.phy_write(0xb87c, 0x839e);
    c.phy_modify(0xb87e, 0xff00, 0x2f00);
    c.phy_write(0xb87c, 0x83f2);
    c.phy_modify(0xb87e, 0xff00, 0x0800);
    c.phy_set(0xada0, 0x0002);
    c.phy_write(0xb87c, 0x80f3);
    c.phy_modify(0xb87e, 0xff00, 0x9900);
    c.phy_write(0xb87c, 0x8126);
    c.phy_modify(0xb87e, 0xff00, 0xc100);
    c.phy_write(0xb87c, 0x893a);
    c.phy_write(0xb87e, 0x8080);
    c.phy_write(0xb87c, 0x8647);
    c.phy_modify(0xb87e, 0xff00, 0xe600);
    c.phy_write(0xb87c, 0x862c);
    c.phy_modify(0xb87e, 0xff00, 0x1200);
    c.phy_write(0xb87c, 0x864a);
    c.phy_modify(0xb87e, 0xff00, 0xe600);
    c.phy_write(0xb87c, 0x80a0);
    c.phy_write(0xb87e, 0xbcbc);
    c.phy_write(0xb87c, 0x805e);
    c.phy_write(0xb87e, 0xbcbc);
    c.phy_write(0xb87c, 0x8056);
    c.phy_write(0xb87e, 0x3077);
    c.phy_write(0xb87c, 0x8058);
    c.phy_modify(0xb87e, 0xff00, 0x5a00);
    c.phy_write(0xb87c, 0x8098);
    c.phy_write(0xb87e, 0x3077);
    c.phy_write(0xb87c, 0x809a);
    c.phy_modify(0xb87e, 0xff00, 0x5a00);
    c.phy_write(0xb87c, 0x8052);
    c.phy_write(0xb87e, 0x3733);
    c.phy_write(0xb87c, 0x8094);
    c.phy_write(0xb87e, 0x3733);
    c.phy_write(0xb87c, 0x807f);
    c.phy_write(0xb87e, 0x7c75);
    c.phy_write(0xb87c, 0x803d);
    c.phy_write(0xb87e, 0x7c75);
    c.phy_write(0xb87c, 0x8036);
    c.phy_modify(0xb87e, 0xff00, 0x3000);
    c.phy_write(0xb87c, 0x8078);
    c.phy_modify(0xb87e, 0xff00, 0x3000);
    c.phy_write(0xb87c, 0x8031);
    c.phy_modify(0xb87e, 0xff00, 0x3300);
    c.phy_write(0xb87c, 0x8073);
    c.phy_modify(0xb87e, 0xff00, 0x3300);
    c.phy_modify(0xae06, 0xfc00, 0x7c00);
    c.phy_write(0xb87c, 0x89d1);
    c.phy_write(0xb87e, 0x0004);
    c.phy_write(0xa436, 0x8fbd);
    c.phy_modify(0xa438, 0xff00, 0x0a00);
    c.phy_write(0xa436, 0x8fbe);
    c.phy_write(0xa438, 0x0d09);
    c.phy_write(0xb87c, 0x89cd);
    c.phy_write(0xb87e, 0x0f0f);
    c.phy_write(0xb87c, 0x89cf);
    c.phy_write(0xb87e, 0x0f0f);
    c.phy_write(0xb87c, 0x83a4);
    c.phy_write(0xb87e, 0x6600);
    c.phy_write(0xb87c, 0x83a6);
    c.phy_write(0xb87e, 0x6601);
    c.phy_write(0xb87c, 0x83c0);
    c.phy_write(0xb87e, 0x6600);
    c.phy_write(0xb87c, 0x83c2);
    c.phy_write(0xb87e, 0x6601);
    c.phy_write(0xb87c, 0x8414);
    c.phy_write(0xb87e, 0x6600);
    c.phy_write(0xb87c, 0x8416);
    c.phy_write(0xb87e, 0x6601);
    c.phy_write(0xb87c, 0x83f8);
    c.phy_write(0xb87e, 0x6600);
    c.phy_write(0xb87c, 0x83fa);
    c.phy_write(0xb87e, 0x6601);
    c.patch_phy_mcu(true);
    c.phy_modify(0xbd96, 0x1f00, 0x1000);
    c.phy_modify(0xbf1c, 0x0007, 0x0007);
    c.phy_clear(0xbfbe, 0x8000);
    c.phy_modify(0xbf40, 0x0380, 0x0280);
    c.phy_modify(0xbf90, 0x0080, 0x0060);
    c.phy_modify(0xbf90, 0x0010, 0x000c);
    c.patch_phy_mcu(false);
    c.phy_write(0xa436, 0x843b);
    c.phy_modify(0xa438, 0xff00, 0x2000);
    c.phy_write(0xa436, 0x843d);
    c.phy_modify(0xa438, 0xff00, 0x2000);
    c.phy_clear(0xb516, 0x007f);
    c.phy_clear(0xbf80, 0x0030);
    c.phy_write(0xa436, 0x8188);
    for &value in &MAC_CFG2_A438_VALUE[..11] {
        c.phy_write(0xa438, value);
    }
    c.phy_write(0xb87c, 0x8015);
    c.phy_modify(0xb87e, 0xff00, 0x0800);
    c.phy_write(0xb87c, 0x8ffd);
    c.phy_modify(0xb87e, 0xff00, 0x0000);
    c.phy_write(0xb87c, 0x8fff);
    c.phy_modify(0xb87e, 0xff00, 0x7f00);
    c.phy_write(0xb87c, 0x8ffb);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8fe9);
    c.phy_write(0xb87e, 0x0002);
    c.phy_write(0xb87c, 0x8fef);
    c.phy_write(0xb87e, 0x00a5);
    c.phy_write(0xb87c, 0x8ff1);
    c.phy_write(0xb87e, 0x0106);
    c.phy_write(0xb87c, 0x8fe1);
    c.phy_write(0xb87e, 0x0102);
    c.phy_write(0xb87c, 0x8fe3);
    c.phy_modify(0xb87e, 0xff00, 0x0400);
    c.phy_set(0xa654, 0x0800);
    c.phy_clear(0xa654, 0x0003);
    c.phy_write(0xac3a, 0x5851);
    c.phy_modify(0xac3c, 0xd000, 0x2000);
    c.phy_modify(0xac42, 0x0200, 0x01c0);
    c.phy_clear(0xac3e, 0xe000);
    c.phy_clear(0xac42, 0x0038);
    c.phy_modify(0xac42, 0x0002, 0x0005);
    c.phy_write(0xac1a, 0x00db);
    c.phy_write(0xade4, 0x01b5);
    c.phy_clear(0xad9c, 0x0c00);
    c.phy_write(0xb87c, 0x814b);
    c.phy_modify(0xb87e, 0xff00, 0x1100);
    c.phy_write(0xb87c, 0x814d);
    c.phy_modify(0xb87e, 0xff00, 0x1100);
    c.phy_write(0xb87c, 0x814f);
    c.phy_modify(0xb87e, 0xff00, 0x0b00);
    c.phy_write(0xb87c, 0x8142);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8144);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8150);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8118);
    c.phy_modify(0xb87e, 0xff00, 0x0700);
    c.phy_write(0xb87c, 0x811a);
    c.phy_modify(0xb87e, 0xff00, 0x0700);
    c.phy_write(0xb87c, 0x811c);
    c.phy_modify(0xb87e, 0xff00, 0x0500);
    c.phy_write(0xb87c, 0x810f);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8111);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x811d);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_set(0xac36, 0x1000);
    c.phy_clear(0xad1c, 0x0100);
    c.phy_modify(0xade8, 0xffc0, 0x1400);
    c.phy_write(0xb87c, 0x864b);
    c.phy_modify(0xb87e, 0xff00, 0x9d00);
    c.phy_write(0xa436, 0x8f97);
    for &value in &MAC_CFG2_A438_VALUE[11..] {
        c.phy_write(0xa438, value);
    }
    c.phy_set(0xad9c, 0x0020);
    c.phy_write(0xb87c, 0x8122);
    c.phy_modify(0xb87e, 0xff00, 0x0c00);
    c.phy_write(0xb87c, 0x82c8);
    for &value in &MAC_CFG2_B87E_VALUE[..20] {
        c.phy_write(0xb87e, value);
    }
    c.phy_write(0xb87c, 0x80ef);
    c.phy_modify(0xb87e, 0xff00, 0x0c00);
    c.phy_write(0xb87c, 0x82a0);
    for &value in &MAC_CFG2_B87E_VALUE[20..] {
        c.phy_write(0xb87e, value);
    }
    c.phy_write(0xa436, 0x8018);
    c.phy_set(0xa438, 0x2000);
    c.phy_write(0xb87c, 0x8fe4);
    c.phy_modify(0xb87e, 0xff00, 0x0000);
    c.phy_modify(0xb54c, 0xffc0, 0x3700);
}

fn r26_2(c: &mut Ocp<'_>) {
    const MAC_CFG3_B87E_VALUE: [u16; 41] = [
        0x03ed, 0x03ff, 0x0009, 0x03fe, 0x000b, 0x0021, 0x03f7, 0x03b8, 0x03e0, 0x0049, 0x0049,
        0x03e0, 0x03b8, 0x03f7, 0x0021, 0x000b, 0x03fe, 0x0009, 0x03ff, 0x03ed, 0x82a0, 0x000e,
        0x03fe, 0x03ed, 0x0006, 0x001a, 0x03f1, 0x03d8, 0x0023, 0x0054, 0x0322, 0x00dd, 0x03ab,
        0x03dc, 0x0027, 0x000e, 0x03e5, 0x03f9, 0x0012, 0x0001, 0x03f1,
    ];
    c.phy_set(0xa442, 0x0800);
    c.phy_write(0xa436, 0x8183);
    c.phy_modify(0xa438, 0xff00, 0x5900);
    c.phy_set(0xa654, 0x0800);
    c.phy_set(0xb648, 0x4000);
    c.phy_set(0xad2c, 0x8000);
    c.phy_set(0xad94, 0x0020);
    c.phy_set(0xada0, 0x0002);
    c.phy_modify(0xae06, 0xfc00, 0x7c00);
    c.phy_write(0xb87c, 0x8647);
    c.phy_modify(0xb87e, 0xff00, 0xe600);
    c.phy_write(0xb87c, 0x8036);
    c.phy_modify(0xb87e, 0xff00, 0x3000);
    c.phy_write(0xb87c, 0x8078);
    c.phy_modify(0xb87e, 0xff00, 0x3000);
    c.phy_write(0xb87c, 0x89e9);
    c.phy_set(0xb87e, 0xff00);
    c.phy_write(0xb87c, 0x8ffd);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8ffe);
    c.phy_modify(0xb87e, 0xff00, 0x0200);
    c.phy_write(0xb87c, 0x8fff);
    c.phy_modify(0xb87e, 0xff00, 0x0400);
    c.phy_write(0xa436, 0x8018);
    c.phy_modify(0xa438, 0xff00, 0x7700);
    c.phy_write(0xa436, 0x8f9c);
    c.phy_write(0xa438, 0x0005);
    c.phy_write(0xa438, 0x0000);
    c.phy_write(0xa438, 0x00ed);
    c.phy_write(0xa438, 0x0502);
    c.phy_write(0xa438, 0x0b00);
    c.phy_write(0xa438, 0xd401);
    c.phy_write(0xa436, 0x8fa8);
    c.phy_modify(0xa438, 0xff00, 0x2900);
    c.phy_write(0xb87c, 0x814b);
    c.phy_modify(0xb87e, 0xff00, 0x1100);
    c.phy_write(0xb87c, 0x814d);
    c.phy_modify(0xb87e, 0xff00, 0x1100);
    c.phy_write(0xb87c, 0x814f);
    c.phy_modify(0xb87e, 0xff00, 0x0b00);
    c.phy_write(0xb87c, 0x8142);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8144);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8150);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8118);
    c.phy_modify(0xb87e, 0xff00, 0x0700);
    c.phy_write(0xb87c, 0x811a);
    c.phy_modify(0xb87e, 0xff00, 0x0700);
    c.phy_write(0xb87c, 0x811c);
    c.phy_modify(0xb87e, 0xff00, 0x0500);
    c.phy_write(0xb87c, 0x810f);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x8111);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_write(0xb87c, 0x811d);
    c.phy_modify(0xb87e, 0xff00, 0x0100);
    c.phy_set(0xad1c, 0x0100);
    c.phy_modify(0xade8, 0xffc0, 0x1400);
    c.phy_write(0xb87c, 0x864b);
    c.phy_modify(0xb87e, 0xff00, 0x9d00);
    c.phy_write(0xb87c, 0x862c);
    c.phy_modify(0xb87e, 0xff00, 0x1200);
    c.phy_write(0xa436, 0x8566);
    c.phy_write(0xa438, 0x003f);
    c.phy_write(0xa438, 0x3f02);
    c.phy_write(0xa438, 0x023c);
    c.phy_write(0xa438, 0x3b0a);
    c.phy_write(0xa438, 0x1c00);
    c.phy_write(0xa438, 0x0000);
    c.phy_write(0xa438, 0x0000);
    c.phy_write(0xa438, 0x0000);
    c.phy_write(0xa438, 0x0000);
    c.phy_set(0xad9c, 0x0020);
    c.phy_write(0xb87c, 0x8122);
    c.phy_modify(0xb87e, 0xff00, 0x0c00);
    c.phy_write(0xb87c, 0x82c8);
    for &value in &MAC_CFG3_B87E_VALUE[..20] {
        c.phy_write(0xb87e, value);
    }
    c.phy_write(0xb87c, 0x80ef);
    c.phy_modify(0xb87e, 0xff00, 0x0c00);
    for &value in &MAC_CFG3_B87E_VALUE[20..] {
        c.phy_write(0xb87e, value);
    }
    c.phy_set(0xa430, 0x0003);
    c.phy_modify(0xb54c, 0xffc0, 0x3700);
    c.phy_set(0xb648, 0x0040);
    c.phy_write(0xb87c, 0x8082);
    c.phy_modify(0xb87e, 0xff00, 0x5d00);
    c.phy_write(0xb87c, 0x807c);
    c.phy_modify(0xb87e, 0xff00, 0x5000);
    c.phy_write(0xb87c, 0x809d);
    c.phy_modify(0xb87e, 0xff00, 0x5000);
}
