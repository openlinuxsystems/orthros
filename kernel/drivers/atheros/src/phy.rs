// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The PHY inside the chip, Atheros' F1 and its successors, which
//! FreeBSD's `atphy(4)` drives: reached through the chip's MDIO
//! register, with debug registers behind two of its own and, on the
//! AR816x, extended registers behind another of the chip's.
//!
//! The driver resets it as FreeBSD's alc_phy_reset() does, with the
//! vendor's values for its analog parts and its hibernation off, sets it
//! up as `atphy(4)` does, and has it negotiate every speed it has; the
//! link is read from its specific status register, which says what it
//! resolved.
//!
//! FreeBSD writes two of the AR813x's analog values with a shift and a
//! mask mixed up; they are written here as intended, which are the
//! defaults the AR816x's own registers name (`DBG_TST100BTCFG_DEFAULT`,
//! `DBG_ANACTL_DEFAULT`).

use radix_hw::Mmio;
use radix_nic::{Link, wait_for};

use crate::{Chip, device, reg};

/// The MDIO register's bits.
mod mdio {
    pub const DATA: u32 = 0x0000_ffff;
    pub const REG_SHIFT: u32 = 16;
    pub const REG_MASK: u32 = 0x001f_0000;
    pub const READ: u32 = 0x0020_0000;
    pub const SUP_PREAMBLE: u32 = 0x0040_0000;
    pub const EXECUTE: u32 = 0x0080_0000;
    /// The MDIO clock: the chip's 25 MHz divided by 4, or by 128 on an
    /// AR816x whose link is up.
    pub const CLK_25_4: u32 = 0x0000_0000;
    pub const CLK_25_128: u32 = 0x0700_0000;
    pub const BUSY: u32 = 0x0800_0000;
    pub const MODE_EXT: u32 = 0x4000_0000;
    pub const EXT_DEVADDR_SHIFT: u32 = 16;
}

/// The GPHY configuration's bits.
mod gphy {
    pub const EXT_RESET: u32 = 0x0001;
    pub const LED_MODE: u32 = 0x0004;
    pub const GATE_25M_ENB: u32 = 0x0020;
    pub const PHY_IDDQ: u32 = 0x0080;
    pub const HIB_EN: u32 = 0x0400;
    pub const HIB_PULSE: u32 = 0x0800;
    pub const SEL_ANA_RESET: u32 = 0x1000;
    pub const PHY_PLL_ON: u32 = 0x2000;
    pub const PWDOWN_HW: u32 = 0x4000;
    pub const AB_100_ENB: u32 = 0x0002_0000;
}

/// The PHY's registers: the standard ones, Atheros' specific control and
/// status, and the two that reach its debug registers.
const BMCR: u32 = 0x00;
const BMSR: u32 = 0x01;
const ANAR: u32 = 0x04;
const GTCR: u32 = 0x09;
const SCR: u32 = 0x10;
const SSR: u32 = 0x11;
const DEBUG_ADDRESS: u32 = 0x1d;
const DEBUG_DATA: u32 = 0x1e;

const BMCR_RESTART: u16 = 0x0200;
const BMCR_ISOLATE: u16 = 0x0400;
const BMCR_AUTONEG: u16 = 0x1000;
const BMCR_RESET: u16 = 0x8000;
const BMSR_LINK: u16 = 0x0004;
/// BMSR's abilities, which make ANAR's when shifted.
const BMSR_MEDIA: u16 = 0x7800;
const ANAR_CSMA: u16 = 0x0001;
/// 10 and 100 Mb/s, half and full duplex: what a PHY that says nothing
/// has.
const ANAR_ALL: u16 = 0x01e1;
const GTCR_1000: u16 = 0x0300;
const SCR_POLARITY_REVERSAL: u16 = 0x0002;
const SCR_MAC_PDOWN: u16 = 0x0008;
const SCR_AUTO_X_MODE: u16 = 0x0060;
const SCR_ASSERT_CRS_ON_TX: u16 = 0x0800;
const SSR_RESOLVED: u16 = 0x0800;
const SSR_DUPLEX: u16 = 0x2000;
const SSR_SPEED: u16 = 0xc000;
const SSR_1000: u16 = 0x8000;
const SSR_100: u16 = 0x4000;

/// The debug registers.
mod debug {
    pub const ANACTL: u16 = 0x00;
    pub const SRDSYSMOD: u16 = 0x05;
    pub const HIBNEG: u16 = 0x0b;
    pub const TST10BTCFG: u16 = 0x12;
    pub const LEGCYPS: u16 = 0x29;
    pub const TST100BTCFG: u16 = 0x36;
    pub const GREENCFG: u16 = 0x3b;
    pub const GREENCFG2: u16 = 0x3d;
    /// The AR813x's analog configuration, at the same numbers.
    pub const ANA_CFG0: u16 = 0x00;
    pub const ANA_CFG4: u16 = 0x04;
    pub const ANA_CFG5: u16 = 0x05;
    pub const ANA_CFG11: u16 = 0x0b;
    pub const ANA_CFG18: u16 = 0x12;
    pub const ANA_CFG41: u16 = 0x29;
    pub const ANA_CFG54: u16 = 0x36;
    /// The vendor's values.
    pub const ANACTL_DEFAULT: u16 = 0x02ef;
    pub const SRDSYSMOD_DEFAULT: u16 = 0x2c46;
    pub const HIBNEG_DEFAULT: u16 = 0xbc40;
    pub const HIBNEG_PSHIB_EN: u16 = 0x8000;
    pub const HIBNEG_HIB_PULSE: u16 = 0x1000;
    pub const TST10BTCFG_DEFAULT: u16 = 0x4c04;
    pub const LEGCYPS_DEFAULT: u16 = 0x129d;
    pub const LEGCYPS_ENB: u16 = 0x8000;
    pub const TST100BTCFG_DEFAULT: u16 = 0xe12c;
    pub const GREENCFG_DEFAULT: u16 = 0x7078;
    pub const GREENCFG2_GATE_DFSE_EN: u16 = 0x0080;
    pub const GREENCFG2_BP_GREEN: u16 = 0x8000;
    /// The AR813x's: 10BASE-T loopback, idle and test-bit masking, and a
    /// timer interval of 1.
    pub const ANA_CFG18_VALUE: u16 = 0x0004 | 0x0800 | 0x0400 | (1 << 14);
    /// The SerDes clock recovery's bandwidth 2, de-emphasis, high speed,
    /// the PLL and lock detection.
    pub const ANA_CFG5_VALUE: u16 = 2 | 0x0040 | 0x0400 | 0x0800 | 0x2000;
    /// Long and short cable thresholds of 44 and 33 at 100 Mb/s, with
    /// the bad link accumulator and the small bandwidth bypassed.
    pub const ANA_CFG54_VALUE: u16 = 0x2c | (33 << 8) | 0x4000 | 0x8000;
    /// The echo adjustments.
    pub const ANA_CFG4_VALUE: u16 = 0xb | (11 << 4) | (8 << 8) | (8 << 12);
    /// Calibration restarted, a manual switch of 7, high speed, the
    /// bias and the 125 MHz output.
    pub const ANA_CFG0_VALUE: u16 = 0x0001 | (7 << 1) | 0x0020 | 0x0040 | 0x0080 | 0x0200;
    /// The bit that has the PHY hibernate, in two registers.
    pub const HIBERNATE: u16 = 0x8000;
}

/// The extended registers, by device and number.
mod ext {
    pub const PCS: u32 = 3;
    pub const ANEG: u32 = 7;
    pub const CLDCTL3: u16 = 0x8003;
    pub const CLDCTL3_BP_CABLE1TH_DET_GT: u16 = 0x8000;
    pub const CLDCTL5: u16 = 0x8005;
    pub const CLDCTL5_BP_VD_HLFBIAS: u16 = 0x4000;
    pub const LOCAL_EEEADV: u16 = 0x3c;
    pub const S3DIG10: u16 = 0x8023;
    pub const NLP78: u16 = 0x8027;
    pub const NLP78_120M_DEFAULT: u16 = 0x8a05;
}

/// MDIO's time: 10 000 looks 5 µs apart.
const MDIO_US: u32 = 50_000;
const MDIO_STEP_US: u32 = 5;
/// The time a PHY's reset takes to clear.
const BMCR_RESET_US: u32 = 1_000;

/// The PHY, as one access to it needs it.
struct Phy<'a> {
    regs: &'a Mmio,
    chip: &'a Chip,
    /// The MDIO clock.
    clock: u32,
    delay: &'a mut dyn FnMut(u32),
}

impl<'a> Phy<'a> {
    /// The PHY of `chip`, whose link is up or not.
    fn new(regs: &'a Mmio, chip: &'a Chip, link_up: bool, delay: &'a mut dyn FnMut(u32)) -> Self {
        let clock = if chip.ar816x() && link_up { mdio::CLK_25_128 } else { mdio::CLK_25_4 };
        Phy { regs, chip, clock, delay }
    }

    /// Runs an MDIO operation, and waits for it to end: the value read.
    fn run(&mut self, command: u32) -> Option<u16> {
        let regs = self.regs;
        regs.write32(reg::MDIO, command | mdio::EXECUTE | mdio::SUP_PREAMBLE | self.clock);
        let busy = if self.chip.ar816x() { mdio::BUSY } else { mdio::EXECUTE | mdio::BUSY };
        let mut value = 0;
        let done = wait_for(self.delay, MDIO_US, MDIO_STEP_US, || {
            value = regs.read32(reg::MDIO);
            value & busy == 0
        });
        done.then_some((value & mdio::DATA) as u16)
    }

    fn address(phy: u32) -> u32 {
        (phy << mdio::REG_SHIFT) & mdio::REG_MASK
    }

    fn read(&mut self, phy: u32) -> Option<u16> {
        self.run(mdio::READ | Phy::address(phy))
    }

    fn write(&mut self, phy: u32, value: u16) {
        let _ = self.run(Phy::address(phy) | u32::from(value));
    }

    fn debug_read(&mut self, at: u16) -> Option<u16> {
        self.write(DEBUG_ADDRESS, at);
        self.read(DEBUG_DATA)
    }

    fn debug_write(&mut self, at: u16, value: u16) {
        self.write(DEBUG_ADDRESS, at);
        self.write(DEBUG_DATA, value);
    }

    /// Changes debug register `at`: its bits in `clear` cleared and those
    /// in `set` set, if it could be read.
    fn debug_change(&mut self, at: u16, clear: u16, set: u16) {
        if let Some(value) = self.debug_read(at) {
            self.debug_write(at, (value & !clear) | set);
        }
    }

    fn ext_select(&mut self, device: u32, at: u16) {
        self.regs.write32(reg::EXT_MDIO, u32::from(at) | (device << mdio::EXT_DEVADDR_SHIFT));
    }

    fn ext_read(&mut self, device: u32, at: u16) -> Option<u16> {
        self.ext_select(device, at);
        self.run(mdio::READ | mdio::MODE_EXT)
    }

    fn ext_write(&mut self, device: u32, at: u16, value: u16) {
        self.ext_select(device, at);
        let _ = self.run(mdio::MODE_EXT | u32::from(value));
    }

    fn ext_set(&mut self, device: u32, at: u16, set: u16) {
        if let Some(value) = self.ext_read(device, at) {
            self.ext_write(device, at, value | set);
        }
    }
}

/// Resets the PHY and sets its analog parts and power saving as the
/// vendor's driver does, with hibernation off.
pub(crate) fn reset(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    if chip.ar816x() {
        reset_816x(regs, chip, delay);
    } else {
        reset_813x(regs, chip, delay);
    }
}

fn reset_813x(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    regs.write16(reg::GPHY_CFG, gphy::SEL_ANA_RESET as u16);
    let _ = regs.read16(reg::GPHY_CFG);
    delay(10_000);
    regs.write16(reg::GPHY_CFG, (gphy::EXT_RESET | gphy::SEL_ANA_RESET) as u16);
    let _ = regs.read16(reg::GPHY_CFG);
    delay(10_000);
    let id = chip.device();
    let mut phy = Phy::new(regs, chip, false, delay);
    if id == device::AR8152_B {
        phy.debug_change(0x000a, 0x2000, 0);
    }
    if matches!(id, device::AR8151 | device::AR8151_V2 | device::AR8152_B | device::AR8152_B2) {
        phy.debug_change(debug::GREENCFG, 0x0008, 0);
        (phy.delay)(20_000);
    }
    if id == device::AR8151 {
        phy.debug_write(debug::ANA_CFG41, 0x929d);
    }
    if matches!(id, device::AR8131 | device::AR8132 | device::AR8151_V2 | device::AR8152_B2) {
        phy.debug_write(debug::ANA_CFG41, 0xb6dd);
    }
    phy.debug_write(debug::ANA_CFG18, debug::ANA_CFG18_VALUE);
    phy.debug_write(debug::ANA_CFG5, debug::ANA_CFG5_VALUE);
    phy.debug_write(debug::ANA_CFG54, debug::ANA_CFG54_VALUE);
    phy.debug_write(debug::ANA_CFG4, debug::ANA_CFG4_VALUE);
    phy.debug_write(debug::ANA_CFG0, debug::ANA_CFG0_VALUE);
    (phy.delay)(1_000);
    phy.debug_change(debug::ANA_CFG41, debug::HIBERNATE, 0);
    phy.debug_change(debug::ANA_CFG11, debug::HIBERNATE, 0);
}

fn reset_816x(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    let off = gphy::EXT_RESET
        | gphy::LED_MODE
        | gphy::GATE_25M_ENB
        | gphy::PHY_IDDQ
        | gphy::PHY_PLL_ON
        | gphy::PWDOWN_HW
        | gphy::AB_100_ENB
        | gphy::HIB_PULSE
        | gphy::HIB_EN;
    let config = (regs.read32(reg::GPHY_CFG) & !off) | gphy::SEL_ANA_RESET;
    regs.write32(reg::GPHY_CFG, config);
    delay(10);
    regs.write32(reg::GPHY_CFG, config | gphy::EXT_RESET);
    delay(800);
    let mut phy = Phy::new(regs, chip, false, delay);
    phy.debug_write(debug::LEGCYPS, debug::LEGCYPS_DEFAULT & !debug::LEGCYPS_ENB);
    phy.debug_write(
        debug::HIBNEG,
        debug::HIBNEG_DEFAULT & !(debug::HIBNEG_PSHIB_EN | debug::HIBNEG_HIB_PULSE),
    );
    phy.debug_write(debug::GREENCFG, debug::GREENCFG_DEFAULT);
    // Energy-efficient Ethernet off, which FreeBSD leaves off.
    let lpi = regs.read32(reg::LPI_CTL);
    regs.write32(reg::LPI_CTL, lpi & !crate::LPI_CTL_ENB);
    phy.ext_write(ext::ANEG, ext::LOCAL_EEEADV, 0);
    phy.debug_write(debug::TST10BTCFG, debug::TST10BTCFG_DEFAULT);
    phy.debug_write(debug::SRDSYSMOD, debug::SRDSYSMOD_DEFAULT);
    phy.debug_write(debug::TST100BTCFG, debug::TST100BTCFG_DEFAULT);
    phy.debug_write(debug::ANACTL, debug::ANACTL_DEFAULT);
    phy.debug_change(debug::GREENCFG2, debug::GREENCFG2_GATE_DFSE_EN, 0);
    // For a link to an RTL8139C over 120 m of cable.
    phy.ext_write(ext::ANEG, ext::NLP78, ext::NLP78_120M_DEFAULT);
    phy.ext_write(ext::ANEG, ext::S3DIG10, 0);
    if chip.link_war {
        phy.ext_set(ext::PCS, ext::CLDCTL3, ext::CLDCTL3_BP_CABLE1TH_DET_GT);
        phy.debug_change(debug::GREENCFG2, 0, debug::GREENCFG2_BP_GREEN);
        phy.ext_set(ext::PCS, ext::CLDCTL5, ext::CLDCTL5_BP_VD_HLFBIAS);
    }
}

/// Keeps the AR8151 and AR8152's PHY from hibernating while TWSI loads
/// the EEPROM (`before`), and lets it again after.
pub(crate) fn hibernation_for_eeprom(
    regs: &Mmio,
    chip: &Chip,
    before: bool,
    delay: &mut dyn FnMut(u32),
) {
    let mut phy = Phy::new(regs, chip, false, delay);
    if before {
        phy.debug_change(0x00, 0x0080, 0);
        phy.debug_change(debug::GREENCFG, 0, 0x0008);
    } else {
        phy.debug_change(0x00, 0, 0x0080);
        phy.debug_change(debug::GREENCFG, 0x0008, 0);
    }
    (phy.delay)(20);
}

/// Sets the PHY up as FreeBSD's atphy_reset() does: out of power down,
/// its hibernation off, crossover and polarity found by itself; then has
/// it negotiate.
pub(crate) fn start(regs: &Mmio, chip: &Chip, delay: &mut dyn FnMut(u32)) {
    let mut phy = Phy::new(regs, chip, false, delay);
    phy.debug_write(debug::LEGCYPS, 0);
    if let Some(control) = phy.read(SCR) {
        let control = (control | SCR_AUTO_X_MODE | SCR_ASSERT_CRS_ON_TX | SCR_POLARITY_REVERSAL)
            & !SCR_MAC_PDOWN;
        phy.write(SCR, control);
    }
    negotiate_with(&mut phy);
}

/// Has the PHY negotiate the link again, with every speed it has.
pub(crate) fn negotiate(regs: &Mmio, chip: &Chip, link_up: bool, delay: &mut dyn FnMut(u32)) {
    let mut phy = Phy::new(regs, chip, link_up, delay);
    negotiate_with(&mut phy);
}

fn negotiate_with(phy: &mut Phy<'_>) {
    let anar = match phy.read(BMSR) {
        Some(status) if status != 0 && status != u16::MAX => {
            ((status & BMSR_MEDIA) >> 6) | ANAR_CSMA
        }
        _ => ANAR_ALL,
    };
    phy.write(ANAR, anar);
    // A fast chip's PHY is the gigabit one, without gigabit: it must not
    // offer it.
    let gigabit = if phy.chip.is(crate::flag::FAST) { 0 } else { GTCR_1000 };
    phy.write(GTCR, gigabit);
    phy.write(BMCR, BMCR_RESET | BMCR_AUTONEG | BMCR_RESTART);
    for _ in 0..BMCR_RESET_US {
        if phy.read(BMCR).is_none_or(|control| control & BMCR_RESET == 0) {
            break;
        }
        (phy.delay)(1);
    }
}

/// The link the PHY negotiated, as its status registers say: down until
/// it resolved a speed the chip has.
pub(crate) fn link(regs: &Mmio, chip: &Chip, link_up: bool, delay: &mut dyn FnMut(u32)) -> Link {
    let mut phy = Phy::new(regs, chip, link_up, delay);
    // The link bit stays down once it went down until it is read.
    let status = phy.read(BMSR).unwrap_or(0) | phy.read(BMSR).unwrap_or(0);
    if status == u16::MAX || status & BMSR_LINK == 0 {
        return Link::default();
    }
    if phy.read(BMCR).is_none_or(|control| control & BMCR_ISOLATE != 0) {
        return Link::default();
    }
    let Some(specific) = phy.read(SSR) else { return Link::default() };
    if specific & SSR_RESOLVED == 0 {
        return Link::default();
    }
    let mbps = match specific & SSR_SPEED {
        SSR_1000 => 1000,
        SSR_100 => 100,
        0 => 10,
        _ => return Link::default(),
    };
    if mbps == 1000 && chip.is(crate::flag::FAST) {
        return Link::default();
    }
    Link { up: true, mbps, full_duplex: specific & SSR_DUPLEX != 0 }
}
