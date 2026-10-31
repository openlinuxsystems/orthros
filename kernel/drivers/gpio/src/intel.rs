// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Intel's GPIO controllers of the PCH's communities, as of Sunrise
//! Point-LP (ACPI `INT344B`), in the 100 Series PCH and the 6th to 8th
//! generation Core U and Y processors (Intel's *100 Series Chipset
//! Family PCH Datasheet*, volume 2, "GPIO"; Linux's `pinctrl-intel` as
//! the reference for what it is used for).
//!
//! The pads are numbered across the communities, which the `_CRS` gives
//! as register ranges in order, and grouped by 24 within each. A
//! community's registers, from its range:
//!
//! - PADBAR (0x0c), where its pads' configuration registers are: two
//!   32-bit registers a pad. The first, DW0, has the pad's mode (bits
//!   12:10, 0 for GPIO), its input buffer's disable (bit 9) and output
//!   buffer's (bit 8), its input's level (bit 1), the inversion of its
//!   input (RXINV, bit 23) and what of the input is an event (RXEVCFG,
//!   bits 26:25: 0 its level, 1 its edge).
//! - PAD_OWN (0x20): four bits a pad, 0 when the host owns it rather
//!   than the Management Engine.
//! - HOSTSW_OWN (0xd0): a bit a pad, set when the operating system's
//!   GPIO driver has its events rather than the ACPI firmware's GPEs.
//! - GPI_IS (0x100) and GPI_IE (0x120): a register a group, a bit a pad,
//!   for the events that are pending (cleared by writing them as 1) and
//!   those that interrupt.
//!
//! The firmware may lock a pad's configuration; a change that does not
//! take is reported as locked.

use radix_hw::Mmio;

use crate::{Error, Trigger, Unusable};

/// The pads in a group.
const GROUP: u16 = 24;
/// How many communities a controller may have.
const MAX_COMMUNITIES: usize = 4;

const PADBAR: usize = 0x0c;
const PAD_OWN: usize = 0x20;
const HOSTSW_OWN: usize = 0xd0;
const GPI_IS: usize = 0x100;
const GPI_IE: usize = 0x120;
/// The bytes of a pad's configuration registers.
const PAD_SIZE: usize = 8;

const RX_STATE: u32 = 1 << 1;
const TX_DISABLE: u32 = 1 << 8;
const RX_DISABLE: u32 = 1 << 9;
const MODE: u32 = 7 << 10;
const RX_INVERT: u32 = 1 << 23;
const RX_EVENT_SHIFT: u32 = 25;
const RX_EVENT: u32 = 3 << RX_EVENT_SHIFT;
const EDGE: u32 = 1;

/// A chip's communities.
pub struct Layout {
    /// The chip's name, for the log.
    pub name: &'static str,
    /// Each community's first pad and how many it has.
    pub communities: &'static [(u16, u16)],
}

/// Sunrise Point-LP's: GPP_A and GPP_B, then GPP_C to GPP_E, then GPP_F
/// and GPP_G's eight pads.
pub const SUNRISE_POINT_LP: Layout =
    Layout { name: "Intel Sunrise Point-LP's", communities: &[(0, 48), (48, 72), (120, 32)] };

/// A community: its registers and where its pads' are.
struct Community {
    registers: Mmio,
    first: u16,
    pads: u16,
    padbar: usize,
}

impl Community {
    fn groups(&self) -> u16 {
        self.pads.div_ceil(GROUP)
    }
}

/// An Intel GPIO controller.
pub struct Intel {
    layout: &'static Layout,
    communities: [Option<Community>; MAX_COMMUNITIES],
}

impl Intel {
    /// The controller of `layout` at `ranges`, if they look like its.
    pub(crate) fn new(layout: &'static Layout, ranges: &[Mmio]) -> Result<Intel, Unusable> {
        if ranges.len() < layout.communities.len() || layout.communities.len() > MAX_COMMUNITIES {
            return Err(Unusable::Registers);
        }
        let mut communities = [const { None }; MAX_COMMUNITIES];
        for ((slot, registers), &(first, pads)) in
            communities.iter_mut().zip(ranges).zip(layout.communities)
        {
            if registers.len() < GPI_IE + 4 * usize::from(pads.div_ceil(GROUP)) {
                return Err(Unusable::Registers);
            }
            let padbar = registers.read32(PADBAR);
            if padbar == u32::MAX {
                return Err(Unusable::Absent);
            }
            let padbar = padbar as usize;
            let end = padbar.checked_add(usize::from(pads) * PAD_SIZE);
            if padbar < GPI_IE
                || !padbar.is_multiple_of(4)
                || end.is_none_or(|end| end > registers.len())
            {
                return Err(Unusable::Layout);
            }
            *slot = Some(Community { registers: registers.clone(), first, pads, padbar });
        }
        Ok(Intel { layout, communities })
    }

    pub(crate) fn name(&self) -> &'static str {
        self.layout.name
    }

    pub(crate) fn pins(&self) -> u16 {
        self.communities().map(|community| community.first + community.pads).max().unwrap_or(0)
    }

    fn communities(&self) -> impl Iterator<Item = &Community> {
        self.communities.iter().flatten()
    }

    /// The community of `pin` and its place in it.
    fn find(&self, pin: u16) -> Result<(&Community, u16), Error> {
        self.communities()
            .find(|community| (community.first..community.first + community.pads).contains(&pin))
            .map(|community| (community, pin - community.first))
            .ok_or(Error::NoPin)
    }

    pub(crate) fn quiet(&self) {
        for community in self.communities() {
            for group in 0..usize::from(community.groups()) {
                community.registers.write32(GPI_IE + 4 * group, 0);
                community.registers.write32(GPI_IS + 4 * group, u32::MAX);
            }
        }
    }

    pub(crate) fn set_interrupt(&self, pin: u16, trigger: Trigger) -> Result<(), Error> {
        let (community, pad) = self.find(pin)?;
        let (group, bit) = (usize::from(pad / GROUP), pad % GROUP);
        let registers = &community.registers;
        let pad = usize::from(pad);
        let owner = registers.read32(PAD_OWN + pad / 8 * 4) >> (pad % 8 * 4) & 0xf;
        if owner != 0 || registers.read32(HOSTSW_OWN + 4 * group) & (1 << bit) == 0 {
            return Err(Error::Firmware);
        }
        let dw0 = community.padbar + pad * PAD_SIZE;
        let value = registers.read32(dw0);
        if value & MODE != 0 {
            return Err(Error::Function);
        }
        let (event, invert) = match trigger {
            Trigger::LevelHigh => (0, 0),
            Trigger::LevelLow => (0, RX_INVERT),
            Trigger::RisingEdge => (EDGE, 0),
            Trigger::FallingEdge => (EDGE, RX_INVERT),
            Trigger::BothEdges => return Err(Error::Trigger(trigger)),
        };
        self.set_masked(pin, true);
        let wanted = (value & !(RX_EVENT | RX_INVERT | RX_DISABLE))
            | event << RX_EVENT_SHIFT
            | invert
            | TX_DISABLE;
        if wanted != value {
            registers.write32(dw0, wanted);
            if registers.read32(dw0) & !RX_STATE != wanted & !RX_STATE {
                return Err(Error::Locked);
            }
        }
        Ok(())
    }

    pub(crate) fn set_masked(&self, pin: u16, masked: bool) {
        let Ok((community, pad)) = self.find(pin) else { return };
        let (group, bit) = (usize::from(pad / GROUP), 1 << (pad % GROUP));
        let registers = &community.registers;
        let enabled = registers.read32(GPI_IE + 4 * group);
        if masked {
            registers.write32(GPI_IE + 4 * group, enabled & !bit);
        } else {
            // A level that is still active sets the status again.
            registers.write32(GPI_IS + 4 * group, bit);
            registers.write32(GPI_IE + 4 * group, enabled | bit);
        }
    }

    pub(crate) fn pending(&self, found: &mut dyn FnMut(u16)) {
        for community in self.communities() {
            for group in 0..community.groups() {
                let at = 4 * usize::from(group);
                let enabled = community.registers.read32(GPI_IE + at);
                if enabled == 0 {
                    continue;
                }
                let pending = community.registers.read32(GPI_IS + at) & enabled;
                if pending == 0 {
                    continue;
                }
                community.registers.write32(GPI_IS + at, pending);
                for bit in (0..GROUP).filter(|bit| pending & (1 << bit) != 0) {
                    found(community.first + group * GROUP + bit);
                }
            }
        }
    }

    pub(crate) fn level(&self, pin: u16) -> Result<bool, Error> {
        let (community, pad) = self.find(pin)?;
        let dw0 = community.padbar + usize::from(pad) * PAD_SIZE;
        Ok(community.registers.read32(dw0) & RX_STATE != 0)
    }
}
