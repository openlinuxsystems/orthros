// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated ENA, which acts as ena-com expects the
//! card to: it answers register reads by DMA in readless mode, runs admin
//! commands as their doorbell rings, takes descriptors up to each
//! submission queue's tail, writes completions with their phase, and
//! masks the queue pair's interrupt as it raises it.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};
use radix_nic::{Error, Interrupt, Nic, PAGE};

use crate::{Ena, identify, start};

const MAC: [u8; 6] = [0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f];
const FALLBACK: [u8; 6] = [0x02, 0, 0, 0, 0, 1];

const VERSION: usize = 0x00;
const CONTROLLER_VERSION: usize = 0x04;
const CAPS: usize = 0x08;
const AQ_BASE_LO: usize = 0x10;
const AQ_BASE_HI: usize = 0x14;
const AQ_CAPS: usize = 0x18;
const ACQ_BASE_LO: usize = 0x20;
const ACQ_BASE_HI: usize = 0x24;
const ACQ_CAPS: usize = 0x28;
const AQ_DB: usize = 0x2c;
const AENQ_CAPS: usize = 0x34;
const AENQ_BASE_LO: usize = 0x38;
const AENQ_BASE_HI: usize = 0x3c;
const AENQ_HEAD_DB: usize = 0x40;
const INTR_MASK: usize = 0x4c;
const DEV_CTL: usize = 0x54;
const DEV_STS: usize = 0x58;
const MMIO_REG_READ: usize = 0x5c;
const MMIO_RESP_LO: usize = 0x60;
const MMIO_RESP_HI: usize = 0x64;
/// Where the simulated card puts each queue's doorbells: submission
/// queues', completion queues' unmask registers and their heads'.
const SQ_DOORBELLS: usize = 0x1000;
const UNMASKS: usize = 0x1800;
const HEADS: usize = 0x1c00;
const BAR: usize = 0x2000;

const LINK_CHANGE: u16 = 0;
const FATAL_ERROR: u16 = 1;
const DEVICE_REQUEST_RESET: u16 = 7;

fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * PAGE))
}

fn read(phys: u64, len: usize) -> Vec<u8> {
    let mut out = std::vec![0u8; len];
    fake_memory(phys, &mut out);
    out
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn address(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32_at(bytes, at)) | u64::from(u16_at(bytes, at + 4)) << 32
}

/// A queue as the card goes round it.
#[derive(Clone, Copy)]
struct Queue {
    base: u64,
    depth: usize,
    /// Entries taken or written so far.
    count: usize,
    entry: usize,
}

impl Queue {
    fn new(base: u64, depth: usize, entry: usize) -> Queue {
        Queue { base, depth, count: 0, entry }
    }

    fn at(&self) -> u64 {
        self.base + ((self.count % self.depth) * self.entry) as u64
    }

    /// The phase the current entry takes.
    fn phase(&self) -> bool {
        (self.count / self.depth).is_multiple_of(2)
    }
}

struct Cq {
    queue: Queue,
    interrupt: bool,
    vector: u32,
    words: u8,
    /// The head the driver last wrote to its doorbell.
    head: Option<u32>,
}

struct Sq {
    queue: Queue,
    direction: u8,
    placement: u8,
    cq: usize,
    /// The tail the driver last wrote.
    tail: usize,
}

struct State {
    readless: bool,
    /// Whether it never answers register reads by DMA.
    silent: bool,
    ready: bool,
    resetting: bool,
    /// Every value DEV_CTL was written.
    controls: Vec<u32>,
    response: u64,
    response_low: u32,
    aq: Option<Queue>,
    acq: Option<Queue>,
    aenq: Option<Queue>,
    aq_low: u32,
    acq_low: u32,
    aenq_low: u32,
    aq_base: u64,
    acq_base: u64,
    aenq_base: u64,
    admin_masked: bool,
    aenq_heads: Vec<u32>,
    supported: u32,
    max_depth: u32,
    mtu: Option<u32>,
    groups: u32,
    /// An opcode the card refuses.
    refuses: Option<u8>,
    opcodes: Vec<u8>,
    cqs: Vec<Cq>,
    sqs: Vec<Sq>,
    masked: bool,
    pending: bool,
    interrupts: u32,
    unmasks: u32,
    sent: Vec<Vec<u8>>,
    /// Whether the transmit queue takes nothing.
    stalled: bool,
}

struct Card {
    state: Mutex<State>,
}

impl Card {
    fn new() -> Arc<Card> {
        Arc::new(Card {
            state: Mutex::new(State {
                readless: true,
                silent: false,
                ready: true,
                resetting: false,
                controls: Vec::new(),
                response: 0,
                response_low: 0,
                aq: None,
                acq: None,
                aenq: None,
                aq_low: 0,
                acq_low: 0,
                aenq_low: 0,
                aq_base: 0,
                acq_base: 0,
                aenq_base: 0,
                admin_masked: false,
                aenq_heads: Vec::new(),
                // Device attributes, the extended queue maxima, the MTU,
                // the event queue's groups and the link.
                supported: 1 << 1 | 1 << 2 | 1 << 7 | 1 << 14 | 1 << 26 | 1 << 27,
                max_depth: 1024,
                mtu: None,
                groups: 0,
                refuses: None,
                opcodes: Vec::new(),
                cqs: Vec::new(),
                sqs: Vec::new(),
                masked: true,
                pending: false,
                interrupts: 0,
                unmasks: 0,
                sent: Vec::new(),
                stalled: false,
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn mmio(self: &Arc<Card>) -> Mmio {
        Mmio::fake(self.clone(), BAR)
    }

    fn start(self: &Arc<Card>, revision: u8, interrupt: Interrupt) -> Result<Ena, Error> {
        start(self.mmio(), revision, interrupt, FALLBACK, &mut alloc, &mut |_| {})
    }

    /// The queue of the direction given: 1 transmit, 2 receive.
    fn sq(state: &State, direction: u8) -> usize {
        state.sqs.iter().position(|sq| sq.direction == direction).unwrap()
    }

    /// Writes a completion for `sq` to its completion queue, `bytes` of
    /// it with the phase bit put in, and raises the interrupt.
    fn complete(state: &mut State, cq: usize, mut entry: Vec<u8>, phase_at: (usize, u8)) {
        let queue = state.cqs[cq].queue;
        if queue.phase() {
            entry[phase_at.0] |= phase_at.1;
        }
        fake_memory_write(queue.at(), &entry);
        state.cqs[cq].queue.count += 1;
        if state.cqs[cq].interrupt {
            if state.masked {
                state.pending = true;
            } else {
                state.masked = true;
                state.interrupts += 1;
            }
        }
    }

    /// Takes the frames sent up to the transmit queue's tail.
    fn transmit(state: &mut State) {
        let index = Card::sq(state, 1);
        while !state.stalled && state.sqs[index].queue.count < state.sqs[index].tail {
            let queue = state.sqs[index].queue;
            let descriptor = read(queue.at(), 16);
            let control = u32_at(&descriptor, 0);
            assert_eq!(control & 7 << 26, 7 << 26, "first, last and a completion asked for");
            assert_eq!(control & 1 << 24 != 0, queue.phase(), "the transmit descriptor's phase");
            let id = (u32_at(&descriptor, 4) >> 22) | ((control >> 16) & 0x3f) << 10;
            let at = address(&descriptor, 8);
            assert_eq!(u32_at(&descriptor, 12) >> 16, 0, "no header in the descriptor");
            state.sent.push(read(at, (control & 0xffff) as usize));
            state.sqs[index].queue.count += 1;
            let mut entry = std::vec![0u8; 8];
            entry[0..2].copy_from_slice(&(id as u16).to_le_bytes());
            let cq = state.sqs[index].cq;
            Card::complete(state, cq, entry, (3, 1));
        }
    }

    /// Writes `frame` into the next receive buffer, `offset` bytes in,
    /// with its completion's first and last bits as given: whether there
    /// was a buffer.
    fn deliver_as(&self, frame: &[u8], offset: u8, first: bool, last: bool) -> bool {
        let mut state = self.state();
        let index = Card::sq(&state, 2);
        let sq = &state.sqs[index];
        if sq.queue.count == sq.tail {
            return false;
        }
        let descriptor = read(sq.queue.at(), 16);
        assert_eq!(descriptor[3] & 0x1c, 0x1c, "first, last and a completion asked for");
        assert_eq!(descriptor[3] & 1 != 0, sq.queue.phase(), "the receive descriptor's phase");
        assert_eq!(usize::from(u16_at(&descriptor, 0)), radix_nic::SLOT);
        let id = u16_at(&descriptor, 4);
        fake_memory_write(address(&descriptor, 8) + u64::from(offset), frame);
        let cq = sq.cq;
        state.sqs[index].queue.count += 1;
        let mut entry = std::vec![0u8; 16];
        let status = u32::from(first) << 26 | u32::from(last) << 27;
        entry[0..4].copy_from_slice(&status.to_le_bytes());
        entry[4..6].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        entry[6..8].copy_from_slice(&id.to_le_bytes());
        entry[14] = offset;
        Card::complete(&mut state, cq, entry, (3, 1));
        true
    }

    fn deliver(&self, frame: &[u8]) -> bool {
        self.deliver_as(frame, 0, true, true)
    }

    /// Puts an event in the event queue.
    fn event(&self, group: u16, flags: u32) {
        let mut state = self.state();
        let queue = state.aenq.expect("the event queue");
        let mut entry = std::vec![0u8; 64];
        entry[0..2].copy_from_slice(&group.to_le_bytes());
        entry[4] = u8::from(queue.phase());
        entry[16..20].copy_from_slice(&flags.to_le_bytes());
        fake_memory_write(queue.at(), &entry);
        state.aenq.as_mut().unwrap().count += 1;
    }

    fn value(state: &State, offset: usize) -> u32 {
        match offset {
            VERSION => 0x0000_0002,
            CONTROLLER_VERSION => 0x0001_0001,
            // A reset in 500 ms, 48-bit DMA addresses.
            CAPS => 5 << 1 | 48 << 8,
            DEV_STS => u32::from(state.ready) | u32::from(state.resetting) << 3,
            _ => panic!("read register {offset:#x}"),
        }
    }

    fn run_admin(state: &mut State, tail: u32) {
        loop {
            let queue = state.aq.expect("the admin queue");
            if queue.count >= tail as usize {
                break;
            }
            let command = read(queue.at(), 64);
            assert_eq!(command[3] & 1 != 0, queue.phase(), "the admin command's phase");
            state.aq.as_mut().unwrap().count += 1;
            let id = u16_at(&command, 0);
            let mut entry = std::vec![0u8; 64];
            let status = Card::admin(state, &command, &mut entry);
            entry[0..2].copy_from_slice(&id.to_le_bytes());
            entry[2] = status;
            let acq = state.acq.as_mut().expect("the admin completion queue");
            entry[3] = u8::from(acq.phase());
            fake_memory_write(acq.at(), &entry);
            acq.count += 1;
        }
    }

    /// Runs one admin command, filling its completion: its status.
    fn admin(state: &mut State, command: &[u8], entry: &mut [u8]) -> u8 {
        let opcode = command[2];
        state.opcodes.push(opcode);
        if state.refuses == Some(opcode) {
            return 1;
        }
        let put = |entry: &mut [u8], at: usize, value: u32| {
            entry[at..at + 4].copy_from_slice(&value.to_le_bytes());
        };
        match opcode {
            8 => {
                let feature = command[17];
                if feature != 1 && state.supported & 1 << feature == 0 {
                    return 2;
                }
                match feature {
                    1 => {
                        put(entry, 16, state.supported);
                        entry[32..38].copy_from_slice(&MAC);
                        put(entry, 40, 9216);
                    }
                    2 => {
                        put(entry, 12, state.max_depth);
                        put(entry, 20, state.max_depth);
                    }
                    7 => {
                        assert_eq!(command[18], 1, "version 1 of the queue maxima");
                        entry[8] = 1;
                        for at in [28, 32, 36, 40] {
                            put(entry, at, state.max_depth);
                        }
                    }
                    // Every group but CONF_NOTIFICATIONS (6).
                    26 => put(entry, 8, 0x9f),
                    27 => {
                        put(entry, 8, 25_000);
                        put(entry, 16, 1 << 1);
                    }
                    _ => return 2,
                }
            }
            9 => match command[17] {
                14 => state.mtu = Some(u32_at(command, 20)),
                26 => state.groups = u32_at(command, 24),
                _ => return 2,
            },
            3 => {
                let depth = usize::from(u16_at(command, 6));
                let words = command[5] & 0x1f;
                state.cqs.push(Cq {
                    queue: Queue::new(address(command, 12), depth, usize::from(words) * 4),
                    interrupt: command[4] & 1 << 5 != 0,
                    vector: u32_at(command, 8),
                    words,
                    head: None,
                });
                let index = (state.cqs.len() - 1) as u16;
                entry[8..10].copy_from_slice(&index.to_le_bytes());
                entry[10..12].copy_from_slice(&(depth as u16).to_le_bytes());
                put(entry, 16, (HEADS + usize::from(index) * 8) as u32);
                put(entry, 20, (UNMASKS + usize::from(index) * 8) as u32);
            }
            1 => {
                assert_eq!(command[7] & 1, 1, "physically contiguous");
                assert_eq!(command[6] >> 4, 0, "a completion for each descriptor");
                state.sqs.push(Sq {
                    queue: Queue::new(address(command, 12), usize::from(u16_at(command, 10)), 16),
                    direction: command[4] >> 5,
                    placement: command[6] & 0xf,
                    cq: usize::from(u16_at(command, 8)),
                    tail: 0,
                });
                let index = state.sqs.len() - 1;
                put(entry, 12, (SQ_DOORBELLS + index * 8) as u32);
                entry[8..10].copy_from_slice(&(index as u16).to_le_bytes());
            }
            _ => return 3,
        }
        0
    }
}

impl Registers for Card {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4);
        let state = self.state();
        assert!(!state.readless, "register {offset:#x} read directly in readless mode");
        u64::from(Card::value(&state, offset))
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4);
        let value = value as u32;
        let mut state = self.state();
        match offset {
            DEV_CTL => {
                state.controls.push(value);
                if value & 1 != 0 {
                    // The card forgets its queues and where its register
                    // answers go.
                    state.resetting = true;
                    state.response = 0;
                    state.aq = None;
                    state.acq = None;
                    state.aenq = None;
                    state.cqs.clear();
                    state.sqs.clear();
                    state.masked = true;
                } else {
                    state.resetting = false;
                }
            }
            MMIO_RESP_LO => state.response_low = value,
            MMIO_RESP_HI => {
                state.response = u64::from(state.response_low) | u64::from(value) << 32;
            }
            MMIO_REG_READ => {
                if state.response != 0 && !state.silent {
                    let register = (value >> 16) as usize;
                    let mut answer = [0u8; 8];
                    answer[0..2].copy_from_slice(&(value as u16).to_le_bytes());
                    answer[2..4].copy_from_slice(&(register as u16).to_le_bytes());
                    answer[4..8].copy_from_slice(&Card::value(&state, register).to_le_bytes());
                    fake_memory_write(state.response, &answer);
                }
            }
            AQ_BASE_LO => state.aq_low = value,
            AQ_BASE_HI => state.aq_base = u64::from(state.aq_low) | u64::from(value) << 32,
            ACQ_BASE_LO => state.acq_low = value,
            ACQ_BASE_HI => state.acq_base = u64::from(state.acq_low) | u64::from(value) << 32,
            AENQ_BASE_LO => state.aenq_low = value,
            AENQ_BASE_HI => state.aenq_base = u64::from(state.aenq_low) | u64::from(value) << 32,
            AQ_CAPS => {
                assert_eq!(value >> 16, 64, "64-byte commands");
                state.aq = Some(Queue::new(state.aq_base, (value & 0xffff) as usize, 64));
            }
            ACQ_CAPS => {
                state.acq = Some(Queue::new(state.acq_base, (value & 0xffff) as usize, 64));
            }
            AENQ_CAPS => {
                state.aenq = Some(Queue::new(state.aenq_base, (value & 0xffff) as usize, 64));
            }
            INTR_MASK => state.admin_masked = value & 1 != 0,
            AQ_DB => Card::run_admin(&mut state, value),
            AENQ_HEAD_DB => state.aenq_heads.push(value),
            _ if (SQ_DOORBELLS..UNMASKS).contains(&offset) => {
                let index = (offset - SQ_DOORBELLS) / 8;
                state.sqs[index].tail = value as usize;
                if state.sqs[index].direction == 1 {
                    Card::transmit(&mut state);
                }
            }
            _ if (UNMASKS..HEADS).contains(&offset) => {
                assert_eq!(value, 1 << 30, "an unmask with no delay");
                state.unmasks += 1;
                state.masked = false;
                if state.pending {
                    state.pending = false;
                    state.masked = true;
                    state.interrupts += 1;
                }
            }
            _ if (HEADS..BAR).contains(&offset) => {
                state.cqs[(offset - HEADS) / 8].head = Some(value);
            }
            _ => panic!("wrote {value:#x} to register {offset:#x}"),
        }
    }
}

fn frame(n: u8, len: usize) -> Vec<u8> {
    let mut frame = std::vec![n; len];
    frame[..6].copy_from_slice(&MAC);
    frame
}

#[test]
fn identifies_its_devices() {
    for device in [0x0ec2, 0x1ec2, 0xec20, 0xec21] {
        assert!(identify(0x1d0f, device));
    }
    assert!(!identify(0x1d0f, 0x1234));
    assert!(!identify(0x8086, 0xec20));
}

#[test]
fn starts_in_readless_mode() {
    let card = Card::new();
    let ena = card.start(0, Interrupt::Polled).unwrap();
    let info = ena.info();
    assert_eq!((info.model, info.mac), ("ENA", MAC));
    assert_eq!((info.receive_slots, info.transmit_slots), (255, 255));
    let state = card.state();
    assert_eq!(state.controls, [1, 0], "one reset");
    assert_ne!(state.response, 0);
    assert!(state.admin_masked, "admin commands are polled");
    assert_eq!(state.mtu, Some(1500));
    assert_eq!(state.groups, 0x8f, "link, errors, warnings, notices and reset requests");
    assert_eq!(state.aenq_heads, [16]);
    let tx = &state.sqs[Card::sq(&state, 1)];
    let rx = &state.sqs[Card::sq(&state, 2)];
    assert_eq!((tx.placement, rx.placement), (1, 1), "descriptors in host memory");
    assert_eq!((state.cqs[tx.cq].words, state.cqs[rx.cq].words), (2, 4));
    assert!(state.cqs.iter().all(|cq| cq.interrupt && cq.vector == 1));
    assert_eq!(rx.tail, 255, "every receive buffer but one given");
}

#[test]
fn reads_registers_directly_when_its_revision_says() {
    let card = Card::new();
    card.state().readless = false;
    let ena = card.start(1, Interrupt::Polled).unwrap();
    assert_eq!(ena.info().mac, MAC);
    assert_eq!(card.state().response, 0, "no register answers by DMA");
}

#[test]
fn moves_frames_both_ways() {
    let card = Card::new();
    let mut ena = card.start(0, Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    // More than a queue's worth each way, so that every slot comes back.
    for round in 0..600u32 {
        let n = round as u8;
        assert!(card.deliver(&frame(n, 60 + (round as usize % 1400))));
        assert_eq!(ena.receive(&mut buffer), Some(60 + (round as usize % 1400)));
        assert_eq!(&buffer[6..10], &[n; 4]);
        assert!(ena.send(&frame(n, 100)));
        ena.flush();
        assert_eq!(card.state().sent.last().unwrap(), &frame(n, 100));
    }
    assert_eq!(ena.receive(&mut buffer), None);
    assert_eq!(card.state().sent.len(), 600);
    assert_eq!(ena.dropped(), 0);
    // The card was told how far its completions were taken.
    let state = card.state();
    assert!(state.cqs.iter().all(|cq| cq.head.is_some_and(|head| head >= 512)));
}

#[test]
fn transmit_waits_for_slots() {
    let card = Card::new();
    let mut ena = card.start(0, Interrupt::Polled).unwrap();
    card.state().stalled = true;
    for n in 0..255 {
        assert!(ena.send(&frame(n as u8, 64)), "frame {n}");
    }
    assert!(!ena.can_send());
    assert!(!ena.send(&frame(0, 64)));
    ena.flush();
    {
        let mut state = card.state();
        state.stalled = false;
        Card::transmit(&mut state);
        assert_eq!(state.sent.len(), 255);
    }
    assert!(ena.can_send());
    assert!(!ena.send(&[0; 10]), "too short");
    assert!(!ena.send(&[0; 1519]), "too long");
}

#[test]
fn queues_shrink_to_the_cards() {
    let card = Card::new();
    {
        let mut state = card.state();
        state.max_depth = 100;
        // Only the older queue maxima.
        state.supported &= !(1 << 7);
    }
    let ena = card.start(0, Interrupt::Polled).unwrap();
    assert_eq!((ena.info().receive_slots, ena.info().transmit_slots), (63, 63));
    assert!(!card.state().opcodes.is_empty());
}

#[test]
fn too_small_queues_are_refused() {
    let card = Card::new();
    card.state().max_depth = 8;
    assert_eq!(
        card.start(0, Interrupt::Polled).err(),
        Some(Error::Unsupported("queues of 16 descriptors"))
    );
    assert_eq!(card.state().controls, [1, 0, 1, 0], "reset, and reset again to stop it");
}

#[test]
fn the_link_comes_from_events() {
    let card = Card::new();
    let mut ena = card.start(0, Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    assert!(!ena.link().up, "down until the card says");
    card.event(LINK_CHANGE, 1);
    assert_eq!(ena.receive(&mut buffer), None);
    let link = ena.link();
    assert_eq!((link.up, link.mbps, link.full_duplex), (true, 25_000, true));
    assert_eq!(card.state().aenq_heads, [16, 17]);
    // Events past the queue's wrap.
    for _ in 0..12 {
        card.event(LINK_CHANGE, 1);
    }
    assert_eq!(ena.receive(&mut buffer), None);
    for _ in 0..8 {
        card.event(LINK_CHANGE, 1);
    }
    card.event(LINK_CHANGE, 0);
    assert_eq!(ena.receive(&mut buffer), None);
    assert!(!ena.link().up);
    assert_eq!(card.state().aenq_heads, [16, 17, 29, 38]);
    assert!(!ena.failed());
}

#[test]
fn fatal_events_fail_it() {
    for group in [FATAL_ERROR, DEVICE_REQUEST_RESET] {
        let card = Card::new();
        let mut ena = card.start(0, Interrupt::Polled).unwrap();
        let mut buffer = [0u8; 2048];
        card.event(group, 0);
        assert_eq!(ena.receive(&mut buffer), None);
        assert!(ena.failed());
        assert!(card.deliver(&frame(1, 64)));
        assert_eq!(ena.receive(&mut buffer), None, "nothing more from a failed card");
        assert!(!ena.can_send());
    }
}

#[test]
fn refused_commands_fail_its_start() {
    let card = Card::new();
    card.state().refuses = Some(1);
    assert_eq!(
        card.start(0, Interrupt::Polled).err(),
        Some(Error::Device("it refused an admin command"))
    );
    assert_eq!(card.state().controls, [1, 0, 1, 0]);
}

#[test]
fn a_card_not_ready_is_refused() {
    let card = Card::new();
    card.state().ready = false;
    assert_eq!(card.start(0, Interrupt::Polled).err(), Some(Error::Device("it is not ready")));
}

#[test]
fn a_card_that_never_answers_reads_times_out() {
    let card = Card::new();
    card.state().silent = true;
    let mut waited = 0u32;
    let started =
        start(card.mmio(), 0, Interrupt::Polled, FALLBACK, &mut alloc, &mut |us| waited += us);
    assert_eq!(started.err(), Some(Error::Timeout("reading a register")));
    assert_eq!(waited, 200_000, "200 ms");
}

#[test]
fn with_an_interrupt_it_unmasks_once_nothing_is_left() {
    let card = Card::new();
    let mut ena = card.start(0, Interrupt::Msi).unwrap();
    let mut buffer = [0u8; 2048];
    assert_eq!(card.state().unmasks, 1);
    assert!(card.deliver(&frame(1, 64)));
    assert_eq!(card.state().interrupts, 1);
    assert!(card.deliver(&frame(2, 64)));
    assert_eq!(card.state().interrupts, 1, "masked as it was raised");
    assert_eq!(ena.receive(&mut buffer), Some(64));
    assert_eq!(ena.receive(&mut buffer), Some(64));
    assert_eq!(card.state().unmasks, 1);
    assert_eq!(ena.receive(&mut buffer), None);
    let state = card.state();
    assert_eq!((state.unmasks, state.interrupts), (2, 2), "the pending one");
    drop(state);

    let card = Card::new();
    let mut ena = card.start(0, Interrupt::Polled).unwrap();
    assert!(card.deliver(&frame(1, 64)));
    assert_eq!(ena.receive(&mut buffer), Some(64));
    assert_eq!(ena.receive(&mut buffer), None);
    assert_eq!(card.state().unmasks, 0, "polled, never unmasked");
}

#[test]
fn damaged_frames_are_dropped_and_their_buffers_given_back() {
    let card = Card::new();
    let mut ena = card.start(0, Interrupt::Polled).unwrap();
    let mut buffer = [0u8; 2048];
    assert!(card.deliver(&[0; 10]));
    assert!(card.deliver_as(&frame(1, 1000), 0, true, false));
    assert!(card.deliver_as(&frame(1, 1000), 0, false, true));
    assert!(card.deliver_as(&frame(3, 200), 2, true, true));
    assert_eq!(ena.receive(&mut buffer), Some(200));
    assert_eq!(buffer[..200], frame(3, 200)[..], "read from its offset");
    assert_eq!(ena.dropped(), 2, "the short frame and the one in two buffers");
    ena.flush();
    for n in 0..300u32 {
        assert!(card.deliver(&frame(n as u8, 64)), "buffer {n}");
        assert_eq!(ena.receive(&mut buffer), Some(64));
        ena.flush();
    }
}

#[test]
fn a_completion_for_no_frame_fails_it() {
    let card = Card::new();
    let mut ena = card.start(0, Interrupt::Polled).unwrap();
    {
        let mut state = card.state();
        let cq = state.sqs[Card::sq(&state, 1)].cq;
        Card::complete(&mut state, cq, std::vec![5, 0, 0, 0, 0, 0, 0, 0], (3, 1));
    }
    assert!(!ena.can_send());
    assert!(ena.failed());
}

#[test]
fn stopping_resets_it() {
    let card = Card::new();
    let mut ena = card.start(0, Interrupt::Msi).unwrap();
    let mut buffer = [0u8; 2048];
    ena.stop();
    assert_eq!(card.state().controls, [1, 0, 1, 0]);
    assert!(!ena.send(&frame(1, 64)));
    assert_eq!(ena.receive(&mut buffer), None);
    ena.stop();
    drop(ena);
    assert_eq!(card.state().controls, [1, 0, 1, 0], "stopped once");
}
