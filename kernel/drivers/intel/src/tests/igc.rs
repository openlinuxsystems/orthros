// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The igc driver against the simulated card of [`super::igb`], made an
//! I225: its interrupt registers at 0x1500, its reset bit 29, its PHY at
//! address 0, and a 2.5 Gb/s link. The simulated card panics at an
//! access to the 82576's interrupt registers.

use std::sync::Arc;

use radix_hw::Mmio;
use radix_nic::{Error, Nic};

use super::igb::{
    CTRL_EXT, Card, DRV_LOAD, IVAR0, QUEUE_ENABLE, RXDCTL, SIZE, SW_FW_SYNC, TXDCTL, Variant, frame,
};
use super::{alloc, no_delay};
use crate::Card as Running;
use crate::Interrupt;
use crate::igc::{self, Mac};

const I225: Variant =
    Variant { icr: 0x1500, ims: 0x1508, imc: 0x150c, reset: 1 << 29, phy: 0, fast: true };

fn start(card: &Arc<Card>, interrupt: Interrupt) -> Result<Running, Error> {
    let regs = Mmio::fake(card.clone(), SIZE);
    igc::start("I225-V", regs, interrupt, [0x02, 0, 0, 0, 0, 1], &mut alloc, &mut no_delay)
}

#[test]
fn an_i225_starts_at_2500_mbps() {
    let card = Card::of(I225);
    let igc = start(&card, Interrupt::Msi).unwrap();
    assert_eq!(igc.info().model, "I225-V");
    assert_ne!(card.reg(RXDCTL) & QUEUE_ENABLE, 0);
    assert_ne!(card.reg(TXDCTL) & QUEUE_ENABLE, 0);
    assert_ne!(card.reg(CTRL_EXT) & DRV_LOAD, 0, "the firmware told");
    assert_eq!(card.reg(IVAR0), 0x8080);
    assert_ne!(card.reg(I225.ims) & (1 << 7), 0, "frames received");
    assert_eq!(card.state.lock().unwrap().negotiations, 1, "through the PHY at address 0");
    assert_eq!(card.reg(SW_FW_SYNC), 0, "the PHY given back");
    let link = igc.link();
    assert!(link.up && link.full_duplex);
    assert_eq!(link.mbps, 2500);
    assert_eq!(card.state.lock().unwrap().resets, 2, "reset with its own bit");
}

#[test]
fn frames_go_both_ways_and_interrupts_are_acknowledged_where_igc_has_them() {
    let card = Card::of(I225);
    let mut igc = start(&card, Interrupt::Msi).unwrap();
    let mut buffer = [0u8; 2048];
    for n in 0..600 {
        assert!(igc.send(&frame(n)));
        igc.flush();
        assert_eq!(card.sent(), [frame(n)]);
        assert!(card.deliver(&frame(n), 0));
        let len = igc.receive(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], &frame(n)[..]);
        assert_eq!(igc.receive(&mut buffer), None);
        assert_eq!(card.state.lock().unwrap().icr, 0, "acknowledged");
    }
    assert_eq!(igc.dropped(), 0);
}

#[test]
fn stopping_resets_it_with_its_own_bit() {
    let card = Card::of(I225);
    let mut igc = start(&card, Interrupt::Msi).unwrap();
    igc.stop();
    igc.stop();
    let state = card.state.lock().unwrap();
    assert_eq!(state.resets, 3);
    assert_eq!(state.regs[I225.ims / 4], 0, "its interrupts off");
    assert_eq!(state.regs[CTRL_EXT / 4] & DRV_LOAD, 0);
}

#[test]
fn identifies_the_family() {
    assert_eq!(igc::identify(0x15f3), Some(("I225-V", Mac::I225)));
    assert_eq!(igc::identify(0x125c), Some(("I226-V", Mac::I226)));
    assert_eq!(igc::identify(0x125e), Some(("I221-V", Mac::I226)));
    // Blank NVMs, and an igb.
    assert_eq!(igc::identify(0x15fd), None);
    assert_eq!(igc::identify(0x125f), None);
    assert_eq!(igc::identify(0x1533), None);
}
