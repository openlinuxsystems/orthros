// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chips one PCI device ID stands for, told apart by the revision in
//! their transmit configuration register, and what each needs: the
//! revisions and their quirks are FreeBSD's `re(4)`'s.

/// What a revision needs, as flags.
pub mod needs {
    /// Its PHY woken at start: page 0 chosen, and register 0x0e cleared.
    pub const PHY_WAKE: u32 = 1 << 0;
    /// Its PHY taken out of power management first (PMCH's bit 7).
    pub const PHY_WAKE_PM: u32 = 1 << 1;
    /// The C+ command register with the MAC's statistics off, and only
    /// its bit 0 set where the older chips have their receive and
    /// transmit enables.
    pub const MAC_STAT: u32 = 1 << 2;
    /// 10 and 100 Mb/s only: frames up to 1500 bytes, so that the card
    /// takes none longer than its slot.
    pub const FAST: u32 = 1 << 3;
    /// Stopped with the stop request command.
    pub const CMD_STOP: u32 = 1 << 4;
    /// Its transmit queue waited on after the stop request.
    pub const CMD_STOP_WAIT_TXQ: u32 = 1 << 5;
    /// Stopped once its transmit poll is done.
    pub const WAIT_TX_POLL: u32 = 1 << 6;
    /// Taken out of a deep sleep through its GPIO register.
    pub const MAC_SLEEP: u32 = 1 << 7;
    /// Jumbo frames the RTL8168C's way, which must be turned off, and
    /// frames no longer than its buffers.
    pub const JUMBO_V2: u32 = 1 << 8;
    /// The RTL8168G and later: the RXDV gate, opened as the card starts
    /// and closed as it stops, and the receiver and transmitter turned on
    /// after they are set up, not before.
    pub const G_PLUS: u32 = 1 << 9;
    /// The register at 0x82 written after a reset.
    pub const MAC_RESET: u32 = 1 << 10;
    /// The receive configuration's early-off bits.
    pub const EARLY_OFF: u32 = 1 << 11;
}

use needs::*;

/// The transmit configuration register's bits that hold the revision.
const HWREV: u32 = 0x7cc0_0000;

/// A revision of the chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chip {
    /// What Realtek calls it: "RTL8168H/8111H".
    pub name: &'static str,
    /// Its revision bits.
    pub rev: u32,
    /// What it needs ([`needs`]).
    pub needs: u32,
}

impl Chip {
    /// Whether it needs `what`, one or more of [`needs`].
    pub fn needs(&self, what: u32) -> bool {
        self.needs & what != 0
    }
}

pub(crate) const RTL8169S: u32 = 0x0080_0000;
pub(crate) const RTL8169_8110SC: u32 = 0x1800_0000;
pub(crate) const RTL8169_8110SCE: u32 = 0x9800_0000;
pub(crate) const RTL8401E: u32 = 0x2400_0000;
pub(crate) const RTL8168C: u32 = 0x3c00_0000;
pub(crate) const RTL8168DP: u32 = 0x2880_0000;
pub(crate) const RTL8168E: u32 = 0x2c00_0000;
pub(crate) const RTL8168E_VL: u32 = 0x2c80_0000;
const RTL8168GU: u32 = 0x5080_0000;
const RTL8168H: u32 = 0x5400_0000;

const RTL8102E: u32 = PHY_WAKE | MAC_STAT | FAST | CMD_STOP;
const RTL8105E: u32 = PHY_WAKE | PHY_WAKE_PM | MAC_STAT | FAST | CMD_STOP;
const RTL8168CP: u32 = PHY_WAKE | MAC_STAT | CMD_STOP | JUMBO_V2;
const RTL8411: u32 = PHY_WAKE | MAC_STAT | CMD_STOP | JUMBO_V2 | CMD_STOP_WAIT_TXQ;
const RTL8168G: u32 = RTL8411 | G_PLUS;
const RTL8168H_FAMILY: u32 = PHY_WAKE | MAC_STAT | CMD_STOP | CMD_STOP_WAIT_TXQ | G_PLUS;

/// Every revision of the gigabit chips' register layout, from the
/// RTL8169 to the RTL8168H: the RTL8139 family, the RTL8139C+ among
/// it, is another driver's.
const CHIPS: &[(u32, &str, u32)] = &[
    (0x0000_0000, "RTL8169", MAC_RESET),
    (RTL8169S, "RTL8169S", MAC_RESET),
    (0x0400_0000, "RTL8110S", MAC_RESET),
    (0x1000_0000, "RTL8169SB/8110SB", PHY_WAKE | MAC_RESET),
    (RTL8169_8110SC, "RTL8169SC/8110SC", PHY_WAKE | MAC_RESET),
    (0x7cc0_0000, "RTL8169SBL/8110SBL", PHY_WAKE | MAC_RESET),
    (RTL8169_8110SCE, "RTL8169SC/8110SC", PHY_WAKE | MAC_RESET),
    (0x3000_0000, "RTL8168B/8111B", PHY_WAKE | MAC_STAT),
    (0x3800_0000, "RTL8168B/8111B", PHY_WAKE | MAC_STAT),
    (0x3840_0000, "RTL8168B/8111B", PHY_WAKE | MAC_STAT),
    (0x3080_0000, "RTL8100E", PHY_WAKE | FAST),
    (0x3400_0000, "RTL8101E", PHY_WAKE | FAST),
    (0x3480_0000, "RTL8102E", RTL8102E),
    (0x2480_0000, "RTL8102EL", RTL8102E),
    (0x24c0_0000, "RTL8102EL", RTL8102E),
    (0x34c0_0000, "RTL8103E", RTL8102E | MAC_SLEEP),
    (RTL8401E, "RTL8401E", RTL8105E),
    (0x4080_0000, "RTL8105E", RTL8105E),
    (0x40c0_0000, "RTL8105E", RTL8105E),
    (0x4480_0000, "RTL8106E", RTL8105E),
    (0x4400_0000, "RTL8402", RTL8105E | CMD_STOP_WAIT_TXQ),
    (RTL8168C, "RTL8168C/8111C", RTL8168CP),
    (0x3c40_0000, "RTL8168C/8111C", RTL8168CP | MAC_SLEEP),
    (0x3c80_0000, "RTL8168CP/8111CP", RTL8168CP),
    (0x2800_0000, "RTL8168D/8111D", RTL8168CP | PHY_WAKE_PM),
    (RTL8168DP, "RTL8168DP/8111DP", PHY_WAKE | MAC_STAT | JUMBO_V2 | WAIT_TX_POLL),
    (RTL8168E, "RTL8168E/8111E", RTL8168CP | PHY_WAKE_PM),
    (RTL8168E_VL, "RTL8168E-VL/8111E-VL", RTL8411 | EARLY_OFF),
    (0x4800_0000, "RTL8168F/8111F", RTL8411 | EARLY_OFF),
    (0x4880_0000, "RTL8411", RTL8411),
    (0x5000_0000, "RTL8168EP/8111EP", RTL8168G),
    (0x5480_0000, "RTL8168FP/8111FP", RTL8168G),
    (0x4c00_0000, "RTL8168G/8111G", RTL8168G),
    (0x5c80_0000, "RTL8411B", RTL8168G),
    (RTL8168GU, "RTL8168GU/8111GU", RTL8168H_FAMILY | JUMBO_V2),
    (RTL8168H, "RTL8168H/8111H", RTL8168H_FAMILY | JUMBO_V2),
];

/// The chip whose transmit configuration register reads `txcfg`, on a
/// card of PCI device ID `device`: `None` for a revision not known here.
pub fn identify(txcfg: u32, device: u16) -> Option<Chip> {
    // The RTL8169s keep their revision's top bit, which the others use
    // for something else; the RTL8168C has a MAC revision beside it.
    let (rev, mac_rev) = match txcfg & 0x7000_0000 {
        0 | 0x1000_0000 => (txcfg & (HWREV | 0x8000_0000), 0),
        _ => (txcfg & HWREV, txcfg & 0x0070_0000),
    };
    let &(rev, name, needs) = CHIPS.iter().find(|(known, ..)| *known == rev)?;
    let mut chip = Chip { name, rev, needs };
    if rev == RTL8168C && mac_rev == 0x0020_0000 {
        chip.needs |= MAC_SLEEP;
    }
    // The RTL810x family's device ID on the RTL8168GU's and RTL8168H's
    // revisions: the RTL8106EUS and RTL8107E, at 10 and 100 Mb/s.
    if device == crate::DEVICE_8101E && matches!(rev, RTL8168GU | RTL8168H) {
        chip.needs = (chip.needs & !JUMBO_V2) | FAST;
        chip.name = if rev == RTL8168GU { "RTL8106EUS" } else { "RTL8107E" };
    }
    Some(chip)
}
