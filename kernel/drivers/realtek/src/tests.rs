// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated chip: registers that act as FreeBSD's
//! `re(4)` expects them to, a PHY behind the PHY access register, and a
//! wire whose frames the tests put in and take out through the
//! descriptors in (fake) DMA memory.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};
use radix_nic::{Error, Interrupt, Link, Nic, PAGE};

use crate::chip::{self, needs};
use crate::{Card, DEVICE_8101E, Model, identify, reg, start};

const MAC: [u8; 6] = [0x00, 0xe0, 0x4c, 0x68, 0x01, 0x02];
const FALLBACK: [u8; 6] = [0x02, 0, 0, 0, 0, 1];

/// The revisions the tests run: the Dell's RTL8106E, an RTL8168H (the
/// RXDV gate), an RTL8168DP (the transmit poll wait) and an RTL8169S.
const RTL8106E: u32 = 0x4480_0000;
const RTL8168H: u32 = 0x5400_0000;
const RTL8168DP: u32 = 0x2880_0000;
const RTL8169S: u32 = 0x0080_0000;

const OWN: u32 = 0x8000_0000;
const EOR: u32 = 0x4000_0000;
const SOF: u32 = 0x2000_0000;
const EOF: u32 = 0x1000_0000;
const RX_ERROR: u32 = 0x0020_0000;
const PHYAR_FLAG: u32 = 0x8000_0000;
/// The revision's bits of the transmit configuration, which the chip
/// keeps whatever is written there, and the queue empty bit.
const REVISION_BITS: u32 = 0xfcf0_0000;
const QUEUE_EMPTY: u32 = 0x0800;

fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * PAGE))
}

fn no_delay(_us: u32) {}

fn read_mem(phys: u64, len: usize) -> Vec<u8> {
    let mut out = std::vec![0u8; len];
    fake_memory(phys, &mut out);
    out
}

fn u32_at(phys: u64) -> u32 {
    u32::from_le_bytes(read_mem(phys, 4).try_into().unwrap())
}

fn u64_at(phys: u64) -> u64 {
    u64::from_le_bytes(read_mem(phys, 8).try_into().unwrap())
}

struct State {
    regs: [u8; 256],
    /// The revision in the transmit configuration register.
    revision: u32,
    /// The address the chip loads into IDR0 to IDR5 at a reset.
    eeprom: [u8; 6],
    phy: [u16; 32],
    media: u8,
    /// The next descriptor of each ring the chip uses.
    rx_index: usize,
    tx_index: usize,
    sent: Vec<Vec<u8>>,
    /// How many transmit polls the chip ignores, as a busy one can.
    ignore_polls: u32,
    /// Every write: register, size, value.
    writes: Vec<(usize, usize, u64)>,
    resets: u32,
}

struct Chip {
    state: Mutex<State>,
}

impl Chip {
    fn new(revision: u32, eeprom: [u8; 6]) -> Arc<Chip> {
        let state = State {
            regs: [0; 256],
            revision,
            eeprom,
            // A PHY left powered down.
            phy: [0; 32],
            media: 0,
            rx_index: 0,
            tx_index: 0,
            sent: Vec::new(),
            ignore_polls: 0,
            writes: Vec::new(),
            resets: 0,
        };
        let chip = Arc::new(Chip { state: Mutex::new(state) });
        {
            let mut state = chip.state.lock().unwrap();
            state.phy[0] = 1 << 11;
            Chip::reset(&mut state);
            state.resets = 0;
        }
        chip
    }

    /// A software reset: the MAC's registers, not the configuration
    /// registers and the power management beside them (0x50 to 0x6f).
    fn reset(state: &mut State) {
        let kept: [u8; 0x20] = state.regs[0x50..0x70].try_into().unwrap();
        state.regs = [0; 256];
        state.regs[0x50..0x70].copy_from_slice(&kept);
        state.regs[..6].copy_from_slice(&state.eeprom);
        // The FIFOs are empty after a reset.
        state.regs[reg::MCU] = 0x30;
        state.rx_index = 0;
        state.tx_index = 0;
        state.resets += 1;
    }

    fn get(state: &State, offset: usize, size: usize) -> u64 {
        let mut bytes = [0u8; 8];
        bytes[..size].copy_from_slice(&state.regs[offset..offset + size]);
        u64::from_le_bytes(bytes)
    }

    fn set(state: &mut State, offset: usize, size: usize, value: u64) {
        state.regs[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
    }

    fn ring(state: &State, lo: usize) -> u64 {
        Chip::get(state, lo, 4) | (Chip::get(state, lo + 4, 4) << 32)
    }

    fn transmit(state: &mut State) {
        let base = Chip::ring(state, reg::TXLIST_LO);
        loop {
            let at = base + (state.tx_index * 16) as u64;
            let command = u32_at(at);
            if command & OWN == 0 {
                break;
            }
            assert_eq!(command & (SOF | EOF), SOF | EOF, "one buffer a frame");
            let len = (command & 0xffff) as usize;
            state.sent.push(read_mem(u64_at(at + 8), len));
            fake_memory_write(at, &(command & !OWN).to_le_bytes());
            state.tx_index = if command & EOR != 0 { 0 } else { state.tx_index + 1 };
            let isr = Chip::get(state, reg::ISR, 2) | 0x04;
            Chip::set(state, reg::ISR, 2, isr);
        }
    }

    /// Puts `frame` on the wire to the chip, with its frame check
    /// sequence and `status` in its descriptors, in as many buffers as it
    /// needs: whether the chip had them.
    fn deliver(&self, frame: &[u8], status: u32) -> bool {
        let mut state = self.state.lock().unwrap();
        let base = Chip::ring(&state, reg::RXLIST_LO);
        if Chip::get(&state, reg::COMMAND, 1) & 0x08 == 0 {
            return false;
        }
        let mut data = frame.to_vec();
        data.extend_from_slice(&[0xfc, 0xfc, 0xfc, 0xfc]);
        let mut offset = 0;
        let mut first = true;
        while offset < data.len() {
            let at = base + (state.rx_index * 16) as u64;
            let command = u32_at(at);
            if command & OWN == 0 {
                return false;
            }
            let room = (command & 0x1fff) as usize;
            let piece = room.min(data.len() - offset);
            fake_memory_write(u64_at(at + 8), &data[offset..offset + piece]);
            offset += piece;
            let mut written = (command & EOR) | status;
            if first {
                written |= SOF;
            }
            written |= if offset == data.len() { EOF | data.len() as u32 } else { room as u32 };
            fake_memory_write(at, &written.to_le_bytes());
            state.rx_index = if command & EOR != 0 { 0 } else { state.rx_index + 1 };
            first = false;
        }
        let isr = Chip::get(&state, reg::ISR, 2) | 0x01;
        Chip::set(&mut state, reg::ISR, 2, isr);
        true
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.state.lock().unwrap().sent)
    }

    fn reg(&self, offset: usize, size: usize) -> u64 {
        Chip::get(&self.state.lock().unwrap(), offset, size)
    }

    /// The order of the writes to `offset` with `value`, among all.
    fn write_index(&self, offset: usize, value: u64) -> Option<usize> {
        let state = self.state.lock().unwrap();
        state.writes.iter().rposition(|&(o, _, v)| o == offset && v == value)
    }
}

impl Registers for Chip {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let state = self.state.lock().unwrap();
        match offset {
            reg::TXCFG => {
                let written = Chip::get(&state, offset, 4) as u32 & !REVISION_BITS;
                u64::from(written | state.revision | QUEUE_EMPTY)
            }
            reg::GMEDIASTAT => u64::from(state.media),
            _ => Chip::get(&state, offset, size),
        }
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.state.lock().unwrap();
        state.writes.push((offset, size, value));
        match offset {
            reg::COMMAND if value & 0x10 != 0 => Chip::reset(&mut state),
            reg::ISR => {
                let isr = Chip::get(&state, reg::ISR, 2) & !value;
                Chip::set(&mut state, reg::ISR, 2, isr);
            }
            reg::PHYAR => {
                let phy = ((value >> 16) & 0x1f) as usize;
                let value = value as u32;
                let done = if value & PHYAR_FLAG != 0 {
                    state.phy[phy] = value as u16;
                    value & !PHYAR_FLAG
                } else {
                    PHYAR_FLAG | (value & 0x001f_0000) | u32::from(state.phy[phy])
                };
                Chip::set(&mut state, reg::PHYAR, 4, u64::from(done));
            }
            reg::TXPOLL if value & 0x40 != 0 => {
                if state.ignore_polls > 0 {
                    state.ignore_polls -= 1;
                } else {
                    Chip::transmit(&mut state);
                }
            }
            _ => Chip::set(&mut state, offset, size, value),
        }
    }
}

fn chip(revision: u32) -> (Arc<Chip>, Mmio) {
    let chip = Chip::new(revision, MAC);
    let regs = Mmio::fake(chip.clone(), 256);
    (chip, regs)
}

fn started(revision: u32, device: u16, interrupt: Interrupt) -> (Arc<Chip>, Card) {
    let (chip, regs) = chip(revision);
    let card = start(device, regs, interrupt, FALLBACK, &mut alloc, &mut no_delay).unwrap();
    (chip, card)
}

fn frame(n: usize, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i + n) as u8).collect()
}

#[test]
fn identifies_realtek_cards_by_their_pci_ids() {
    assert_eq!(identify(0x10ec, 0x8136, 0), Some(Model { name: "RTL810xE", pcie: true }));
    assert_eq!(identify(0x10ec, 0x8168, 0x1234).map(|m| m.name), Some("RTL8168"));
    assert_eq!(identify(0x10ec, 0x8169, 0), Some(Model { name: "RTL8169", pcie: false }));
    assert!(!identify(0x10ec, 0x8169, 0).unwrap().msi());
    assert_eq!(identify(0x1186, 0x4300, 0).map(|m| m.name), Some("RTL8169"));
    // The Linksys EG1032 only in its third revision; the RTL8139, the
    // RTL8125 and other makers' IDs not at all.
    assert_eq!(identify(0x1737, 0x1032, 0x0024).map(|m| m.name), Some("RTL8169S"));
    assert_eq!(identify(0x1737, 0x1032, 0x0015), None);
    assert_eq!(identify(0x10ec, 0x8139, 0), None);
    assert_eq!(identify(0x10ec, 0x8125, 0), None);
    assert_eq!(identify(0x8086, 0x8168, 0), None);
}

#[test]
fn revisions_are_told_apart_by_the_transmit_configuration() {
    // The MAC revision and low bits do not change the revision.
    let rtl8106e = chip::identify(RTL8106E | 0x0010_0700, 0x8136).unwrap();
    assert_eq!(rtl8106e.name, "RTL8106E");
    assert!(rtl8106e.needs(needs::FAST | needs::PHY_WAKE_PM | needs::CMD_STOP));
    let h = chip::identify(RTL8168H, 0x8168).unwrap();
    assert_eq!(h.name, "RTL8168H/8111H");
    assert!(h.needs(needs::G_PLUS) && h.needs(needs::JUMBO_V2) && !h.needs(needs::FAST));
    // The same revision on the RTL810x's device ID is the RTL8107E.
    let rtl8107e = chip::identify(RTL8168H, DEVICE_8101E).unwrap();
    assert_eq!(rtl8107e.name, "RTL8107E");
    assert!(rtl8107e.needs(needs::FAST) && !rtl8107e.needs(needs::JUMBO_V2));
    assert_eq!(chip::identify(0x5080_0000, DEVICE_8101E).unwrap().name, "RTL8106EUS");
    // The RTL8169s keep their top bit; the RTL8168C's MAC revision 2
    // sleeps.
    assert_eq!(chip::identify(0x9800_0000, 0x8167).unwrap().name, "RTL8169SC/8110SC");
    assert_eq!(chip::identify(0x0000_0000, 0x8169).unwrap().name, "RTL8169");
    assert!(chip::identify(0x3c20_0000, 0x8168).unwrap().needs(needs::MAC_SLEEP));
    assert!(!chip::identify(0x3c00_0000, 0x8168).unwrap().needs(needs::MAC_SLEEP));
    // The RTL8139C+ and an unknown revision.
    assert_eq!(chip::identify(0x7480_0000, 0x8139), None);
    assert_eq!(chip::identify(0x6c00_0000, 0x8168), None);
}

#[test]
fn start_sets_the_chip_up_as_freebsd_does() {
    let (chip, card) = started(RTL8106E, 0x8136, Interrupt::Msi);
    let info = card.info();
    assert_eq!(info.model, "RTL8106E");
    assert_eq!(info.mac, MAC);
    assert_eq!((info.receive_slots, info.transmit_slots), (256, 255));
    // Its address written back, every multicast frame and broadcasts
    // taken, the receiver and transmitter on, an MSI for what the
    // driver handles, frames no longer than a slot.
    assert_eq!(chip.reg(reg::IDR0, 4), u64::from(u32::from_le_bytes([0x00, 0xe0, 0x4c, 0x68])));
    assert_eq!(chip.reg(reg::MAR0, 4), 0xffff_ffff);
    assert_eq!(chip.reg(reg::MAR4, 4), 0xffff_ffff);
    assert_eq!(chip.reg(reg::RXCFG, 4), 0xe000 | 0x0700 | 0x1800 | 0x02 | 0x04 | 0x08);
    assert_eq!(chip.reg(reg::TXCFG, 4) as u32 & !REVISION_BITS & !QUEUE_EMPTY, 0x0300_0700);
    assert_eq!(chip.reg(reg::COMMAND, 1), 0x0c);
    assert_eq!(chip.reg(reg::IMR, 2), 0x7f);
    assert_eq!(chip.reg(reg::MAXRXPKTLEN, 2), 2048);
    // MAC statistics off on this one, with C+ transmit's bit.
    assert_eq!(chip.reg(reg::CPLUS, 2), 0x0008 | 0x0080 | 0x0001);
    // Its PHY out of power management, powered up and negotiating.
    assert_eq!(chip.reg(reg::PMCH, 1) & 0x80, 0x80);
    let control = chip.state.lock().unwrap().phy[0];
    assert_eq!(control & (1 << 11), 0);
    assert_eq!(control & (1 << 12 | 1 << 9), 1 << 12 | 1 << 9);
    // The rings' addresses, page-aligned, every receive descriptor the
    // chip's with a slot's room, and the last ones ending their rings.
    let rx = chip.reg(reg::RXLIST_LO, 4) | (chip.reg(reg::RXLIST_HI, 4) << 32);
    let tx = chip.reg(reg::TXLIST_LO, 4) | (chip.reg(reg::TXLIST_HI, 4) << 32);
    assert_eq!((rx % 256, tx % 256), (0, 0));
    assert_eq!(u32_at(rx), OWN | 2048);
    assert_eq!(u32_at(rx + 255 * 16), OWN | EOR | 2048);
    assert_eq!(u32_at(tx), 0);
    assert_eq!(u32_at(tx + 255 * 16), EOR);
}

#[test]
fn a_chip_without_an_address_gets_the_fallback() {
    let chip = Chip::new(RTL8106E, [0; 6]);
    let regs = Mmio::fake(chip.clone(), 256);
    let card = start(0x8136, regs, Interrupt::Polled, FALLBACK, &mut alloc, &mut no_delay).unwrap();
    assert_eq!(card.info().mac, FALLBACK);
    assert_eq!(chip.reg(reg::IDR0, 4), u64::from(u32::from_le_bytes([2, 0, 0, 0])));
    // Polled: no interrupt causes unmasked.
    assert_eq!(chip.reg(reg::IMR, 2), 0);
}

#[test]
fn unknown_revisions_and_gone_cards_are_refused() {
    let (_, regs) = chip(0x6c00_0000);
    let refused = start(0x8168, regs, Interrupt::Polled, FALLBACK, &mut alloc, &mut no_delay);
    assert_eq!(refused.err(), Some(Error::Unsupported("this revision of the chip")));
    // A card that reads as all ones.
    struct Gone;
    impl Registers for Gone {
        fn read(&self, _: usize, size: usize) -> u64 {
            u64::MAX >> (64 - 8 * size)
        }
        fn write(&self, _: usize, _: usize, _: u64) {}
    }
    let gone = Mmio::fake(Arc::new(Gone), 256);
    let refused = start(0x8136, gone, Interrupt::Polled, FALLBACK, &mut alloc, &mut no_delay);
    assert!(matches!(refused.err(), Some(Error::Device(_))));
    let (_, regs) = chip(RTL8106E);
    let small = regs.window(0, 0x80).unwrap();
    let refused = start(0x8136, small, Interrupt::Polled, FALLBACK, &mut alloc, &mut no_delay);
    assert_eq!(refused.err(), Some(Error::Registers));
}

#[test]
fn the_rtl8168g_and_later_turn_their_macs_on_last() {
    let (chip, _card) = started(RTL8168H, 0x8168, Interrupt::Msi);
    let on = chip.write_index(reg::COMMAND, 0x0c).unwrap();
    let configured = chip.write_index(reg::RXCFG, 0xe000 | 0x0700 | 0x1800 | 0x0e | 0x0800);
    assert!(configured.unwrap() < on, "the receive configuration, early off V2, first");
    assert_eq!(chip.reg(reg::MISC, 4) & 0x0008_0000, 0, "the RXDV gate open");
    assert_eq!(chip.reg(reg::MAXRXPKTLEN, 2), 2048);
    // An RTL8169S turns them on first, takes frames of any length, and
    // has both C+ enables.
    let (chip, _card) = started(RTL8169S, 0x8169, Interrupt::Polled);
    let on = chip.write_index(reg::COMMAND, 0x0c).unwrap();
    assert!(on < chip.write_index(reg::TXCFG, 0x0300_0700).unwrap());
    assert_eq!(chip.reg(reg::MAXRXPKTLEN, 2), 16383);
    assert_eq!(chip.reg(reg::CPLUS, 2), 0x0008 | 0x0002 | 0x0001);
    assert_eq!(chip.reg(reg::MAC_RESET, 1), 1);
}

#[test]
fn received_frames_lose_their_check_sequence() {
    let (chip, mut card) = started(RTL8106E, 0x8136, Interrupt::Polled);
    let mut buffer = [0u8; 2048];
    assert_eq!(card.receive(&mut buffer), None);
    // More frames than descriptors: each is handed back as it is taken.
    for n in 0..600 {
        let sent = frame(n, 60 + n % 1400);
        assert!(chip.deliver(&sent, 0), "frame {n}");
        let len = card.receive(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], &sent[..], "frame {n}");
    }
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.dropped(), 0);
}

#[test]
fn damaged_short_and_long_frames_are_dropped() {
    let (chip, mut card) = started(RTL8169S, 0x8169, Interrupt::Polled);
    let mut buffer = [0u8; 2048];
    let good = frame(7, 100);
    assert!(chip.deliver(&frame(1, 100), RX_ERROR));
    assert!(chip.deliver(&frame(2, 10), 0));
    // Longer than a slot: in two buffers, both skipped.
    assert!(chip.deliver(&frame(3, 3000), 0));
    assert!(chip.deliver(&good, 0));
    let len = card.receive(&mut buffer).unwrap();
    assert_eq!(&buffer[..len], &good[..]);
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(card.dropped(), 4);
    // A frame cut to the buffer it is copied into.
    assert!(chip.deliver(&good, 0));
    let mut small = [0u8; 40];
    assert_eq!(card.receive(&mut small), Some(40));
    assert_eq!(&small[..], &good[..40]);
}

#[test]
fn frames_are_sent_padded_once_flushed() {
    let (chip, mut card) = started(RTL8106E, 0x8136, Interrupt::Polled);
    assert!(card.can_send());
    assert!(card.send(&frame(1, 14)));
    assert!(card.send(&frame(2, 1514)));
    assert!(chip.sent().is_empty(), "nothing before the poll");
    card.flush();
    let sent = chip.sent();
    assert_eq!(sent.len(), 2);
    let mut padded = frame(1, 14);
    padded.resize(60, 0);
    assert_eq!(sent[0], padded);
    assert_eq!(sent[1], frame(2, 1514));
    // Frames of no length a frame has are refused.
    assert!(!card.send(&frame(3, 13)));
    assert!(!card.send(&frame(3, 1519)));
    // Round the ring more than once.
    for n in 0..600 {
        assert!(card.send(&frame(n, 200)), "frame {n}");
        card.flush();
        assert_eq!(chip.sent(), [frame(n, 200)]);
    }
}

#[test]
fn a_full_ring_waits_and_a_lost_poll_is_made_again() {
    let (chip, mut card) = started(RTL8106E, 0x8136, Interrupt::Polled);
    chip.state.lock().unwrap().ignore_polls = 1;
    for n in 0..255 {
        assert!(card.send(&frame(n, 64)), "frame {n}");
    }
    assert!(!card.can_send());
    assert!(!card.send(&frame(255, 64)));
    // The chip ignores the first poll; the next flush polls again, as the
    // chip still owns the descriptors.
    card.flush();
    assert!(chip.sent().is_empty());
    card.flush();
    assert_eq!(chip.sent().len(), 255);
    assert!(card.can_send());
    // With every descriptor back, a flush polls no more.
    let polls = |chip: &Chip| {
        let state = chip.state.lock().unwrap();
        state.writes.iter().filter(|&&(o, ..)| o == reg::TXPOLL).count()
    };
    let before = polls(&chip);
    card.flush();
    assert_eq!(polls(&chip), before);
}

#[test]
fn the_interrupt_is_acknowledged_once_nothing_is_left() {
    let (chip, mut card) = started(RTL8168H, 0x8168, Interrupt::Msi);
    let mut buffer = [0u8; 2048];
    assert!(chip.deliver(&frame(1, 64), 0));
    assert!(chip.deliver(&frame(2, 64), 0));
    assert!(card.receive(&mut buffer).is_some());
    assert_eq!(chip.reg(reg::ISR, 2) & 0x01, 0x01, "not before the last frame");
    assert!(card.receive(&mut buffer).is_some());
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(chip.reg(reg::ISR, 2), 0);
    assert!(!card.failed());
    // A PCI error stays, and says the card failed.
    {
        let mut state = chip.state.lock().unwrap();
        Chip::set(&mut state, reg::ISR, 2, 0x8001);
    }
    assert_eq!(card.receive(&mut buffer), None);
    assert_eq!(chip.reg(reg::ISR, 2), 0x8000);
    assert!(card.failed());
}

#[test]
fn the_link_is_read_from_the_media_status() {
    let (chip, card) = started(RTL8168H, 0x8168, Interrupt::Msi);
    assert_eq!(card.link(), Link::default());
    let set = |media: u8| chip.state.lock().unwrap().media = media;
    set(0x02 | 0x10 | 0x01);
    assert_eq!(card.link(), Link { up: true, mbps: 1000, full_duplex: true });
    set(0x02 | 0x08);
    assert_eq!(card.link(), Link { up: true, mbps: 100, full_duplex: false });
    set(0x02 | 0x04 | 0x01);
    assert_eq!(card.link(), Link { up: true, mbps: 10, full_duplex: true });
    set(0xff);
    assert_eq!(card.link(), Link::default());
}

#[test]
fn stopping_resets_the_chip_and_ends_its_frames() {
    let (chip, mut card) = started(RTL8168H, 0x8168, Interrupt::Msi);
    let resets = chip.state.lock().unwrap().resets;
    card.stop();
    assert_eq!(chip.state.lock().unwrap().resets, resets + 1);
    // The RXDV gate closed, the MACs off, then the reset.
    let gate = chip.write_index(reg::MISC, 0x0008_0000).unwrap();
    let off = chip.write_index(reg::COMMAND, 0x00).unwrap();
    let reset = chip.write_index(reg::COMMAND, 0x10).unwrap();
    assert!(gate < off && off < reset);
    assert_eq!(chip.reg(reg::IMR, 2), 0);
    let mut buffer = [0u8; 2048];
    assert!(!chip.deliver(&frame(1, 64), 0), "its receiver is off");
    assert_eq!(card.receive(&mut buffer), None);
    assert!(!card.send(&frame(1, 64)));
    // A second stop does nothing; dropping a running card stops it.
    card.stop();
    assert_eq!(chip.state.lock().unwrap().resets, resets + 1);
    let (chip, card) = started(RTL8168DP, 0x8168, Interrupt::Msi);
    let resets = chip.state.lock().unwrap().resets;
    drop(card);
    assert_eq!(chip.state.lock().unwrap().resets, resets + 1);
    assert!(chip.write_index(reg::COMMAND, 0x00).is_some(), "the RTL8168DP turned off");
}
