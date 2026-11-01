// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The e1000 driver against a simulated 82540EM and 82574L.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Mmio, Registers};
use radix_nic::{Error, Nic};

use super::{alloc, no_delay, read_mem, u64_at, write_mem};
use crate::Card as Running;
use crate::Interrupt;
use crate::em::{self, Mac};

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const FALLBACK: [u8; 6] = [0x02, 0, 0, 0, 0, 1];

const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EECD: usize = 0x0010;
const EERD: usize = 0x0014;
const MDIC: usize = 0x0020;
const ICR: usize = 0x00c0;
const IMS: usize = 0x00d0;
const IMC: usize = 0x00d8;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const EXTCNF_CTRL: usize = 0x0f00;
const RDBAL: usize = 0x2800;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const TDBAL: usize = 0x3800;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const TARC0: usize = 0x3840;
const RAL0: usize = 0x5400;
const RAH0: usize = 0x5404;
const GCR: usize = 0x5b00;
const SIZE: usize = 0x20000;

struct State {
    regs: Vec<u32>,
    icr: u32,
    phy_control: u16,
    link_up: bool,
    /// Whether the card loads RAL0 and RAH0 from its NVM at a reset.
    loads_rar: bool,
    /// Whether its NVM is empty, all ones.
    nvm_empty: bool,
    /// Whether it sends what its transmit tail hands it.
    stalled: bool,
    /// Whether its registers read as all ones, as a card that is gone.
    gone: bool,
    sent: Vec<Vec<u8>>,
    resets: u32,
}

struct Card {
    state: Mutex<State>,
    mac: Mac,
}

impl Card {
    fn new(mac: Mac, loads_rar: bool) -> Arc<Card> {
        let state = State {
            regs: std::vec![0; SIZE / 4],
            icr: 0,
            // A PHY left powered down.
            phy_control: 1 << 11,
            link_up: true,
            loads_rar,
            nvm_empty: false,
            stalled: false,
            gone: false,
            sent: Vec::new(),
            resets: 0,
        };
        let card = Arc::new(Card { state: Mutex::new(state), mac });
        card.reset();
        card
    }

    fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.regs.iter_mut().for_each(|r| *r = 0);
        state.resets += 1;
        if state.loads_rar {
            state.regs[RAL0 / 4] = u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]);
            state.regs[RAH0 / 4] = u32::from(u16::from_le_bytes([MAC[4], MAC[5]])) | (1 << 31);
        }
    }

    fn reg(&self, offset: usize) -> u32 {
        self.state.lock().unwrap().regs[offset / 4]
    }

    /// Puts `frame` on the wire to the card, with `errors` in its
    /// descriptor: whether the card had a descriptor for it.
    fn deliver(&self, frame: &[u8], errors: u8) -> bool {
        let mut state = self.state.lock().unwrap();
        let count = state.regs[RDLEN / 4] as usize / 16;
        let head = state.regs[RDH / 4] as usize;
        if count == 0 || head == state.regs[RDT / 4] as usize {
            return false;
        }
        let base = u64::from(state.regs[RDBAL / 4]) | (u64::from(state.regs[RDBAL / 4 + 1]) << 32);
        let at = base + (head * 16) as u64;
        write_mem(u64_at(at), frame);
        let mut write_back = [0u8; 8];
        write_back[..2].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        write_back[4] = 0x01 | 0x02;
        write_back[5] = errors;
        write_mem(at + 8, &write_back);
        state.regs[RDH / 4] = ((head + 1) % count) as u32;
        state.icr |= 1 << 7;
        true
    }

    fn transmit(state: &mut State) {
        let count = state.regs[TDLEN / 4] as usize / 16;
        let base = u64::from(state.regs[TDBAL / 4]) | (u64::from(state.regs[TDBAL / 4 + 1]) << 32);
        let mut head = state.regs[TDH / 4] as usize;
        let tail = state.regs[TDT / 4] as usize;
        while head != tail && !state.stalled {
            let at = base + (head * 16) as u64;
            let descriptor = read_mem(at, 16);
            let len = usize::from(u16::from_le_bytes([descriptor[8], descriptor[9]]));
            let command = descriptor[11];
            assert_eq!(command & 0x0b, 0x0b, "EOP, IFCS and RS");
            state.sent.push(read_mem(u64_at(at), len));
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
        if state.gone {
            return u64::from(u32::MAX);
        }
        let value = match offset {
            STATUS => {
                let mut status = (2 << 6) | (state.regs[STATUS / 4] & (1 << 2));
                if state.link_up {
                    status |= 0x3;
                }
                status
            }
            EECD => state.regs[EECD / 4] | (1 << 9),
            ICR => core::mem::take(&mut state.icr),
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
            EERD if value & 1 != 0 => {
                let (done, shift) =
                    if matches!(self.mac, Mac::I82540) { (1 << 4, 8) } else { (1 << 1, 2) };
                let word = ((value >> shift) & 0xff) as usize;
                let data = match word {
                    _ if state.nvm_empty => 0xffff,
                    0..=2 => u16::from_le_bytes([MAC[2 * word], MAC[2 * word + 1]]),
                    _ => 0xffff,
                };
                state.regs[EERD / 4] = (u32::from(data) << 16) | done;
            }
            MDIC => {
                let register = (value >> 16) & 0x1f;
                assert_eq!((value >> 21) & 0x1f, 1, "the internal PHY");
                let data = match (value >> 26) & 3 {
                    2 if register == 0 => state.phy_control,
                    1 if register == 0 => {
                        state.phy_control = value as u16;
                        value as u16
                    }
                    _ => 0,
                };
                state.regs[MDIC / 4] = (value & !0xffff) | u32::from(data) | (1 << 28);
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
    em::start("test", card.mac, regs, interrupt, FALLBACK, &mut alloc, &mut no_delay)
}

fn frame(n: usize) -> Vec<u8> {
    let mut frame = std::vec![0u8; 60 + n % 100];
    frame[..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(&MAC);
    frame[14..22].copy_from_slice(&(n as u64).to_le_bytes());
    frame
}

#[test]
fn an_82540_starts_with_its_nvm_address_and_a_powered_phy() {
    let card = Card::new(Mac::I82540, false);
    let em = start(&card, Interrupt::Polled).unwrap();
    assert_eq!(em.info().mac, MAC);
    assert_eq!(em.info().receive_slots, 255);
    assert_eq!(card.reg(RAH0) >> 31, 1, "the address is valid");
    assert_eq!(card.reg(RAL0), u32::from_le_bytes([0x52, 0x54, 0x00, 0x12]));
    let rctl = card.reg(RCTL);
    assert_eq!(rctl & ((1 << 1) | (1 << 4) | (1 << 15) | (1 << 26)), rctl, "EN MPE BAM SECRC");
    assert_ne!(card.reg(TCTL) & (1 << 1), 0);
    assert_eq!(card.reg(IMS), 0, "polled: no interrupts");
    assert_eq!(card.reg(RDT), 255);
    let phy = card.state.lock().unwrap().phy_control;
    assert_eq!(phy & (1 << 11), 0, "powered up");
    assert_ne!(phy & (1 << 12), 0, "negotiating");
    assert_eq!(card.state.lock().unwrap().resets, 2, "the card's own and the driver's");
    let link = em.link();
    assert!(link.up && link.full_duplex);
    assert_eq!(link.mbps, 1000);
}

#[test]
fn an_82574_takes_its_address_from_its_registers_and_raises_an_msi() {
    let card = Card::new(Mac::I82574, true);
    let em = start(&card, Interrupt::Msi).unwrap();
    assert_eq!(em.info().mac, MAC);
    assert_ne!(card.reg(IMS) & (1 << 7), 0, "frames received");
    assert_ne!(card.reg(GCR) & (1 << 22), 0, "the 82574's errata");
    assert_ne!(card.reg(TARC0) & (1 << 26), 0);
    assert_ne!(card.reg(TCTL) & (1 << 28), 0, "MULR on PCI Express");
    assert_eq!(card.reg(EXTCNF_CTRL) & (1 << 5), 0, "MDIO given back");
}

#[test]
fn without_an_address_it_takes_the_fallback() {
    let card = Card::new(Mac::I82574, false);
    card.state.lock().unwrap().nvm_empty = true;
    let em = start(&card, Interrupt::Polled).unwrap();
    assert_eq!(em.info().mac, FALLBACK);
}

#[test]
fn frames_go_both_ways_and_round_the_rings() {
    let card = Card::new(Mac::I82540, false);
    let mut em = start(&card, Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    for round in 0..5 {
        // More than a ring's worth in all, a batch at a time.
        for n in 0..200 {
            assert!(em.can_send());
            assert!(em.send(&frame(round * 1000 + n)));
        }
        assert!(card.sent().is_empty(), "nothing goes before the doorbell");
        em.flush();
        let sent = card.sent();
        assert_eq!(sent.len(), 200);
        for (n, got) in sent.iter().enumerate() {
            assert_eq!(*got, frame(round * 1000 + n));
        }
        for n in 0..200 {
            assert!(card.deliver(&frame(n), 0), "round {round}, frame {n}");
            let len = em.receive(&mut buffer).unwrap();
            assert_eq!(&buffer[..len], &frame(n)[..]);
        }
        assert_eq!(em.receive(&mut buffer), None);
        em.flush();
        assert_eq!(card.reg(RDT), card.reg(RDH).wrapping_sub(1) % 256);
    }
    assert_eq!(em.dropped(), 0);
}

#[test]
fn damaged_and_short_frames_are_dropped() {
    let card = Card::new(Mac::I82540, false);
    let mut em = start(&card, Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    assert!(card.deliver(&frame(1), 0x01));
    assert!(card.deliver(&[0u8; 10], 0));
    assert!(card.deliver(&frame(2), 0));
    let len = em.receive(&mut buffer).unwrap();
    assert_eq!(&buffer[..len], &frame(2)[..]);
    assert_eq!(em.dropped(), 2);
    // A frame is cut to the buffer it is copied to.
    assert!(card.deliver(&frame(3), 0));
    let mut small = [0u8; 20];
    assert_eq!(em.receive(&mut small), Some(20));
    assert_eq!(&small[..], &frame(3)[..20]);
}

#[test]
fn a_full_receive_ring_waits_for_its_tail() {
    let card = Card::new(Mac::I82540, false);
    let mut em = start(&card, Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    for n in 0..255 {
        assert!(card.deliver(&frame(n), 0));
    }
    assert!(!card.deliver(&frame(255), 0), "the tail's descriptor is the card's to keep");
    assert!(em.receive(&mut buffer).is_some());
    assert!(!card.deliver(&frame(255), 0), "not until the tail is written");
    em.flush();
    assert!(card.deliver(&frame(255), 0));
}

#[test]
fn a_stalled_card_runs_out_of_transmit_slots() {
    let card = Card::new(Mac::I82540, false);
    let mut em = start(&card, Interrupt::Polled).unwrap();
    card.state.lock().unwrap().stalled = true;
    for n in 0..255 {
        assert!(em.send(&frame(n)), "frame {n}");
    }
    assert!(!em.can_send());
    assert!(!em.send(&frame(255)));
    em.flush();
    card.state.lock().unwrap().stalled = false;
    // The card sends them once it runs again.
    card.write(TDT, 4, u64::from(card.reg(TDT)));
    assert_eq!(card.sent().len(), 255);
    assert!(em.can_send());
}

#[test]
fn frames_of_the_wrong_length_are_not_sent() {
    let card = Card::new(Mac::I82540, false);
    let mut em = start(&card, Interrupt::Polled).unwrap();
    assert!(!em.send(&[0u8; 13]));
    assert!(!em.send(&[0u8; 1519]));
    assert!(em.send(&[0u8; 1518]));
}

#[test]
fn the_interrupt_is_acknowledged_once_no_frame_is_left() {
    let card = Card::new(Mac::I82574, true);
    let mut em = start(&card, Interrupt::Msi).unwrap();
    let mut buffer = [0u8; 2048];
    assert!(card.deliver(&frame(1), 0));
    assert!(em.receive(&mut buffer).is_some());
    assert_ne!(card.state.lock().unwrap().icr, 0);
    assert_eq!(em.receive(&mut buffer), None);
    assert_eq!(card.state.lock().unwrap().icr, 0);
}

#[test]
fn the_link_follows_the_card() {
    let card = Card::new(Mac::I82540, false);
    let em = start(&card, Interrupt::Polled).unwrap();
    card.state.lock().unwrap().link_up = false;
    assert!(!em.link().up);
    assert!(!em.failed());
    card.state.lock().unwrap().gone = true;
    assert!(em.failed());
}

#[test]
fn a_card_that_is_gone_is_refused() {
    let card = Card::new(Mac::I82540, false);
    card.state.lock().unwrap().gone = true;
    assert!(matches!(start(&card, Interrupt::Polled), Err(Error::Device(_))));
}

#[test]
fn a_window_too_small_is_refused() {
    let card = Card::new(Mac::I82540, false);
    let regs = Mmio::fake(card.clone(), 0x1000);
    let started = em::start(
        "test",
        Mac::I82540,
        regs,
        Interrupt::Polled,
        FALLBACK,
        &mut alloc,
        &mut no_delay,
    );
    assert!(matches!(started, Err(Error::Registers)));
}

#[test]
fn dropping_it_stops_the_card() {
    let card = Card::new(Mac::I82540, false);
    let em = start(&card, Interrupt::Polled).unwrap();
    drop(em);
    assert_eq!(card.reg(RCTL), 0);
    assert_eq!(card.state.lock().unwrap().resets, 3);
}

#[test]
fn a_stopped_card_does_nothing_more_and_is_stopped_once() {
    let card = Card::new(Mac::I82574, true);
    let mut em = start(&card, Interrupt::Msi).unwrap();
    em.stop();
    assert_eq!(card.reg(IMS), 0, "interrupts off");
    assert_eq!(card.state.lock().unwrap().resets, 3);
    assert!(!em.can_send());
    assert!(!em.send(&frame(1)));
    card.deliver(&frame(2), 0);
    let mut buffer = [0u8; 2048];
    assert_eq!(em.receive(&mut buffer), None);
    em.flush();
    assert!(card.sent().is_empty());
    em.stop();
    drop(em);
    assert_eq!(card.state.lock().unwrap().resets, 3, "stopped once");
}
