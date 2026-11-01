// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The igb driver against a simulated 82576: advanced descriptors, queues
//! that must be turned on, a PHY shared with firmware through SW_FW_SYNC,
//! and a link that is down after a reset until it is negotiated again,
//! as QEMU's igb has it.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Mmio, Registers};
use radix_nic::{Error, Nic};

use super::{alloc, no_delay, read_mem, u64_at, write_mem};
use crate::Card as Running;
use crate::Interrupt;
use crate::igb::{self, Mac};

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x63];
const FALLBACK: [u8; 6] = [0x02, 0, 0, 0, 0, 1];

const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EECD: usize = 0x0010;
const CTRL_EXT: usize = 0x0018;
const MDIC: usize = 0x0020;
const ICR: usize = 0x00c0;
const IMS: usize = 0x00d0;
const IMC: usize = 0x00d8;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const EEMNGCTL: usize = 0x1010;
const GPIE: usize = 0x1514;
const EITR0: usize = 0x1680;
const IVAR0: usize = 0x1700;
const RDBAL: usize = 0x2800;
const RDLEN: usize = 0x2808;
const SRRCTL: usize = 0x280c;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const RXDCTL: usize = 0x2828;
const TDBAL: usize = 0x3800;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const TXDCTL: usize = 0x3828;
const RAL0: usize = 0x5400;
const RAH0: usize = 0x5404;
const SWSM: usize = 0x5b50;
const SW_FW_SYNC: usize = 0x5b5c;
const SIZE: usize = 0x20000;

const QUEUE_ENABLE: u32 = 1 << 25;
const DRV_LOAD: u32 = 1 << 28;

struct State {
    regs: Vec<u32>,
    icr: u32,
    phy_control: u16,
    link_up: bool,
    /// Whether its firmware holds the PHY, and never lets go.
    firmware_has_phy: bool,
    /// Whether its queues stay off when they are turned on.
    queues_stuck: bool,
    /// Its link mode, as its NVM sets CTRL_EXT at a reset.
    link_mode: u32,
    sent: Vec<Vec<u8>>,
    resets: u32,
    /// How many times its link was negotiated again.
    negotiations: u32,
}

struct Card {
    state: Mutex<State>,
}

impl Card {
    fn new() -> Arc<Card> {
        let state = State {
            regs: std::vec![0; SIZE / 4],
            icr: 0,
            phy_control: 0x1140,
            link_up: false,
            firmware_has_phy: false,
            queues_stuck: false,
            link_mode: 0,
            sent: Vec::new(),
            resets: 0,
            negotiations: 0,
        };
        let card = Arc::new(Card { state: Mutex::new(state) });
        card.reset();
        card
    }

    /// A reset, as QEMU's: every register as it starts, the address
    /// loaded, the link down until negotiated.
    fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.regs.iter_mut().for_each(|r| *r = 0);
        state.resets += 1;
        state.link_up = false;
        state.regs[RAL0 / 4] = u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]);
        state.regs[RAH0 / 4] = u32::from(u16::from_le_bytes([MAC[4], MAC[5]])) | (1 << 31);
        state.regs[CTRL_EXT / 4] = state.link_mode;
        state.regs[EEMNGCTL / 4] = 1 << 18;
        if state.firmware_has_phy {
            state.regs[SW_FW_SYNC / 4] = 0x02 << 16;
        }
    }

    fn reg(&self, offset: usize) -> u32 {
        self.state.lock().unwrap().regs[offset / 4]
    }

    fn set(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
    }

    /// Puts `frame` on the wire to the card, with `errors` in its
    /// descriptor's status word: whether the card took it.
    fn deliver(&self, frame: &[u8], errors: u32) -> bool {
        let mut state = self.state.lock().unwrap();
        let count = state.regs[RDLEN / 4] as usize / 16;
        let head = state.regs[RDH / 4] as usize;
        let on = state.regs[RXDCTL / 4] & QUEUE_ENABLE != 0 && state.regs[RCTL / 4] & 2 != 0;
        if !on || !state.link_up || count == 0 || head == state.regs[RDT / 4] as usize {
            return false;
        }
        assert_eq!(state.regs[SRRCTL / 4] >> 25 & 7, 1, "advanced, one buffer");
        let base = u64::from(state.regs[RDBAL / 4]) | (u64::from(state.regs[RDBAL / 4 + 1]) << 32);
        let at = base + (head * 16) as u64;
        assert_eq!(u64_at(at + 8), 0, "no header buffer");
        write_mem(u64_at(at), frame);
        // The write-back: packet type and RSS (none), then the status and
        // errors, the length and the VLAN tag.
        write_mem(at, &[0; 8]);
        write_mem(at + 8, &(0x03 | errors).to_le_bytes());
        write_mem(at + 12, &(frame.len() as u16).to_le_bytes());
        write_mem(at + 14, &[0, 0]);
        state.regs[RDH / 4] = ((head + 1) % count) as u32;
        state.icr |= 1 << 7;
        true
    }

    fn transmit(state: &mut State) {
        if state.regs[TXDCTL / 4] & QUEUE_ENABLE == 0 || state.regs[TCTL / 4] & 2 == 0 {
            return;
        }
        let count = state.regs[TDLEN / 4] as usize / 16;
        let base = u64::from(state.regs[TDBAL / 4]) | (u64::from(state.regs[TDBAL / 4 + 1]) << 32);
        let mut head = state.regs[TDH / 4] as usize;
        let tail = state.regs[TDT / 4] as usize;
        while head != tail {
            let at = base + (head * 16) as u64;
            let word = u64_at(at + 8);
            let len = (word & 0xffff) as usize;
            assert_eq!((word >> 20) & 0xf, 3, "a data descriptor");
            assert_eq!((word >> 24) & 0xff, 0x2b, "EOP, IFCS, RS and DEXT");
            assert_eq!((word >> 46) as usize, len, "the payload's length");
            state.sent.push(read_mem(u64_at(at), len));
            // The write-back: DD in the status.
            write_mem(at + 12, &[0x01]);
            head = (head + 1) % count;
        }
        state.regs[TDH / 4] = head as u32;
        state.icr |= 1;
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.state.lock().unwrap().sent)
    }
}

impl Registers for Card {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4, "32-bit registers");
        let mut state = self.state.lock().unwrap();
        let value = match offset {
            STATUS => (2 << 6) | if state.link_up { 0x3 } else { 0 },
            EECD => 1 << 9,
            ICR => core::mem::take(&mut state.icr),
            // Reading takes software's semaphore.
            SWSM => {
                let value = state.regs[SWSM / 4];
                state.regs[SWSM / 4] |= 1;
                value
            }
            _ => state.regs[offset / 4],
        };
        u64::from(value)
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4, "32-bit registers");
        let value = value as u32;
        if offset == CTRL && value & (1 << 26) != 0 {
            self.reset();
            self.state.lock().unwrap().regs[CTRL / 4] = value & !(1 << 26);
            return;
        }
        let mut state = self.state.lock().unwrap();
        match offset {
            MDIC => {
                let register = (value >> 16) & 0x1f;
                assert_eq!((value >> 21) & 0x1f, 1, "the internal PHY");
                let sync = state.regs[SW_FW_SYNC / 4];
                assert!(
                    sync & 0x02 != 0 && sync & (0x02 << 16) == 0,
                    "MDIO with the PHY's semaphore"
                );
                let data = match (value >> 26) & 3 {
                    2 if register == 0 => state.phy_control,
                    1 if register == 0 => {
                        state.phy_control = value as u16 & !(1 << 9);
                        if value & (1 << 9) != 0 && value & (1 << 12) != 0 {
                            state.negotiations += 1;
                            state.link_up = true;
                        }
                        value as u16
                    }
                    _ => 0,
                };
                state.regs[MDIC / 4] = (value & !0xffff) | u32::from(data) | (1 << 28);
            }
            SW_FW_SYNC => {
                assert_ne!(state.regs[SWSM / 4] & 3, 0, "SW_FW_SYNC with the semaphores");
                let firmware = state.regs[SW_FW_SYNC / 4] & 0xffff_0000;
                state.regs[SW_FW_SYNC / 4] = (value & 0xffff) | firmware;
            }
            RXDCTL | TXDCTL if state.queues_stuck => {
                state.regs[offset / 4] = value & !QUEUE_ENABLE;
            }
            IMC => state.regs[IMS / 4] &= !value,
            IMS => state.regs[IMS / 4] |= value,
            TDT => {
                state.regs[TDT / 4] = value;
                Card::transmit(&mut state);
            }
            _ => state.regs[offset / 4] = value,
        }
    }
}

fn start(card: &Arc<Card>, interrupt: Interrupt) -> Result<Running, Error> {
    let regs = Mmio::fake(card.clone(), SIZE);
    igb::start("test", Mac::I82576, regs, interrupt, FALLBACK, &mut alloc, &mut no_delay)
}

fn frame(n: usize) -> Vec<u8> {
    let mut frame = std::vec![0u8; 60 + n % 1400];
    frame[..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(&MAC);
    frame[14..22].copy_from_slice(&(n as u64).to_le_bytes());
    frame
}

#[test]
fn an_82576_starts_with_its_queues_on_and_its_link_negotiated() {
    let card = Card::new();
    let igb = start(&card, Interrupt::Msi).unwrap();
    assert_eq!(igb.info().mac, MAC);
    assert_eq!((igb.info().receive_slots, igb.info().transmit_slots), (255, 255));
    assert_eq!(card.reg(SRRCTL), 0x0200_0402, "advanced, one 2 KiB buffer");
    assert_ne!(card.reg(RXDCTL) & QUEUE_ENABLE, 0);
    assert_ne!(card.reg(TXDCTL) & QUEUE_ENABLE, 0);
    assert_eq!(card.reg(RDT), 255);
    assert_ne!(card.reg(CTRL_EXT) & DRV_LOAD, 0, "the firmware told");
    assert_eq!((card.reg(GPIE), card.reg(IVAR0)), (0, 0x8080), "MSI, for queue 0");
    assert_eq!(card.reg(EITR0) & 0x7ffc, 200, "50 us apart");
    assert_ne!(card.reg(IMS) & (1 << 7), 0, "frames received");
    assert_eq!(card.state.lock().unwrap().negotiations, 1);
    assert!(igb.link().up);
    assert_eq!(card.reg(SW_FW_SYNC), 0, "the PHY given back");
    assert_eq!(card.reg(SWSM) & 3, 0, "the semaphores given back");
    assert_eq!(card.state.lock().unwrap().resets, 2, "the card's own and the driver's");
}

#[test]
fn frames_go_both_ways_in_advanced_descriptors() {
    let card = Card::new();
    let mut igb = start(&card, Interrupt::Polled).unwrap();
    assert_eq!(card.reg(IMS), 0, "polled: no interrupts");
    let mut buffer = [0u8; 2048];
    for round in 0..4 {
        for n in 0..200 {
            assert!(igb.send(&frame(round * 1000 + n)));
        }
        assert!(card.sent().is_empty(), "nothing goes before the doorbell");
        igb.flush();
        let sent = card.sent();
        assert_eq!(sent.len(), 200);
        for (n, got) in sent.iter().enumerate() {
            assert_eq!(*got, frame(round * 1000 + n));
        }
        for n in 0..200 {
            assert!(card.deliver(&frame(n), 0), "round {round}, frame {n}");
            let len = igb.receive(&mut buffer).unwrap();
            assert_eq!(&buffer[..len], &frame(n)[..]);
        }
        assert_eq!(igb.receive(&mut buffer), None);
        igb.flush();
    }
    assert_eq!(igb.dropped(), 0);
}

#[test]
fn damaged_frames_are_dropped() {
    let card = Card::new();
    let mut igb = start(&card, Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    // An RX data error, then a CRC error, then a good frame.
    assert!(card.deliver(&frame(1), 0x8000_0000));
    assert!(card.deliver(&frame(2), 0x0100_0000));
    assert!(card.deliver(&frame(3), 0));
    let len = igb.receive(&mut buffer).unwrap();
    assert_eq!(&buffer[..len], &frame(3)[..]);
    assert_eq!(igb.dropped(), 2);
}

#[test]
fn a_phy_the_firmware_holds_is_left_alone() {
    let card = Card::new();
    card.set(|state| state.firmware_has_phy = true);
    card.reset();
    let igb = start(&card, Interrupt::Polled).unwrap();
    assert_eq!(card.state.lock().unwrap().negotiations, 0);
    assert_eq!(card.reg(SW_FW_SYNC), 0x02 << 16, "the firmware's, untouched");
    assert_eq!(card.reg(SWSM) & 3, 0);
    assert!(!igb.link().up);
}

#[test]
fn links_other_than_copper_are_refused() {
    let card = Card::new();
    card.set(|state| state.link_mode = 3 << 22);
    assert!(matches!(start(&card, Interrupt::Polled), Err(Error::Unsupported(_))));
}

#[test]
fn queues_that_do_not_turn_on_are_a_timeout() {
    let card = Card::new();
    card.set(|state| state.queues_stuck = true);
    assert!(matches!(start(&card, Interrupt::Polled), Err(Error::Timeout(_))));
    assert_eq!(card.reg(CTRL_EXT) & DRV_LOAD, 0, "given back");
}

#[test]
fn stopping_gives_the_card_back_and_resets_it() {
    let card = Card::new();
    let mut igb = start(&card, Interrupt::Msi).unwrap();
    igb.stop();
    let state = card.state.lock().unwrap();
    assert_eq!(state.resets, 3);
    assert_eq!(state.regs[CTRL_EXT / 4] & DRV_LOAD, 0);
    assert_eq!(state.regs[IMS / 4], 0);
}

#[test]
fn identifies_the_family() {
    assert_eq!(igb::identify(0x10c9), Some(("82576", Mac::I82576)));
    assert_eq!(igb::identify(0x1521), Some(("I350", Mac::I350)));
    assert_eq!(igb::identify(0x1533), Some(("I210", Mac::I210)));
    assert_eq!(igb::identify(0x1539), Some(("I211", Mac::I211)));
    // Fiber and SerDes.
    assert_eq!(igb::identify(0x10e6), None);
    assert_eq!(igb::identify(0x1536), None);
}
