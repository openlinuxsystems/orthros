// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated gVNIC, which acts as FreeBSD's gve
//! expects the card to in its GQI-QPL format: big-endian registers, admin
//! commands run as the doorbell counts them, frames only through the
//! registered pages, a counter of the transmit descriptors done, receive
//! descriptors marked with sequence numbers 1 to 7, and notification
//! blocks that raise an interrupt once each until acknowledged.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::vec::Vec;

use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};
use radix_nic::{Error, Interrupt, Nic, PAGE};

use crate::{DEVICE, Gve, VENDOR, start};

const MAC: [u8; 6] = [0x42, 0x01, 0x0a, 0x80, 0x00, 0x02];
const FALLBACK: [u8; 6] = [0x02, 0, 0, 0, 0, 1];

const DEVICE_STATUS: usize = 0;
const MAX_TX_QUEUES: usize = 8;
const MAX_RX_QUEUES: usize = 12;
const ADMINQ_PFN: usize = 16;
const ADMINQ_DOORBELL: usize = 20;
const ADMINQ_EVENT_COUNTER: usize = 24;
const DRIVER_STATUS: usize = 4;
const DRIVER_VERSION: usize = 31;
const ADMINQ_BASE_ADDRESS_HI: usize = 32;
const ADMINQ_BASE_ADDRESS_LO: usize = 36;
const ADMINQ_LENGTH: usize = 40;

const IRQ_ACK: u32 = 1 << 31;
const IRQ_MASK: u32 = 1 << 30;
const IRQ_EVENT: u32 = 1 << 29;

/// The doorbell indexes the simulated card gives: its queues', then its
/// notification blocks', each 4 bytes apart in BAR 2.
const TX_DOORBELL: u32 = 0x100;
const RX_DOORBELL: u32 = 0x101;
const IRQ_DOORBELLS: u32 = 0x200;
const TX_COUNTER: u32 = 5;
const DOORS: usize = 0x2000;
/// Its queues: two of each, so the receive queue's notification block
/// and page list are numbered 2.
const QUEUES: u32 = 2;

fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * PAGE))
}

fn read(phys: u64, len: usize) -> Vec<u8> {
    let mut out = std::vec![0u8; len];
    fake_memory(phys, &mut out);
    out
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn be64(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn write_be32(phys: u64, value: u32) {
    fake_memory_write(phys, &value.to_be_bytes());
}

/// A notification block's interrupt, as the card raises it.
#[derive(Default, Clone, Copy)]
struct Block {
    masked: bool,
    /// Whether it may raise one: acknowledged since the last.
    armed: bool,
    pending: bool,
}

struct State {
    status: u32,
    /// The admin queue's address, and its high half as written.
    admin: u64,
    admin_high: u32,
    admin_length: u16,
    /// Whether the driver runs, and how often it reset the card.
    running: bool,
    resets: u32,
    events: u32,
    version: Vec<u8>,
    tx_entries: u16,
    rx_entries: u16,
    rx_pages: u16,
    /// The options it describes: id and data.
    options: Vec<(u16, Vec<u8>)>,
    compat_unimplemented: bool,
    capabilities: Option<u64>,
    refuses: Option<u32>,
    mtu: Option<u64>,
    format: u8,
    counters: u64,
    irq_dbs: u64,
    lists: BTreeMap<u32, Vec<u64>>,
    tx: Option<(u64, u32, u16)>,
    rx: Option<(u64, u64, u32, u16, u32)>,
    blocks: [Block; 4],
    interrupts: u32,
    irq_writes: Vec<(usize, u32)>,
    tx_head: u32,
    tx_counter: u32,
    stalled: bool,
    sent: Vec<Vec<u8>>,
    descriptors: Vec<u8>,
    rx_filled: u32,
    rx_written: u32,
}

struct Card {
    state: Mutex<State>,
}

/// BAR 0 or BAR 2 of the card.
struct Bar(Arc<Card>, bool);

impl Card {
    fn new() -> Arc<Card> {
        Arc::new(Card {
            state: Mutex::new(State {
                status: 1 << 2,
                admin: 0,
                admin_high: 0,
                admin_length: 0,
                running: false,
                resets: 0,
                events: 0,
                version: Vec::new(),
                tx_entries: 256,
                rx_entries: 256,
                rx_pages: 256,
                options: std::vec![(3, std::vec![0; 4])],
                compat_unimplemented: false,
                capabilities: None,
                refuses: None,
                mtu: None,
                format: 0,
                counters: 0,
                irq_dbs: 0,
                lists: BTreeMap::new(),
                tx: None,
                rx: None,
                blocks: [Block::default(); 4],
                interrupts: 0,
                irq_writes: Vec::new(),
                tx_head: 0,
                tx_counter: 0,
                stalled: false,
                sent: Vec::new(),
                descriptors: Vec::new(),
                rx_filled: 0,
                rx_written: 0,
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn start(self: &Arc<Card>, interrupt: Interrupt) -> Result<Gve, Error> {
        let regs = Mmio::fake(Arc::new(Bar(self.clone(), false)), 64);
        let doors = Mmio::fake(Arc::new(Bar(self.clone(), true)), DOORS);
        // Revision 1, as the host's addresses are past what a page
        // number of 32 bits reaches.
        start(regs, doors, 1, interrupt, FALLBACK, &mut alloc, &mut |_| {})
    }

    /// Where offset `offset` of page list `list` is.
    fn page(state: &State, list: u32, offset: u64) -> u64 {
        state.lists[&list][(offset / PAGE as u64) as usize] + offset % PAGE as u64
    }

    /// An event on notification block `block`.
    fn event(state: &mut State, block: usize) {
        let it = &mut state.blocks[block];
        if !it.masked && it.armed {
            it.armed = false;
            state.interrupts += 1;
        } else {
            it.pending = true;
        }
    }

    fn admin(state: &mut State, command: &[u8]) -> u32 {
        let opcode = be32(command, 0);
        if state.refuses == Some(opcode) {
            return 0xffff_fff7;
        }
        match opcode {
            0xf => {
                if state.compat_unimplemented {
                    return 0xffff_fffe;
                }
                assert_eq!(be64(command, 8), 304, "the driver description's length");
                state.capabilities = Some(be64(&read(be64(command, 16), 24), 16));
            }
            0x1 => {
                assert_eq!(be32(command, 16), 1, "version 1 of the description");
                let mut out = std::vec![0u8; 40];
                out[0..8].copy_from_slice(&4096u64.to_be_bytes());
                out[10..12].copy_from_slice(&state.tx_entries.to_be_bytes());
                out[12..14].copy_from_slice(&state.rx_entries.to_be_bytes());
                out[14..16].copy_from_slice(&1u16.to_be_bytes());
                out[16..18].copy_from_slice(&1500u16.to_be_bytes());
                out[18..20].copy_from_slice(&64u16.to_be_bytes());
                out[22..24].copy_from_slice(&state.rx_pages.to_be_bytes());
                out[24..30].copy_from_slice(&MAC);
                out[30..32].copy_from_slice(&(state.options.len() as u16).to_be_bytes());
                for (id, data) in &state.options {
                    out.extend_from_slice(&id.to_be_bytes());
                    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
                    out.extend_from_slice(&0u32.to_be_bytes());
                    out.extend_from_slice(data);
                }
                let total = out.len() as u16;
                out[32..34].copy_from_slice(&total.to_be_bytes());
                assert!(out.len() <= be32(command, 20) as usize);
                fake_memory_write(be64(command, 8), &out);
            }
            0x2 => {
                state.counters = be64(command, 8);
                state.irq_dbs = be64(command, 16);
                assert_eq!(be32(command, 24), 64, "every counter");
                assert_eq!(be32(command, 28), QUEUES * 2, "a block for each queue");
                assert_eq!(be32(command, 32), 64, "the blocks a cache line apart");
                state.format = command[40];
                for block in 0..QUEUES * 2 {
                    write_be32(state.irq_dbs + u64::from(block) * 64, IRQ_DOORBELLS + block);
                }
            }
            0x3 => {
                let (id, count) = (be32(command, 8), be32(command, 12) as usize);
                assert_eq!(be64(command, 24), PAGE as u64);
                let list = read(be64(command, 16), count * 8);
                let pages = (0..count).map(|i| be64(&list, i * 8)).collect();
                state.lists.insert(id, pages);
            }
            0x5 => {
                let resources = be64(command, 16);
                assert_eq!(be32(command, 36), 0, "notification block 0");
                state.tx = Some((be64(command, 24), be32(command, 32), be16(command, 48)));
                write_be32(resources, TX_DOORBELL);
                write_be32(resources + 4, TX_COUNTER);
            }
            0x6 => {
                assert_eq!(be32(command, 20), QUEUES, "the block after the transmit queues'");
                assert_eq!(be16(command, 54), 2048, "2 KiB buffers");
                let resources = be64(command, 24);
                state.rx = Some((
                    be64(command, 32),
                    be64(command, 40),
                    be32(command, 48),
                    be16(command, 52),
                    0,
                ));
                write_be32(resources, RX_DOORBELL);
                write_be32(resources + 4, 6);
            }
            0xb => {
                assert_eq!(be32(command, 8), 1, "the MTU");
                state.mtu = Some(be64(command, 16));
            }
            _ => return 0xffff_fffe,
        }
        1
    }

    fn run_admin(state: &mut State, upto: u32) {
        while state.events < upto {
            assert!(state.running && state.admin_length == 4096, "the admin queue given");
            let at = state.admin + u64::from(state.events % 64) * 64;
            let command = read(at, 64);
            let status = Card::admin(state, &command);
            write_be32(at + 4, status);
            state.events += 1;
        }
    }

    /// Takes the frames sent up to the transmit doorbell's count.
    fn transmit(state: &mut State, upto: u32) {
        let (ring, list, size) = state.tx.expect("the transmit queue");
        while !state.stalled && state.tx_head != upto {
            let at = ring + u64::from(state.tx_head % u32::from(size)) * 16;
            let descriptor = read(at, 16);
            assert_eq!(descriptor[0], 0x00, "a packet's first descriptor");
            let count = descriptor[3];
            let len = usize::from(be16(&descriptor, 4));
            let mut frame = read(
                Card::page(state, list, be64(&descriptor, 8)),
                usize::from(be16(&descriptor, 6)),
            );
            if count == 2 {
                let at = ring + u64::from((state.tx_head + 1) % u32::from(size)) * 16;
                let segment = read(at, 16);
                assert_eq!(segment[0], 0x20, "a segment's descriptor");
                let more = read(
                    Card::page(state, list, be64(&segment, 8)),
                    usize::from(be16(&segment, 6)),
                );
                frame.extend_from_slice(&more);
            }
            assert_eq!(frame.len(), len);
            state.sent.push(frame);
            state.descriptors.push(count);
            state.tx_head = state.tx_head.wrapping_add(u32::from(count));
            state.tx_counter = state.tx_counter.wrapping_add(u32::from(count));
            write_be32(state.counters + u64::from(TX_COUNTER) * 4, state.tx_counter);
            Card::event(state, 0);
        }
    }

    /// Writes `frame` into the next receive buffer with the flags given:
    /// whether there was one.
    fn deliver_with(&self, frame: &[u8], flags: u16) -> bool {
        let mut state = self.state();
        let (ring, data, list, size, _) = state.rx.expect("the receive queue");
        if state.rx_written == state.rx_filled {
            return false;
        }
        let slot = state.rx_written % u32::from(size);
        let offset = be64(&read(data + u64::from(slot) * 8, 8), 0);
        fake_memory_write(Card::page(&state, list, offset) + 2, frame);
        let sequence = (state.rx_written % 7 + 1) as u16;
        let mut descriptor = std::vec![0u8; 64];
        descriptor[60..62].copy_from_slice(&(frame.len() as u16 + 2).to_be_bytes());
        descriptor[62..64].copy_from_slice(&(sequence | flags).to_be_bytes());
        fake_memory_write(ring + u64::from(slot) * 64, &descriptor);
        state.rx_written += 1;
        Card::event(&mut state, QUEUES as usize);
        true
    }

    fn deliver(&self, frame: &[u8]) -> bool {
        self.deliver_with(frame, 0)
    }
}

impl Registers for Bar {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert!(!self.1, "a doorbell read");
        assert_eq!(size, 4);
        let state = self.0.state();
        let value = match offset {
            DEVICE_STATUS => state.status | u32::from(!state.running) << 4,
            MAX_TX_QUEUES | MAX_RX_QUEUES => QUEUES,
            ADMINQ_EVENT_COUNTER => state.events,
            _ => panic!("read register {offset}"),
        };
        u64::from(value.to_be())
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut state = self.0.state();
        if !self.1 && offset == DRIVER_VERSION {
            assert_eq!(size, 1);
            state.version.push(value as u8);
            return;
        }
        if !self.1 && offset == ADMINQ_LENGTH {
            assert_eq!(size, 2);
            state.admin_length = u16::from_be(value as u16);
            return;
        }
        assert_eq!(size, 4);
        let value = u32::from_be(value as u32);
        if self.1 {
            let index = (offset / 4) as u32;
            match index {
                TX_DOORBELL => Card::transmit(&mut state, value),
                RX_DOORBELL => state.rx_filled = value,
                _ if (IRQ_DOORBELLS..IRQ_DOORBELLS + 4).contains(&index) => {
                    let block = (index - IRQ_DOORBELLS) as usize;
                    state.irq_writes.push((block, value));
                    let it = &mut state.blocks[block];
                    if value == IRQ_MASK {
                        it.masked = true;
                    } else if value == 0 || value == IRQ_ACK | IRQ_EVENT {
                        it.masked = false;
                        it.armed = true;
                        if std::mem::take(&mut it.pending) {
                            it.armed = false;
                            state.interrupts += 1;
                        }
                    } else {
                        panic!("wrote {value:#x} to block {block}'s doorbell");
                    }
                }
                _ => panic!("rang doorbell {index:#x}"),
            }
            return;
        }
        match offset {
            ADMINQ_PFN => panic!("the admin queue's page number, past 32 bits"),
            ADMINQ_BASE_ADDRESS_HI => state.admin_high = value,
            ADMINQ_BASE_ADDRESS_LO => {
                state.admin = u64::from(state.admin_high) << 32 | u64::from(value);
            }
            DRIVER_STATUS if value == 1 => state.running = true,
            DRIVER_STATUS if value == 2 => {
                // The card lets go of everything it was given.
                state.running = false;
                state.resets += 1;
                state.events = 0;
                state.lists.clear();
                state.tx = None;
                state.rx = None;
            }
            ADMINQ_DOORBELL => Card::run_admin(&mut state, value),
            _ => panic!("wrote register {offset}"),
        }
    }
}

fn frame(n: u8, len: usize) -> Vec<u8> {
    let mut frame = std::vec![n; len];
    frame[..6].copy_from_slice(&MAC);
    frame
}

#[test]
fn identifies_its_device() {
    assert_eq!((VENDOR, DEVICE), (0x1ae0, 0x0042));
}

#[test]
fn starts_in_the_gqi_qpl_format() {
    let card = Card::new();
    let gve = card.start(Interrupt::Polled).unwrap();
    let info = gve.info();
    assert_eq!((info.model, info.mac), ("gVNIC", MAC));
    // A 256-descriptor ring has 16 transmit pages, two frames to each.
    assert_eq!((info.receive_slots, info.transmit_slots), (256, 31));
    let state = card.state();
    assert_eq!(state.version, b"GVE-Dukovo-1.0.0\n");
    assert_eq!(state.capabilities, Some(1), "GQI-QPL alone");
    assert_eq!((state.format, state.mtu), (2, Some(1500)));
    assert_eq!(state.lists[&0].len(), 16);
    assert_eq!(state.lists[&2].len(), 256);
    assert_eq!(state.rx_filled, 256, "every receive buffer given");
    assert!(state.blocks[0].masked && state.blocks[2].masked, "polled");
}

#[test]
fn moves_frames_both_ways() {
    let card = Card::new();
    let mut gve = card.start(Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    for round in 0..600u32 {
        let n = round as u8;
        let len = 60 + (round as usize * 7) % 1450;
        assert!(card.deliver(&frame(n, len)), "round {round}");
        assert_eq!(gve.receive(&mut buffer), Some(len));
        assert_eq!(buffer[..len], frame(n, len)[..]);
        gve.flush();
        assert!(gve.send(&frame(n, len)));
        gve.flush();
        assert_eq!(card.state().sent.last().unwrap(), &frame(n, len));
    }
    assert_eq!(gve.receive(&mut buffer), None);
    assert_eq!(gve.dropped(), 0);
    let state = card.state();
    assert_eq!(state.sent.len(), 600);
    // Frames longer than the first descriptor holds took a second.
    assert!(state.descriptors.contains(&1) && state.descriptors.contains(&2));
}

#[test]
fn transmit_waits_for_slots() {
    let card = Card::new();
    let mut gve = card.start(Interrupt::Polled).unwrap();
    card.state().stalled = true;
    for n in 0..31 {
        assert!(gve.send(&frame(n, 1000)), "frame {n}");
    }
    assert!(!gve.can_send());
    gve.flush();
    {
        let mut state = card.state();
        state.stalled = false;
        Card::transmit(&mut state, 62);
        assert_eq!(state.sent.len(), 31);
    }
    assert!(gve.can_send());
    assert!(!gve.send(&[0; 13]), "too short");
}

#[test]
fn damaged_frames_are_dropped() {
    let card = Card::new();
    let mut gve = card.start(Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    assert!(card.deliver_with(&frame(1, 100), 1 << 11));
    assert!(card.deliver_with(&frame(2, 1000), 1 << 13));
    assert!(card.deliver(&frame(2, 1000)));
    assert!(card.deliver(&[0; 8]));
    assert!(card.deliver(&frame(3, 64)));
    assert_eq!(gve.receive(&mut buffer), Some(64));
    assert_eq!(gve.dropped(), 3, "the error, the frame in two buffers and the short one");
    gve.flush();
    assert_eq!(card.state().rx_filled, 261, "their buffers given back");
}

#[test]
fn with_an_interrupt_it_acknowledges_once_nothing_is_left() {
    let card = Card::new();
    let mut gve = card.start(Interrupt::Msi).unwrap();
    let mut buffer = [0u8; 2048];
    assert!(card.deliver(&frame(1, 64)));
    assert_eq!(card.state().interrupts, 1);
    assert!(card.deliver(&frame(2, 64)));
    assert_eq!(card.state().interrupts, 1, "one until acknowledged");
    assert_eq!(gve.receive(&mut buffer), Some(64));
    assert_eq!(gve.receive(&mut buffer), Some(64));
    assert_eq!(gve.receive(&mut buffer), None);
    let state = card.state();
    assert_eq!(state.interrupts, 2, "the pending one, once acknowledged");
    assert!(state.irq_writes.contains(&(2, IRQ_ACK | IRQ_EVENT)));
    drop(state);

    let card = Card::new();
    let mut gve = card.start(Interrupt::Polled).unwrap();
    assert!(card.deliver(&frame(1, 64)));
    assert_eq!(gve.receive(&mut buffer), Some(64));
    assert_eq!(gve.receive(&mut buffer), None);
    let state = card.state();
    assert_eq!(state.interrupts, 0);
    assert!(state.irq_writes.iter().all(|&(_, value)| value == IRQ_MASK));
}

#[test]
fn the_link_and_resets_come_from_its_status() {
    let card = Card::new();
    let gve = card.start(Interrupt::Polled).unwrap();
    assert!(gve.link().up);
    card.state().status = 0;
    assert!(!gve.link().up);
    assert!(!gve.failed());
    card.state().status = 1 << 1;
    assert!(gve.failed());
}

#[test]
fn a_card_without_gqi_qpl_is_refused() {
    let card = Card::new();
    card.state().options = std::vec![(4, std::vec![0; 8])];
    assert_eq!(
        card.start(Interrupt::Polled).err(),
        Some(Error::Unsupported("the GQI-QPL queue format"))
    );
    let state = card.state();
    assert_eq!(state.resets, 1, "its admin queue taken back");
}

#[test]
fn it_starts_without_the_compatibility_check() {
    let card = Card::new();
    card.state().compat_unimplemented = true;
    assert!(card.start(Interrupt::Polled).is_ok());
}

#[test]
fn a_refused_command_fails_its_start() {
    let card = Card::new();
    card.state().refuses = Some(0x5);
    assert_eq!(
        card.start(Interrupt::Polled).err(),
        Some(Error::Device("it refused an admin command"))
    );
    assert_eq!(card.state().resets, 1);
}

#[test]
fn rings_it_cannot_drive_are_refused() {
    let card = Card::new();
    card.state().rx_entries = 8192;
    assert_eq!(card.start(Interrupt::Polled).err(), Some(Error::Unsupported("its ring sizes")));
}

#[test]
fn fewer_receive_pages_shrink_the_ring() {
    let card = Card::new();
    card.state().rx_pages = 128;
    let gve = card.start(Interrupt::Polled).unwrap();
    assert_eq!(gve.info().receive_slots, 128);
}

#[test]
fn stopping_takes_the_admin_queue_back() {
    let card = Card::new();
    let mut gve = card.start(Interrupt::Msi).unwrap();
    gve.stop();
    {
        let state = card.state();
        assert_eq!(state.resets, 1);
        assert!(state.blocks[0].masked && state.blocks[2].masked);
    }
    assert!(!gve.send(&frame(1, 64)));
    drop(gve);
    assert_eq!(card.state().resets, 1, "taken back once");
}

#[test]
fn an_admin_queue_past_its_page_numbers_needs_revision_1() {
    let card = Card::new();
    let regs = Mmio::fake(Arc::new(Bar(card.clone(), false)), 64);
    let doors = Mmio::fake(Arc::new(Bar(card.clone(), true)), DOORS);
    let started = start(regs, doors, 0, Interrupt::Polled, FALLBACK, &mut alloc, &mut |_| {});
    assert_eq!(started.err(), Some(Error::Unsupported("an admin queue above 16 TiB")));
}
