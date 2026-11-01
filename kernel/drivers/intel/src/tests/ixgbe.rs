// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The ixgbe driver against a simulated X540 or 82599: the extended
//! interrupt registers, queue 0 at 0x1000 and 0x6000, a receiver and a
//! transmitter that RXCTRL and DMATXCTL turn on, and the link in LINKS.
//! The X540's reset must be made with the PHY's semaphore held; the
//! 82599's link comes up once it is negotiated again.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Mmio, Registers};
use radix_nic::{Error, Nic};

use super::{alloc, no_delay, read_mem, u64_at, write_mem};
use crate::Card as Running;
use crate::Interrupt;
use crate::ixgbe::{self, Mac};

const MAC: [u8; 6] = [0xa0, 0x36, 0x9f, 0x01, 0x02, 0x03];

const CTRL: usize = 0x0_0000;
const STATUS: usize = 0x0_0008;
const CTRL_EXT: usize = 0x0_0018;
const EICR: usize = 0x0_0800;
const EITR0: usize = 0x0_0820;
const EIMS: usize = 0x0_0880;
const EIMC: usize = 0x0_0888;
const GPIE: usize = 0x0_0898;
const IVAR0: usize = 0x0_0900;
const RDBAL: usize = 0x0_1000;
const RDLEN: usize = 0x0_1008;
const RDH: usize = 0x0_1010;
const RDT: usize = 0x0_1018;
const RXDCTL: usize = 0x0_1028;
const SRRCTL: usize = 0x0_2100;
const RDRXCTL: usize = 0x0_2f00;
const RXCTRL: usize = 0x0_3000;
const RXPBSIZE0: usize = 0x0_3c00;
const HLREG0: usize = 0x0_4240;
const AUTOC: usize = 0x0_42a0;
const LINKS: usize = 0x0_42a4;
const AUTOC2: usize = 0x0_42a8;
const DMATXCTL: usize = 0x0_4a80;
const FCTRL: usize = 0x0_5080;
const RAL0: usize = 0x0_5400;
const RAH0: usize = 0x0_5404;
const TDBAL: usize = 0x0_6000;
const TDLEN: usize = 0x0_6008;
const TDH: usize = 0x0_6010;
const TDT: usize = 0x0_6018;
const TXDCTL: usize = 0x0_6028;
const SECRXCTRL: usize = 0x0_8d00;
const SECRXSTAT: usize = 0x0_8d04;
const MPSAR_LO0: usize = 0x0_a600;
const SWSM: usize = 0x1_0140;
const SW_FW_SYNC: usize = 0x1_0160;
const SIZE: usize = 0x2_0000;

const RST: u32 = 1 << 26;
const LNK_RST: u32 = 1 << 3;
const QUEUE_ENABLE: u32 = 1 << 25;
const DRV_LOAD: u32 = 1 << 28;
const REGSMP: u32 = 1 << 31;

struct State {
    regs: Vec<u32>,
    eicr: u32,
    mac: Mac,
    link_up: bool,
    /// LINKS's speed bits, once up.
    speed: u32,
    /// Its CTRL reset bits, as written at each reset.
    resets: Vec<u32>,
    /// Whether its reset, once made, never clears.
    reset_stuck: bool,
    queues_stuck: bool,
    negotiations: u32,
    sent: Vec<Vec<u8>>,
}

struct Card {
    state: Mutex<State>,
}

impl Card {
    fn new(mac: Mac) -> Arc<Card> {
        let state = State {
            regs: std::vec![0; SIZE / 4],
            eicr: 0,
            mac,
            link_up: mac != Mac::I82599,
            speed: 3 << 28,
            resets: Vec::new(),
            reset_stuck: false,
            queues_stuck: false,
            negotiations: 0,
            sent: Vec::new(),
        };
        let card = Arc::new(Card { state: Mutex::new(state) });
        Card::load(&mut card.state.lock().unwrap());
        card
    }

    /// What a reset leaves: the registers as they start, the address
    /// loaded from the NVM, and the 82599's link kept down by AUTOC2.
    fn load(state: &mut State) {
        state.regs.iter_mut().for_each(|r| *r = 0);
        state.regs[RAL0 / 4] = u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]);
        state.regs[RAH0 / 4] = u32::from(u16::from_le_bytes([MAC[4], MAC[5]])) | (1 << 31);
        state.regs[HLREG0 / 4] = 0x2fff;
        if state.mac == Mac::I82599 {
            state.regs[AUTOC2 / 4] = 0x7 << 28;
        }
    }

    fn reg(&self, offset: usize) -> u32 {
        self.state.lock().unwrap().regs[offset / 4]
    }

    fn set(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
    }

    fn deliver(&self, frame: &[u8], errors: u32) -> bool {
        let mut state = self.state.lock().unwrap();
        let count = state.regs[RDLEN / 4] as usize / 16;
        let head = state.regs[RDH / 4] as usize;
        let on = state.regs[RXDCTL / 4] & QUEUE_ENABLE != 0 && state.regs[RXCTRL / 4] & 1 != 0;
        if !on || !state.link_up || count == 0 || head == state.regs[RDT / 4] as usize {
            return false;
        }
        let base = u64::from(state.regs[RDBAL / 4]) | (u64::from(state.regs[RDBAL / 4 + 1]) << 32);
        let at = base + (head * 16) as u64;
        write_mem(u64_at(at), frame);
        write_mem(at, &[0; 8]);
        write_mem(at + 8, &(0x03 | errors).to_le_bytes());
        write_mem(at + 12, &(frame.len() as u16).to_le_bytes());
        write_mem(at + 14, &[0, 0]);
        state.regs[RDH / 4] = ((head + 1) % count) as u32;
        state.eicr |= 1;
        true
    }

    fn transmit(state: &mut State) {
        if state.regs[TXDCTL / 4] & QUEUE_ENABLE == 0 || state.regs[DMATXCTL / 4] & 1 == 0 {
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
            write_mem(at + 12, &[0x01]);
            head = (head + 1) % count;
        }
        state.regs[TDH / 4] = head as u32;
        state.eicr |= 1;
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
            // Bus mastering stops once it is disabled.
            STATUS => {
                if state.regs[CTRL / 4] & (1 << 2) != 0 {
                    0
                } else {
                    1 << 19
                }
            }
            CTRL if state.reset_stuck && !state.resets.is_empty() => state.regs[CTRL / 4] | RST,
            EICR => core::mem::take(&mut state.eicr),
            LINKS => {
                if state.link_up {
                    (1 << 30) | state.speed
                } else {
                    0
                }
            }
            SECRXSTAT => 1,
            // Reading takes the semaphore when it is free.
            SWSM | SW_FW_SYNC => {
                let bit = if offset == SWSM { 1 } else { REGSMP };
                let value = state.regs[offset / 4];
                state.regs[offset / 4] |= bit;
                value
            }
            _ => state.regs[offset / 4],
        };
        u64::from(value)
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4, "32-bit registers");
        let value = value as u32;
        let mut state = self.state.lock().unwrap();
        match offset {
            CTRL if value & (RST | LNK_RST) != 0 => {
                if state.mac != Mac::I82599 {
                    let sync = state.regs[SW_FW_SYNC / 4];
                    assert_ne!(sync & 0x02, 0, "the reset with the PHY's semaphore held");
                }
                state.resets.push(value & (RST | LNK_RST));
                let sync = state.regs[SW_FW_SYNC / 4];
                let swsm = state.regs[SWSM / 4];
                Card::load(&mut state);
                // The semaphores are not the MAC's to reset.
                state.regs[SW_FW_SYNC / 4] = sync;
                state.regs[SWSM / 4] = swsm;
                if value & LNK_RST != 0 {
                    state.link_up = false;
                }
                state.regs[CTRL / 4] = value & !(RST | LNK_RST);
            }
            AUTOC if value & (1 << 12) != 0 => {
                assert_eq!(state.regs[AUTOC2 / 4] & (0x7 << 28), 0, "the link allowed up");
                state.negotiations += 1;
                state.link_up = true;
                state.regs[AUTOC / 4] = value & !(1 << 12);
            }
            SW_FW_SYNC => {
                assert!(
                    state.regs[SWSM / 4] & 1 != 0 && state.regs[SW_FW_SYNC / 4] & REGSMP != 0,
                    "SW_FW_SYNC with both semaphores held"
                );
                state.regs[offset / 4] = value;
            }
            RXDCTL | TXDCTL if state.queues_stuck => state.regs[offset / 4] = value & !QUEUE_ENABLE,
            EIMC => state.regs[EIMS / 4] &= !value,
            EIMS => state.regs[EIMS / 4] |= value,
            0x00c0 | 0x00d0 | 0x00d8 | 0x0100 | 0x0400 | 0x2800 | 0x3800 => {
                panic!("an e1000 register, {offset:#x}")
            }
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
    let mac = card.state.lock().unwrap().mac;
    ixgbe::start("test", mac, regs, interrupt, [0x02, 0, 0, 0, 0, 1], &mut alloc, &mut no_delay)
}

fn frame(n: usize) -> Vec<u8> {
    let mut frame = std::vec![0u8; 60 + n % 1400];
    frame[..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(&MAC);
    frame[14..22].copy_from_slice(&(n as u64).to_le_bytes());
    frame
}

#[test]
fn an_x540_starts_at_10_gigabits() {
    let card = Card::new(Mac::X540);
    let nic = start(&card, Interrupt::Msi).unwrap();
    assert_eq!(nic.info().mac, MAC);
    assert_eq!((nic.info().receive_slots, nic.info().transmit_slots), (255, 255));
    assert_eq!(card.state.lock().unwrap().resets, [RST], "the MAC's reset");
    assert_eq!(card.reg(SW_FW_SYNC), 0, "the PHY and the register given back");
    assert_eq!(card.reg(SWSM), 0);
    assert_ne!(card.reg(CTRL_EXT) & DRV_LOAD, 0, "the firmware told");
    assert_eq!(card.reg(RXPBSIZE0), 384 << 10);
    assert_eq!(card.reg(SRRCTL), 0x0200_0402, "advanced, one 2 KiB buffer");
    assert_eq!(card.reg(HLREG0) & 0x407, 0x403, "CRC added, stripped, frames padded");
    assert_ne!(card.reg(RDRXCTL) & 2, 0, "CRC stripped");
    assert_eq!(card.reg(FCTRL), (1 << 10) | (1 << 8), "broadcasts, every multicast");
    assert_eq!(card.reg(MPSAR_LO0), 1, "pool 0");
    assert_eq!(card.reg(DMATXCTL) & 1, 1);
    assert_eq!(card.reg(RXCTRL) & 1, 1);
    assert_eq!(card.reg(SECRXCTRL), 0, "the receive path let go");
    assert_eq!(card.reg(RDT), 255);
    assert_eq!((card.reg(GPIE), card.reg(IVAR0)), (0, 0x8080), "MSI, for queue 0");
    assert_eq!(card.reg(EITR0) & 0xff8, 200, "50 us apart");
    assert_eq!(card.reg(EIMS), 1 | (1 << 20), "queue 0 and the link");
    let link = nic.link();
    assert!(link.up && link.full_duplex);
    assert_eq!(link.mbps, 10_000);
}

#[test]
fn an_82599_resets_its_link_and_negotiates_it_again() {
    let card = Card::new(Mac::I82599);
    let nic = start(&card, Interrupt::Polled).unwrap();
    let state = card.state.lock().unwrap();
    assert_eq!(state.resets, [LNK_RST], "the link down: the link reset too");
    assert_eq!(state.negotiations, 1);
    assert_eq!(state.regs[AUTOC2 / 4] & (0x7 << 28), 0, "the link let up");
    assert_eq!(state.regs[EIMS / 4], 0, "polled: no interrupts");
    drop(state);
    assert!(nic.link().up);
}

#[test]
fn an_x550_says_its_other_speeds() {
    let card = Card::new(Mac::X550);
    let nic = start(&card, Interrupt::Polled).unwrap();
    card.set(|state| state.speed = (2 << 28) | (1 << 27));
    assert_eq!(nic.link().mbps, 2_500);
    card.set(|state| state.speed = (1 << 28) | (1 << 27));
    assert_eq!(nic.link().mbps, 5_000);
    card.set(|state| state.speed = 2 << 28);
    assert_eq!(nic.link().mbps, 1_000);
    card.set(|state| state.link_up = false);
    assert_eq!(nic.link(), radix_nic::Link::default());
}

#[test]
fn frames_go_both_ways_round_the_rings() {
    let card = Card::new(Mac::X540);
    let mut nic = start(&card, Interrupt::Msi).unwrap();
    let mut buffer = [0u8; 2048];
    for n in 0..600 {
        assert!(nic.send(&frame(n)));
        nic.flush();
        assert_eq!(card.sent(), [frame(n)]);
        assert!(card.deliver(&frame(n), 0), "frame {n}");
        let len = nic.receive(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], &frame(n)[..]);
        assert_eq!(nic.receive(&mut buffer), None);
        assert_eq!(card.state.lock().unwrap().eicr, 0, "acknowledged");
        nic.flush();
    }
    assert_eq!(nic.dropped(), 0);
}

#[test]
fn damaged_frames_are_dropped() {
    let card = Card::new(Mac::X540);
    let mut nic = start(&card, Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    // Any MAC error, then a CRC error, then a good frame.
    assert!(card.deliver(&frame(1), 0x2000_0000));
    assert!(card.deliver(&frame(2), 0x0100_0000));
    assert!(card.deliver(&frame(3), 0));
    let len = nic.receive(&mut buffer).unwrap();
    assert_eq!(&buffer[..len], &frame(3)[..]);
    assert_eq!(nic.dropped(), 2);
}

#[test]
fn a_reset_that_never_finishes_is_a_timeout() {
    let card = Card::new(Mac::X540);
    card.set(|state| state.reset_stuck = true);
    assert_eq!(start(&card, Interrupt::Polled).err(), Some(Error::Timeout("resetting")));
}

#[test]
fn queues_that_do_not_turn_on_are_a_timeout() {
    let card = Card::new(Mac::X540);
    card.set(|state| state.queues_stuck = true);
    assert!(matches!(start(&card, Interrupt::Polled), Err(Error::Timeout(_))));
    assert_eq!(card.reg(CTRL_EXT) & DRV_LOAD, 0, "given back");
}

#[test]
fn stopping_gives_the_card_back_and_resets_it() {
    let card = Card::new(Mac::X540);
    let mut nic = start(&card, Interrupt::Msi).unwrap();
    nic.stop();
    nic.stop();
    let state = card.state.lock().unwrap();
    assert_eq!(state.resets.len(), 2);
    assert_eq!(state.regs[CTRL_EXT / 4] & DRV_LOAD, 0);
    assert_eq!(state.regs[EIMS / 4], 0);
}

#[test]
fn identifies_the_family() {
    assert_eq!(ixgbe::identify(0x10fb), Some(("82599ES", Mac::I82599)));
    assert_eq!(ixgbe::identify(0x1528), Some(("X540-T2", Mac::X540)));
    assert_eq!(ixgbe::identify(0x15d1), Some(("X550-T1", Mac::X550)));
    // The 82598, and the X550EM's, which set their links up otherwise.
    assert_eq!(ixgbe::identify(0x10c6), None);
    assert_eq!(ixgbe::identify(0x15ad), None);
}
