// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! How the driver reaches what is behind the chip's registers: the
//! MAC's and the PHY's OCP registers (Realtek's on-chip bus), the
//! standard PHY registers among the PHY's, the PCI Express PHY (EPHY),
//! and the configuration space through the CSI registers. Each access
//! waits as FreeBSD's do: the PHY's, the EPHY's and the CSI's until the
//! chip says it is done, the MAC's not at all.

use radix_hw::Mmio;

use crate::Kind;
use crate::reg::{self, csiar, ephyar, ocp};

/// How many microseconds an access is waited for, one at a time.
const ACCESS_US: u32 = 20_000;
/// What the EPHY and CSI accesses wait after.
const SETTLE_US: u32 = 20;

/// The chip of kind `kind` behind `regs`, with a way to wait.
pub struct Ocp<'a> {
    pub regs: &'a Mmio,
    pub kind: Kind,
    delay: &'a mut dyn FnMut(u32),
}

impl<'a> Ocp<'a> {
    pub fn new(regs: &'a Mmio, kind: Kind, delay: &'a mut dyn FnMut(u32)) -> Ocp<'a> {
        Ocp { regs, kind, delay }
    }

    /// Waits `us` microseconds.
    pub fn delay(&mut self, us: u32) {
        (self.delay)(us);
    }

    /// Waits a microsecond at a time, up to `us`, until `done` says so,
    /// with the register `at` read each time: the last value read.
    fn wait32(&mut self, at: usize, us: u32, done: impl Fn(u32) -> bool) -> u32 {
        let mut value = 0;
        for _ in 0..us {
            (self.delay)(1);
            value = self.regs.read32(at);
            if done(value) {
                break;
            }
        }
        value
    }

    pub fn set8(&self, at: usize, bits: u8) {
        self.regs.write8(at, self.regs.read8(at) | bits);
    }

    pub fn clear8(&self, at: usize, bits: u8) {
        self.regs.write8(at, self.regs.read8(at) & !bits);
    }

    pub fn set16(&self, at: usize, bits: u16) {
        self.regs.write16(at, self.regs.read16(at) | bits);
    }

    pub fn clear16(&self, at: usize, bits: u16) {
        self.regs.write16(at, self.regs.read16(at) & !bits);
    }

    pub fn clear32(&self, at: usize, bits: u32) {
        self.regs.write32(at, self.regs.read32(at) & !bits);
    }

    pub fn mac_read(&mut self, reg: u16) -> u16 {
        self.regs.write32(reg::MACOCP, u32::from(reg >> 1) << ocp::ADDR_SHIFT);
        (self.regs.read32(reg::MACOCP) & ocp::DATA_MASK) as u16
    }

    pub fn mac_write(&mut self, reg: u16, value: u16) {
        let command = (u32::from(reg >> 1) << ocp::ADDR_SHIFT) + u32::from(value);
        self.regs.write32(reg::MACOCP, command | ocp::BUSY);
    }

    pub fn mac_set(&mut self, reg: u16, bits: u16) {
        let value = self.mac_read(reg);
        self.mac_write(reg, value | bits);
    }

    pub fn mac_clear(&mut self, reg: u16, bits: u16) {
        let value = self.mac_read(reg);
        self.mac_write(reg, value & !bits);
    }

    /// Clears `clear` in MAC register `reg` and sets `set`.
    pub fn mac_modify(&mut self, reg: u16, clear: u16, set: u16) {
        let value = self.mac_read(reg) & !clear;
        self.mac_write(reg, value | set);
    }

    /// Reads PHY register `reg`: the chip sets the busy bit once the
    /// data is there.
    pub fn phy_read(&mut self, reg: u16) -> u16 {
        self.regs.write32(reg::PHYOCP, u32::from(reg >> 1) << ocp::ADDR_SHIFT);
        let value = self.wait32(reg::PHYOCP, ACCESS_US, |value| value & ocp::BUSY != 0);
        (value & ocp::DATA_MASK) as u16
    }

    /// Writes PHY register `reg`: the chip clears the busy bit once it
    /// is written.
    pub fn phy_write(&mut self, reg: u16, value: u16) {
        let command = u32::from(reg >> 1) << ocp::ADDR_SHIFT | ocp::BUSY | u32::from(value);
        self.regs.write32(reg::PHYOCP, command);
        let _ = self.wait32(reg::PHYOCP, ACCESS_US, |value| value & ocp::BUSY == 0);
    }

    pub fn phy_set(&mut self, reg: u16, bits: u16) {
        let value = self.phy_read(reg);
        self.phy_write(reg, value | bits);
    }

    pub fn phy_clear(&mut self, reg: u16, bits: u16) {
        let value = self.phy_read(reg);
        self.phy_write(reg, value & !bits);
    }

    /// Clears `clear` in PHY register `reg` and sets `set`.
    pub fn phy_modify(&mut self, reg: u16, clear: u16, set: u16) {
        let value = self.phy_read(reg) & !clear;
        self.phy_write(reg, value | set);
    }

    /// Where standard PHY register `reg` is among the PHY's OCP
    /// registers: eight to a block of 16, from [`reg::PHYBASE`].
    pub fn mii(reg: u16) -> u16 {
        ((reg::PHYBASE + reg / 8) << 4) + ((reg % 8) << 1)
    }

    pub fn mii_read(&mut self, reg: u16) -> u16 {
        self.phy_read(Ocp::mii(reg))
    }

    pub fn mii_write(&mut self, reg: u16, value: u16) {
        self.phy_write(Ocp::mii(reg), value);
    }

    pub fn ephy_read(&mut self, reg: u16) -> u16 {
        let command = (u32::from(reg) & ephyar::ADDR_MASK) << ephyar::ADDR_SHIFT;
        self.regs.write32(reg::EPHYAR, command);
        let value = self.wait32(reg::EPHYAR, ACCESS_US, |value| value & ephyar::BUSY != 0);
        (self.delay)(SETTLE_US);
        (value & ephyar::DATA_MASK) as u16
    }

    pub fn ephy_write(&mut self, reg: u16, value: u16) {
        let command = (u32::from(reg) & ephyar::ADDR_MASK) << ephyar::ADDR_SHIFT
            | ephyar::BUSY
            | (u32::from(value) & ephyar::DATA_MASK);
        self.regs.write32(reg::EPHYAR, command);
        let _ = self.wait32(reg::EPHYAR, ACCESS_US, |value| value & ephyar::BUSY == 0);
        (self.delay)(SETTLE_US);
    }

    pub fn ephy_set(&mut self, reg: u16, bits: u16) {
        let value = self.ephy_read(reg);
        self.ephy_write(reg, value | bits);
    }

    pub fn ephy_clear(&mut self, reg: u16, bits: u16) {
        let value = self.ephy_read(reg);
        self.ephy_write(reg, value & !bits);
    }

    /// Clears `clear` in EPHY register `reg` and sets `set`.
    pub fn ephy_modify(&mut self, reg: u16, clear: u16, set: u16) {
        let value = self.ephy_read(reg) & !clear;
        self.ephy_write(reg, value | set);
    }

    /// Reads the configuration space's word at `reg`, all four bytes.
    pub fn csi_read(&mut self, reg: u32) -> u32 {
        let command = (reg & csiar::ADDR_MASK) | csiar::BYTE_EN << csiar::BYTE_EN_SHIFT;
        self.regs.write32(reg::CSIAR, command);
        let _ = self.wait32(reg::CSIAR, ACCESS_US, |value| value & csiar::BUSY != 0);
        (self.delay)(SETTLE_US);
        self.regs.read32(reg::CSIDR)
    }

    pub fn csi_write(&mut self, reg: u32, value: u32) {
        self.regs.write32(reg::CSIDR, value);
        let command =
            (reg & csiar::ADDR_MASK) | csiar::BYTE_EN << csiar::BYTE_EN_SHIFT | csiar::BUSY;
        self.regs.write32(reg::CSIAR, command);
        let _ = self.wait32(reg::CSIAR, ACCESS_US, |value| value & csiar::BUSY == 0);
        (self.delay)(SETTLE_US);
    }
}
