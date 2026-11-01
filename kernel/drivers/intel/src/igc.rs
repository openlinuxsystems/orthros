// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The igc family: Intel's I225 and I226 controllers, at up to 2.5 Gb/s,
//! on many boards and small machines since 2019, and their I220-V and
//! I221-V.
//!
//! Their registers are igb's ([`igb`](crate::igb)), queue 0 at the same
//! addresses, with the same advanced descriptors and the same semaphore
//! for the PHY the firmware shares, but for a few: the interrupt
//! registers moved (ICR to 0x1500), CTRL resets the device with another
//! bit, the internal PHY is at address 0 on MDIO, the firmware says
//! nothing of its ports, and STATUS tells 2.5 Gb/s from 1 Gb/s with one
//! more bit. FreeBSD's igc driver (BSD-2-Clause, with Intel's shared code
//! under BSD-3-Clause) was the reference for these.

use radix_hw::{Dma, Mmio};
use radix_nic::{Error, Link};

use crate::igb::{self, Family};
use crate::shared::{self, reg};
use crate::{Card, Interrupt, Ops};

/// The interrupt registers, where igc has them.
const ICR: usize = 0x1500;
const IMS: usize = 0x1508;
const IMC: usize = 0x150c;
/// CTRL's bit that resets the device.
const DEV_RST: u32 = 1 << 29;
/// STATUS's bit that says a 1 Gb/s link is 2.5 Gb/s.
const SPEED_2500: u32 = 1 << 22;

/// Which controller a card is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mac {
    /// The I225, and the I220-V.
    I225,
    /// The I226, and the I221-V.
    I226,
}

/// The cards of the family, by device ID, but for those whose NVM is
/// blank, which have no address of their own.
pub fn identify(device: u16) -> Option<(&'static str, Mac)> {
    Some(match device {
        0x15f2 => ("I225-LM", Mac::I225),
        0x15f3 => ("I225-V", Mac::I225),
        0x15f8 => ("I225-I", Mac::I225),
        0x0d9f => ("I225-IT", Mac::I225),
        0x3100 | 0x3101 => ("I225-K", Mac::I225),
        0x5502 => ("I225-LMvP", Mac::I225),
        0x15f7 => ("I220-V", Mac::I225),
        0x125b => ("I226-LM", Mac::I226),
        0x125c => ("I226-V", Mac::I226),
        0x125d => ("I226-IT", Mac::I226),
        0x3102 => ("I226-K", Mac::I226),
        0x5503 => ("I226-LMvP", Mac::I226),
        0x125e => ("I221-V", Mac::I226),
        _ => return None,
    })
}

static IGC: Family = Family {
    icr: ICR,
    ims: IMS,
    imc: IMC,
    reset: DEV_RST,
    phy: 0,
    firmware_ports: false,
    ops: &OPS,
};

static OPS: Ops = Ops { icr: ICR, link, halt };

/// Resets the card, a `model`, and sets it running: see [`crate::start`].
pub fn start(
    model: &'static str,
    regs: Mmio,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
) -> Result<Card, Error> {
    igb::start_family(&IGC, model, true, regs, interrupt, fallback_mac, alloc, delay)
}

fn halt(regs: &Mmio) {
    igb::halt_family(regs, &IGC);
}

/// The link, as STATUS says, with its 2.5 Gb/s.
fn link(regs: &Mmio) -> Link {
    let mut link = shared::link(regs);
    if link.mbps == 1000 && regs.read32(reg::STATUS) & SPEED_2500 != 0 {
        link.mbps = 2500;
    }
    link
}
