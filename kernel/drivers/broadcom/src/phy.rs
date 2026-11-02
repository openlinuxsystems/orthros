// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chip's copper PHY, reached through its MDIO register, as FreeBSD's
//! `brgphy(4)` drives Broadcom's PHYs for `bge(4)`: reset, given the
//! fixes its chip needs, set negotiating, and asked for the link it
//! resolved, which its auxiliary status register says.

use radix_hw::Mmio;
use radix_nic::Link;

use crate::Chip;
use crate::reg::{MI_COMM, mi};

/// The standard registers.
const BMCR: u16 = 0x00;
const BMSR: u16 = 0x01;
const ANAR: u16 = 0x04;
const GTCR: u16 = 0x09;
/// Broadcom's.
const EXTCTL: u16 = 0x10;
const DSP_RW_PORT: u16 = 0x15;
const DSP_ADDR: u16 = 0x17;
const AUXCTL: u16 = 0x18;
const AUXSTS: u16 = 0x19;
const IMR: u16 = 0x1b;

const BMCR_RESET: u16 = 0x8000;
const BMCR_AUTOEN: u16 = 0x1000;
const BMCR_STARTNEG: u16 = 0x0200;
const BMSR_ACOMP: u16 = 0x0020;
/// The abilities BMSR lists, which shifted are ANAR's.
const BMSR_MEDIA: u16 = 0xf800;
const BMSR_MEDIA_TO_ANAR: u32 = 6;
const ANAR_CSMA: u16 = 0x0001;
/// 10 and 100 Mb/s, both duplexes: what is advertised when BMSR reads
/// no ability.
const ANAR_ALL: u16 = 0x01e0;
const GTCR_ADV_1000: u16 = 0x0200 | 0x0100;
const EXTCTL_3_LED: u16 = 0x0002;
const AUXSTS_LINK: u16 = 0x0004;
const AUXSTS_RESOLVED: u16 = 0x0700;
/// The PHY's interrupts FreeBSD unmasks: its low eight, the link's
/// changes among them.
const IMR_LINK: u16 = 0xff00;

/// How long a reset may take: 100 looks, a millisecond apart.
const RESET_LOOKS: u32 = 100;
/// How long one MDIO access may take: FreeBSD's 100,000 looks, 10 µs
/// apart.
const MDIO_LOOKS: u32 = 100_000;

/// The PHY of `chip`, whose MAC's registers are `regs`.
pub struct Phy<'a> {
    regs: &'a Mmio,
    chip: &'a Chip,
    delay: &'a mut dyn FnMut(u32),
}

impl<'a> Phy<'a> {
    pub fn new(regs: &'a Mmio, chip: &'a Chip, delay: &'a mut dyn FnMut(u32)) -> Phy<'a> {
        Phy { regs, chip, delay }
    }

    /// Runs one MDIO command, under the APE's lock on the PHY when the
    /// chip has an APE: the register it read, or `None` when the lock or
    /// the command failed.
    fn run(&mut self, command: u32) -> Option<u16> {
        let ape = self.chip.ape.as_ref();
        if let Some(ape) = ape
            && !ape.lock(ape.phy_lock(), self.delay)
        {
            return None;
        }
        self.regs.write32(MI_COMM, command | mi::COMM_BUSY);
        let mut value = None;
        for _ in 0..MDIO_LOOKS {
            (self.delay)(10);
            if self.regs.read32(MI_COMM) & mi::COMM_BUSY == 0 {
                (self.delay)(5);
                value = Some(self.regs.read32(MI_COMM));
                break;
            }
        }
        if let Some(ape) = ape {
            ape.unlock(ape.phy_lock());
        }
        value.filter(|value| value & mi::COMM_READFAIL == 0).map(|value| value as u16)
    }

    fn read(&mut self, reg: u16) -> u16 {
        let phy = self.chip.phy_address;
        self.run(mi::CMD_READ | mi::phy(phy) | mi::reg(reg)).unwrap_or(0)
    }

    fn write(&mut self, reg: u16, value: u16) {
        let phy = self.chip.phy_address;
        let _ = self.run(mi::CMD_WRITE | mi::phy(phy) | mi::reg(reg) | u32::from(value));
    }

    /// Resets the PHY and applies what FreeBSD's `brgphy_reset()` does
    /// for the chip: nothing for the BCM5717 and BCM57765 families' PHYs and the
    /// BCM5762's; for the BCM5761 its DSP's jitter fix, and for it and
    /// the BCM57780 Ethernet@WireSpeed (which links at a lower speed
    /// over a cable that cannot carry a gigabit) and, on Dell's boards,
    /// the link LED.
    pub fn reset(&mut self) {
        self.write(BMCR, BMCR_RESET);
        for _ in 0..RESET_LOOKS {
            if self.read(BMCR) & BMCR_RESET == 0 {
                break;
            }
            (self.delay)(1_000);
        }
        if !self.chip.phy_fixups() {
            return;
        }
        if self.chip.jitter_bug() {
            self.write(AUXCTL, 0x0c00);
            self.write(DSP_ADDR, 0x000a);
            self.write(DSP_RW_PORT, 0x010b);
            self.write(AUXCTL, 0x0400);
        }
        // Ethernet@WireSpeed, in the misc shadow register of AUXCTL.
        self.write(AUXCTL, 0x7007);
        let misc = self.read(AUXCTL);
        self.write(AUXCTL, misc | 1 << 15 | 1 << 4);
        if self.chip.dell {
            let control = self.read(EXTCTL);
            self.write(EXTCTL, control & !EXTCTL_3_LED);
        }
    }

    /// Resets the PHY and has it negotiate every speed it has, without
    /// pause frames, as FreeBSD's `brgphy_mii_phy_auto()` does.
    pub fn negotiate(&mut self) {
        self.reset();
        let abilities = (self.read(BMSR) & BMSR_MEDIA) >> BMSR_MEDIA_TO_ANAR;
        let abilities = if abilities == 0 { ANAR_ALL } else { abilities };
        self.write(ANAR, abilities | ANAR_CSMA);
        self.write(GTCR, GTCR_ADV_1000);
        let _ = self.read(GTCR);
        self.write(BMCR, BMCR_AUTOEN | BMCR_STARTNEG);
        self.write(IMR, IMR_LINK);
    }

    /// The link the PHY resolved: down while it still negotiates, as
    /// FreeBSD's `brgphy_status()` says.
    pub fn link(&mut self) -> Link {
        let _ = self.read(BMSR);
        let status = self.read(BMSR);
        let control = self.read(BMCR);
        if control & BMCR_AUTOEN != 0 && status & BMSR_ACOMP == 0 {
            return Link::default();
        }
        let aux = self.read(AUXSTS);
        if aux & AUXSTS_LINK == 0 {
            return Link::default();
        }
        let (mbps, full_duplex) = match aux & AUXSTS_RESOLVED {
            0x0700 => (1000, true),
            0x0600 => (1000, false),
            0x0500 => (100, true),
            // 100BASE-T4 is half duplex.
            0x0400 | 0x0300 => (100, false),
            0x0200 => (10, true),
            0x0100 => (10, false),
            _ => return Link::default(),
        };
        Link { up: true, mbps, full_duplex }
    }
}
