// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The driver against a simulated VMXNET3, which acts as QEMU's does: it
//! reads the driver-shared area when it is activated, takes descriptors
//! by their generation bits, writes a completion for each frame, and
//! masks its interrupt as it raises it, raising it again on an unmask if
//! something came meanwhile.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Dma, Mmio, Registers, fake_memory, fake_memory_write};
use radix_nic::{Error, Interrupt, Nic, PAGE};

use crate::{TRANSMIT, Vmxnet3, start};

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x64];
const FALLBACK: [u8; 6] = [0x02, 0, 0, 0, 0, 1];

const IMASK0: usize = 0x000;
const TXPROD0: usize = 0x600;
const RXPROD0: usize = 0x800;
const RXPROD2_0: usize = 0xa00;

const VRRS: usize = 0x000;
const UVRS: usize = 0x008;
const DSAL: usize = 0x010;
const DSAH: usize = 0x018;
const CMD: usize = 0x020;
const MACL: usize = 0x028;
const MACH: usize = 0x030;
const ECR: usize = 0x040;

const ACTIVATE: u32 = 0xcafe_0000;
const QUIESCE: u32 = 0xcafe_0001;
const RESET: u32 = 0xcafe_0002;
const UPDATE_RX_MODE: u32 = 0xcafe_0003;
const UPDATE_MAC_FILTERS: u32 = 0xcafe_0004;
const UPDATE_VLAN_FILTERS: u32 = 0xcafe_0005;
const GET_LINK: u32 = 0xf00d_0002;

const QUEUES: u64 = 1024;
const RXQ: u64 = QUEUES + 256;

fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * PAGE))
}

fn read(phys: u64, len: usize) -> Vec<u8> {
    let mut out = std::vec![0u8; len];
    fake_memory(phys, &mut out);
    out
}

fn u8_at(phys: u64) -> u8 {
    read(phys, 1)[0]
}

fn u16_at(phys: u64) -> u16 {
    u16::from_le_bytes(read(phys, 2).try_into().unwrap())
}

fn u32_at(phys: u64) -> u32 {
    u32::from_le_bytes(read(phys, 4).try_into().unwrap())
}

fn u64_at(phys: u64) -> u64 {
    u64::from_le_bytes(read(phys, 8).try_into().unwrap())
}

fn put32(phys: u64, value: u32) {
    fake_memory_write(phys, &value.to_le_bytes());
}

/// A ring as the card goes round it: where it is, how big it is, the
/// next descriptor and the generation it takes.
#[derive(Clone, Copy, Default)]
struct Ring {
    base: u64,
    size: usize,
    next: usize,
    generation: bool,
}

impl Ring {
    fn new(base: u64, size: u32) -> Ring {
        Ring { base, size: size as usize, next: 0, generation: true }
    }

    fn at(&self) -> u64 {
        self.base + (self.next * 16) as u64
    }

    fn advance(&mut self) {
        self.next += 1;
        if self.next == self.size {
            self.next = 0;
            self.generation = !self.generation;
        }
    }

    fn generation_bit(&self) -> u32 {
        u32::from(self.generation) << 31
    }
}

struct State {
    /// What VRRS reads as: the revisions it has.
    revisions: u32,
    mac: [u8; 6],
    temp_mac: u32,
    shared_low: u32,
    shared: u64,
    command: u32,
    active: bool,
    /// Whether activation fails.
    refuses: bool,
    link_up: bool,
    masked: bool,
    pending: bool,
    /// How many MSIs it sent.
    interrupts: u32,
    /// Whether its transmit side is stalled, taking nothing sent.
    stalled: bool,
    tx: Ring,
    tx_done: Ring,
    rx: Ring,
    rx_done: Ring,
    commands: Vec<u32>,
    rx_producer_writes: u32,
    sent: Vec<Vec<u8>>,
    acknowledged: u32,
}

struct Card {
    state: Mutex<State>,
}

/// BAR 0 and BAR 1 of the same card.
struct Bar0(Arc<Card>);
struct Bar1(Arc<Card>);

impl Card {
    fn new() -> Arc<Card> {
        Arc::new(Card {
            state: Mutex::new(State {
                revisions: 1,
                mac: MAC,
                temp_mac: 0,
                shared_low: 0,
                shared: 0,
                command: 0,
                active: false,
                refuses: false,
                link_up: true,
                masked: true,
                pending: false,
                interrupts: 0,
                stalled: false,
                tx: Ring::default(),
                tx_done: Ring::default(),
                rx: Ring::default(),
                rx_done: Ring::default(),
                commands: Vec::new(),
                rx_producer_writes: 0,
                sent: Vec::new(),
                acknowledged: 0,
            }),
        })
    }

    fn set(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
    }

    fn get<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        f(&self.state.lock().unwrap())
    }

    fn bars(self: &Arc<Card>) -> (Mmio, Mmio) {
        (
            Mmio::fake(Arc::new(Bar0(self.clone())), 0x1000),
            Mmio::fake(Arc::new(Bar1(self.clone())), 0x1000),
        )
    }

    /// Where the driver-shared area is.
    fn shared(&self) -> u64 {
        self.get(|state| state.shared)
    }

    /// Activation, as QEMU's: the area's magic, its one queue of each
    /// kind, and their rings.
    fn activate(state: &mut State) {
        let area = state.shared;
        if state.refuses || area == 0 || u32_at(area) != 0xbabe_fee1 {
            return;
        }
        assert_eq!(u32_at(area + 52), 512, "two queues' descriptions");
        assert_eq!(u32_at(area + 56), 1500, "the MTU");
        assert_eq!((u8_at(area + 62), u8_at(area + 63)), (1, 1), "a queue of each");
        let queues = u64_at(area + 40);
        assert_eq!(queues, area + QUEUES);
        let tx = queues;
        assert_ne!(u64_at(tx + 24), 0, "a data ring");
        assert_eq!(u32_at(tx + 60), u32_at(tx + 56), "as long as the command ring");
        state.tx = Ring::new(u64_at(tx + 16), u32_at(tx + 56));
        state.tx_done = Ring::new(u64_at(tx + 32), u32_at(tx + 64));
        let rx = queues + 256;
        assert_ne!(u64_at(rx + 24), 0, "a body ring");
        assert!(u32_at(rx + 64) >= u32_at(rx + 56) + u32_at(rx + 60), "room for every completion");
        state.rx = Ring::new(u64_at(rx + 16), u32_at(rx + 56));
        state.rx_done = Ring::new(u64_at(rx + 32), u32_at(rx + 64));
        state.active = true;
    }

    fn reset(state: &mut State) {
        state.active = false;
        state.masked = true;
        state.pending = false;
        state.shared = 0;
    }

    /// Raises the interrupt, which the card then masks, as QEMU's does
    /// with automasking and an MSI.
    fn raise(state: &mut State) {
        state.pending = true;
        Card::update_interrupt(state);
    }

    fn update_interrupt(state: &mut State) {
        if state.pending && !state.masked {
            state.interrupts += 1;
            state.pending = false;
            state.masked = true;
        }
    }

    /// Takes every frame handed over since, and completes each.
    fn transmit(state: &mut State) {
        if !state.active || state.stalled {
            return;
        }
        let mut any = false;
        loop {
            let at = state.tx.at();
            let word = u32_at(at + 8);
            if (word >> 14) & 1 != u32::from(state.tx.generation) {
                break;
            }
            let flags = u32_at(at + 12);
            assert_eq!(flags & (3 << 12), 3 << 12, "end of packet and a completion");
            assert_eq!((word >> 16) & 1, 0, "a descriptor of a frame");
            let len = (word & 0x3fff) as usize;
            state.sent.push(read(u64_at(at), len));
            let index = state.tx.next as u32;
            state.tx.advance();
            let done = state.tx_done.at();
            put32(done, index);
            put32(done + 4, 0);
            put32(done + 8, 0);
            put32(done + 12, state.tx_done.generation_bit());
            state.tx_done.advance();
            any = true;
        }
        if any {
            Card::raise(state);
        }
    }

    /// Hands `frame` to the driver as QEMU does, in the next buffer, with
    /// `skip` buffers skipped first: whether it had one.
    fn deliver_with(&self, frame: &[u8], skip: usize, first: u32, error: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.active {
            return false;
        }
        for _ in 0..=skip {
            let at = state.rx.at();
            if u32_at(at + 8) >> 31 != u32::from(state.rx.generation) {
                return false;
            }
            state.rx.advance();
        }
        let index = (state.rx.next + state.rx.size - 1) % state.rx.size;
        let at = state.rx.base + (index * 16) as u64;
        let word = u32_at(at + 8);
        assert_eq!(word & 0x3fff, 2048, "a buffer of 2 KiB");
        assert_eq!((word >> 14) & 1, 0, "a head buffer");
        fake_memory_write(u64_at(at), frame);
        let done = state.rx_done.at();
        put32(done, index as u32 | first);
        put32(done + 4, 0);
        put32(done + 8, frame.len() as u32 | (u32::from(error) << 14));
        put32(done + 12, state.rx_done.generation_bit());
        state.rx_done.advance();
        Card::raise(&mut state);
        true
    }

    fn deliver(&self, frame: &[u8]) -> bool {
        self.deliver_with(frame, 0, (1 << 14) | (1 << 15), false)
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.state.lock().unwrap().sent)
    }
}

impl Registers for Bar0 {
    fn read(&self, _offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4, "32-bit registers");
        0
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4, "32-bit registers");
        let mut state = self.0.state.lock().unwrap();
        match offset {
            IMASK0 => {
                state.masked = value & 1 != 0;
                Card::update_interrupt(&mut state);
            }
            TXPROD0 => Card::transmit(&mut state),
            RXPROD0 | RXPROD2_0 => state.rx_producer_writes += 1,
            _ => panic!("BAR 0 write at {offset:#x}"),
        }
    }
}

impl Registers for Bar1 {
    fn read(&self, offset: usize, size: usize) -> u64 {
        assert_eq!(size, 4, "32-bit registers");
        let state = self.0.state.lock().unwrap();
        let mac = state.mac;
        u64::from(match offset {
            VRRS => state.revisions,
            UVRS => state.revisions,
            MACL => u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]),
            MACH => u32::from(u16::from_le_bytes([mac[4], mac[5]])),
            CMD => match state.command {
                ACTIVATE => u32::from(!state.active),
                GET_LINK if state.link_up => (1000 << 16) | 1,
                _ => 0,
            },
            _ => panic!("BAR 1 read at {offset:#x}"),
        })
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        assert_eq!(size, 4, "32-bit registers");
        let value = value as u32;
        let mut state = self.0.state.lock().unwrap();
        match offset {
            VRRS | UVRS => assert_eq!(value, 1, "revision 1"),
            DSAL => {
                state.shared_low = value;
                state.shared = 0;
            }
            DSAH => state.shared = u64::from(state.shared_low) | (u64::from(value) << 32),
            MACL => state.temp_mac = value,
            MACH => {
                let low = state.temp_mac.to_le_bytes();
                let high = (value as u16).to_le_bytes();
                state.mac = [low[0], low[1], low[2], low[3], high[0], high[1]];
            }
            CMD => {
                state.command = value;
                state.commands.push(value);
                match value {
                    ACTIVATE => Card::activate(&mut state),
                    QUIESCE => state.active = false,
                    RESET => Card::reset(&mut state),
                    _ => {}
                }
            }
            ECR => {
                state.acknowledged |= value;
                let area = state.shared;
                if area != 0 {
                    put32(area + 696, u32_at(area + 696) & !value);
                }
            }
            _ => panic!("BAR 1 write at {offset:#x}"),
        }
    }
}

fn started(card: &Arc<Card>, interrupt: Interrupt) -> Vmxnet3 {
    let (bar0, bar1) = card.bars();
    start(bar0, bar1, interrupt, FALLBACK, &mut alloc).expect("the card starts")
}

fn frame(n: usize, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i + n * 7) as u8).collect()
}

#[test]
fn starts_describing_itself_and_its_queues() {
    let card = Card::new();
    let nic = started(&card, Interrupt::Msi);
    let area = card.shared();
    assert!(card.get(|state| state.active));
    assert_eq!(u32_at(area + 16), 1, "revision 1");
    assert_eq!(u32_at(area + 20), 1, "UPT revision 1");
    assert_eq!(u64_at(area + 24), 0, "no offloads");
    assert_eq!(u16_at(area + 60), 1, "one buffer a frame");
    assert_eq!(u8_at(area + 80), 1, "automasking");
    assert_eq!(u8_at(area + 81), 1, "one interrupt");
    assert_eq!(u32_at(area + 108), 0, "interrupts on");
    assert_eq!(u32_at(area + 120), 0x01 | 0x04 | 0x08, "unicast, broadcast, every multicast");
    assert_eq!(u32_at(area + 136) & 1, 1, "frames without a VLAN tag");
    assert_eq!(u32_at(area + QUEUES + 4), 1, "a completion threshold of one");
    assert_eq!(u8_at(area + RXQ + 72), 0, "the receive queue's interrupt");
    let commands = card.get(|state| state.commands.clone());
    assert_eq!(
        commands,
        [RESET, ACTIVATE, UPDATE_MAC_FILTERS, UPDATE_RX_MODE, UPDATE_VLAN_FILTERS],
        "reset, then activated and its filters set"
    );
    assert!(!card.get(|state| state.masked), "its interrupt unmasked");
    let info = nic.info();
    assert_eq!((info.model, info.mac), ("VMXNET3", MAC));
    assert_eq!((info.receive_slots, info.transmit_slots), (256, 255));
    let link = nic.link();
    assert!(link.up && link.full_duplex);
    assert_eq!(link.mbps, 1000);
    card.set(|state| state.link_up = false);
    assert!(!nic.link().up);
    assert!(!nic.failed());
}

#[test]
fn receives_frames_round_both_rings_many_times() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    let mut buffer = [0u8; 2048];
    for n in 0..1000 {
        let sent = frame(n, 60 + n % 1400);
        assert!(card.deliver(&sent), "a buffer for frame {n}");
        let len = nic.receive(&mut buffer).expect("the frame");
        assert_eq!(&buffer[..len], &sent[..]);
        assert_eq!(nic.receive(&mut buffer), None);
        nic.flush();
    }
    assert_eq!(nic.dropped(), 0);
    // QEMU asks for no receive producer index: none is written.
    assert_eq!(card.get(|state| state.rx_producer_writes), 2, "only the two at activation");
}

#[test]
fn a_full_ring_waits_for_its_buffers_back() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    for n in 0..256 {
        assert!(card.deliver(&frame(n, 64)));
    }
    assert!(!card.deliver(&frame(256, 64)), "every buffer is taken");
    let mut buffer = [0u8; 2048];
    for n in 0..256 {
        let len = nic.receive(&mut buffer).expect("a frame");
        assert_eq!(&buffer[..len], &frame(n, 64)[..]);
    }
    assert!(card.deliver(&frame(256, 64)), "the buffers are back");
}

#[test]
fn buffers_the_card_skips_are_given_back_too() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    let mut buffer = [0u8; 2048];
    assert!(card.deliver_with(&frame(0, 100), 3, (1 << 14) | (1 << 15), false));
    assert_eq!(nic.receive(&mut buffer), Some(100));
    // Were the skipped ones not given back, the card would stop at them
    // on its next time round.
    for n in 1..600 {
        assert!(card.deliver(&frame(n, 100)), "a buffer for frame {n}");
        assert_eq!(nic.receive(&mut buffer), Some(100));
    }
}

#[test]
fn drops_damaged_short_and_split_frames() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    let whole = (1 << 14) | (1 << 15);
    assert!(card.deliver_with(&frame(0, 100), 0, whole, true), "in error");
    assert!(card.deliver(&frame(1, 10)), "too short");
    assert!(card.deliver_with(&frame(2, 100), 0, 1 << 15, false), "a start without its end");
    assert!(card.deliver_with(&frame(3, 100), 0, 1 << 14, false), "its end");
    assert!(card.deliver(&frame(4, 100)));
    let mut buffer = [0u8; 2048];
    assert_eq!(nic.receive(&mut buffer), Some(100));
    assert_eq!(&buffer[..100], &frame(4, 100)[..]);
    assert_eq!(nic.dropped(), 4);
    // Every buffer came back.
    for n in 0..600 {
        assert!(card.deliver(&frame(n, 100)));
        assert_eq!(nic.receive(&mut buffer), Some(100));
    }
}

#[test]
fn sends_frames_round_the_ring_many_times() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    for n in 0..1000 {
        let sent = frame(n, 14 + n % 1500);
        assert!(nic.can_send());
        assert!(nic.send(&sent));
        nic.flush();
        assert_eq!(card.sent(), [sent]);
    }
    assert!(!nic.send(&[0; 13]), "shorter than a header");
    assert!(!nic.send(&[0; 1519]), "longer than a frame");
}

#[test]
fn a_full_transmit_ring_waits_for_completions() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    card.set(|state| state.stalled = true);
    for n in 0..TRANSMIT - 1 {
        assert!(nic.send(&frame(n, 64)), "slot {n}");
    }
    assert!(!nic.can_send());
    assert!(!nic.send(&frame(255, 64)));
    nic.flush();
    assert!(card.sent().is_empty());
    card.set(|state| state.stalled = false);
    // The card takes them when it is told again.
    card.set(Card::transmit);
    assert_eq!(card.sent().len(), TRANSMIT - 1);
    assert!(nic.can_send());
    assert!(nic.send(&frame(255, 64)));
    nic.flush();
    assert_eq!(card.sent(), [frame(255, 64)]);
}

#[test]
fn unmasks_its_interrupt_once_nothing_is_left() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    let interrupts = || card.get(|state| state.interrupts);
    assert!(card.deliver(&frame(0, 64)));
    assert_eq!(interrupts(), 1);
    // Masked by the card as it raised it: a second frame raises nothing
    // yet, though the card remembers it.
    assert!(card.deliver(&frame(1, 64)));
    assert_eq!(interrupts(), 1);
    let mut buffer = [0u8; 2048];
    assert!(nic.receive(&mut buffer).is_some());
    assert!(nic.receive(&mut buffer).is_some());
    assert_eq!(interrupts(), 1, "still masked while frames wait");
    // Unmasked, it raises what it remembers, though the driver took the
    // frame already: the next look finds nothing, and unmasks it again.
    assert_eq!(nic.receive(&mut buffer), None);
    assert_eq!(interrupts(), 2);
    assert_eq!(nic.receive(&mut buffer), None);
    assert!(!card.get(|state| state.masked), "unmasked");
    assert_eq!(interrupts(), 2);
    assert!(card.deliver(&frame(2, 64)));
    assert_eq!(interrupts(), 3);
    // A frame sent raises it too, and the next look unmasks it.
    assert!(nic.send(&frame(3, 64)));
    nic.flush();
    assert_eq!(nic.receive(&mut buffer).map(|_| ()), Some(()));
    assert_eq!(nic.receive(&mut buffer), None);
    assert_eq!(interrupts(), 4, "the frame sent, while masked, at the unmask");
    assert_eq!(nic.receive(&mut buffer), None);
    assert!(!card.get(|state| state.masked));
}

#[test]
fn acknowledges_the_events_it_raised() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    let area = card.shared();
    put32(area + 696, 0x04);
    let mut buffer = [0u8; 2048];
    assert_eq!(nic.receive(&mut buffer), None);
    assert_eq!(card.get(|state| state.acknowledged), 0x04);
    assert_eq!(u32_at(area + 696), 0);
}

#[test]
fn a_queue_the_card_stopped_is_a_failure() {
    let card = Card::new();
    let nic = started(&card, Interrupt::Msi);
    let area = card.shared();
    assert!(!nic.failed());
    fake_memory_write(area + QUEUES + 80, &[1]);
    assert!(nic.failed());
    fake_memory_write(area + QUEUES + 80, &[0]);
    fake_memory_write(area + RXQ + 80, &[1]);
    assert!(nic.failed());
}

#[test]
fn writes_the_receive_producer_only_when_the_card_asks() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    let area = card.shared();
    fake_memory_write(area + RXQ, &[1]);
    let mut buffer = [0u8; 2048];
    nic.flush();
    assert_eq!(card.get(|state| state.rx_producer_writes), 2, "nothing given back yet");
    assert!(card.deliver(&frame(0, 64)));
    assert!(nic.receive(&mut buffer).is_some());
    nic.flush();
    assert_eq!(card.get(|state| state.rx_producer_writes), 3);
}

#[test]
fn polled_its_interrupts_stay_off() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Polled);
    let area = card.shared();
    assert_eq!(u32_at(area + 108), 1, "every interrupt off");
    assert!(card.get(|state| state.masked));
    assert!(card.deliver(&frame(0, 64)));
    let mut buffer = [0u8; 2048];
    assert!(nic.receive(&mut buffer).is_some());
    assert_eq!(nic.receive(&mut buffer), None);
    assert!(card.get(|state| state.masked), "still masked");
    assert_eq!(card.get(|state| state.interrupts), 0);
}

#[test]
fn takes_the_fallback_address_when_the_card_has_none() {
    let card = Card::new();
    card.set(|state| state.mac = [0; 6]);
    let nic = started(&card, Interrupt::Msi);
    assert_eq!(nic.info().mac, FALLBACK);
    assert_eq!(card.get(|state| state.mac), FALLBACK, "and gives it the card");
}

#[test]
fn refuses_cards_it_cannot_drive() {
    let card = Card::new();
    card.set(|state| state.revisions = 0);
    let (bar0, bar1) = card.bars();
    let err = start(bar0, bar1, Interrupt::Msi, FALLBACK, &mut alloc).err();
    assert_eq!(err, Some(Error::Unsupported("revision 1 of the interface")));

    let card = Card::new();
    card.set(|state| state.revisions = u32::MAX);
    let (bar0, bar1) = card.bars();
    let err = start(bar0, bar1, Interrupt::Msi, FALLBACK, &mut alloc).err();
    assert_eq!(err, Some(Error::Device("its registers read as all ones")));

    let card = Card::new();
    card.set(|state| state.refuses = true);
    let (bar0, bar1) = card.bars();
    let err = start(bar0, bar1, Interrupt::Msi, FALLBACK, &mut alloc).err();
    assert_eq!(err, Some(Error::Device("it would not activate")));
    assert_eq!(card.get(|state| state.commands.last().copied()), Some(RESET), "and reset");

    let card = Card::new();
    let (bar0, bar1) = card.bars();
    let small = bar0.window(0, 0x100).unwrap();
    let err = start(small, bar1, Interrupt::Msi, FALLBACK, &mut alloc).err();
    assert_eq!(err, Some(Error::Registers));
}

#[test]
fn a_stopped_card_does_nothing_more_and_is_stopped_once() {
    let card = Card::new();
    let mut nic = started(&card, Interrupt::Msi);
    nic.stop();
    assert!(!card.get(|state| state.active));
    assert!(card.get(|state| state.masked));
    let commands = card.get(|state| state.commands.len());
    let mut buffer = [0u8; 2048];
    assert_eq!(nic.receive(&mut buffer), None);
    assert!(!nic.can_send());
    assert!(!nic.send(&frame(0, 64)));
    nic.flush();
    nic.stop();
    drop(nic);
    let after = card.get(|state| state.commands.clone());
    assert_eq!(after.len(), commands, "stopped once");
    assert_eq!(&after[after.len() - 2..], [QUIESCE, RESET]);
}

#[test]
fn dropped_it_stops() {
    let card = Card::new();
    let nic = started(&card, Interrupt::Msi);
    drop(nic);
    assert!(!card.get(|state| state.active));
    let commands = card.get(|state| state.commands.clone());
    assert_eq!(&commands[commands.len() - 2..], [QUIESCE, RESET]);
}
