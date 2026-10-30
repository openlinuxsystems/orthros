// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The network driver against a simulated virtio-net card: the same PCI
//! configuration space as the block tests, a BAR with two queues, and a
//! wire whose frames the test puts in and takes out.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::{Mmio, Registers, fake_memory, fake_memory_write};

use crate::net::{HEADER, MAX_FRAME, Net, SLOT, feature};
use crate::tests::{BAR_SIZE, alloc, function};
use crate::{Error, Transport, feature as common_feature, status};

const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

#[derive(Clone, Copy, Default)]
struct Ring {
    size: u16,
    desc: u64,
    driver: u64,
    device: u64,
    enabled: bool,
    last_avail: u16,
    used_idx: u16,
}

#[derive(Default)]
struct State {
    device_select: u32,
    driver_select: u32,
    driver_features: u64,
    status: u8,
    select: u16,
    rings: [Ring; 2],
    link_up: bool,
    /// Frames the driver sent, without their headers.
    sent: Vec<Vec<u8>>,
    /// Whether the card takes frames to send when its doorbell rings.
    stalled: bool,
}

struct Card {
    state: Mutex<State>,
    features: u64,
}

fn read_mem(phys: u64, len: usize) -> Vec<u8> {
    let mut out = std::vec![0u8; len];
    fake_memory(phys, &mut out);
    out
}

fn u16_at(phys: u64) -> u16 {
    u16::from_le_bytes(read_mem(phys, 2).try_into().unwrap())
}

impl Card {
    fn new(features: u64) -> Arc<Card> {
        let state = State { link_up: true, ..State::default() };
        Arc::new(Card { state: Mutex::new(state), features })
    }

    /// The next buffer the driver made available on `ring`, if any: its
    /// head, address, length and whether the card writes it.
    fn next(ring: &mut Ring) -> Option<(u16, u64, u32, bool)> {
        let size = u64::from(ring.size);
        if u16_at(ring.driver + 2) == ring.last_avail {
            return None;
        }
        let head = u16_at(ring.driver + 4 + 2 * (u64::from(ring.last_avail) % size));
        ring.last_avail = ring.last_avail.wrapping_add(1);
        let d = read_mem(ring.desc + 16 * u64::from(head), 16);
        let flags = u16::from_le_bytes(d[12..14].try_into().unwrap());
        assert_eq!(flags & 1, 0, "a frame is one buffer");
        let addr = u64::from_le_bytes(d[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(d[8..12].try_into().unwrap());
        Some((head, addr, len, flags & 2 != 0))
    }

    fn put_used(ring: &mut Ring, head: u16, len: u32) {
        let slot = ring.device + 4 + 8 * (u64::from(ring.used_idx) % u64::from(ring.size));
        fake_memory_write(slot, &u32::from(head).to_le_bytes());
        fake_memory_write(slot + 4, &len.to_le_bytes());
        ring.used_idx = ring.used_idx.wrapping_add(1);
        fake_memory_write(ring.device + 2, &ring.used_idx.to_le_bytes());
    }

    /// Takes every frame the driver made available to send.
    fn transmit(s: &mut State) {
        let ring = &mut s.rings[1];
        while let Some((head, addr, len, writes)) = Card::next(ring) {
            assert!(!writes, "the card writes no frame it sends");
            let bytes = read_mem(addr, len as usize);
            assert_eq!(&bytes[..HEADER], &[0; HEADER], "no offload is asked for");
            s.sent.push(bytes[HEADER..].to_vec());
            Card::put_used(ring, head, 0);
        }
        // Told of what comes next, with event indexes.
        let event = ring.device + 4 + 8 * u64::from(ring.size);
        fake_memory_write(event, &ring.last_avail.to_le_bytes());
    }

    /// Puts `frame` on the wire to the driver, saying it is `claimed`
    /// bytes long (header included): false if the driver gave no buffer.
    fn deliver(&self, frame: &[u8], claimed: Option<u32>) -> bool {
        let mut s = self.state.lock().unwrap();
        let ring = &mut s.rings[0];
        let Some((head, addr, len, writes)) = Card::next(ring) else { return false };
        assert!(writes && len as usize >= HEADER + frame.len());
        let mut bytes = std::vec![0u8; HEADER];
        bytes.extend_from_slice(frame);
        fake_memory_write(addr, &bytes);
        Card::put_used(ring, head, claimed.unwrap_or(bytes.len() as u32));
        true
    }

    fn available(&self, queue: usize) -> u16 {
        let s = self.state.lock().unwrap();
        u16_at(s.rings[queue].driver + 2)
    }
}

impl Registers for Card {
    fn read(&self, offset: usize, size: usize) -> u64 {
        let s = self.state.lock().unwrap();
        match (offset, size) {
            (0x04, 4) => (self.features >> (32 * s.device_select)) & 0xffff_ffff,
            (0x12, 2) => 2,
            (0x14, 1) => u64::from(s.status),
            (0x15, 1) => 0,
            (0x18, 2) => 256,
            (0x1e, 2) => u64::from(s.select),
            (0x1a, 2) => 0,
            (0x2000..0x2006, 1) => u64::from(MAC[offset - 0x2000]),
            (0x2006, 2) => u64::from(s.link_up),
            _ => 0,
        }
    }

    fn write(&self, offset: usize, _size: usize, value: u64) {
        let mut s = self.state.lock().unwrap();
        let select = usize::from(s.select);
        match offset {
            0x00 => s.device_select = value as u32,
            0x08 => s.driver_select = value as u32,
            0x0c => {
                let shift = 32 * s.driver_select;
                s.driver_features =
                    (s.driver_features & !(0xffff_ffff << shift)) | (value << shift);
            }
            0x14 => {
                let value = value as u8;
                if value & status::FEATURES_OK != 0 && s.driver_features & !self.features != 0 {
                    s.status = value & !status::FEATURES_OK;
                } else {
                    s.status = value;
                }
                if value == 0 {
                    s.rings = [Ring::default(); 2];
                }
            }
            0x16 => s.select = value as u16,
            0x18 => s.rings[select].size = value as u16,
            0x1c => s.rings[select].enabled = value == 1,
            0x20 => s.rings[select].desc = value,
            0x28 => s.rings[select].driver = value,
            0x30 => s.rings[select].device = value,
            0x3000 | 0x3004 => {
                assert!(s.status & status::DRIVER_OK != 0, "notified before ready");
                if offset == 0x3004 && !s.stalled {
                    Card::transmit(&mut s);
                }
            }
            _ => {}
        }
    }
}

const FEATURES: u64 = common_feature::VERSION_1
    | common_feature::EVENT_IDX
    | feature::MAC
    | feature::STATUS
    // Checksum offload, which the driver does not ask for.
    | 1;

fn setup(features: u64) -> (Arc<Card>, Net) {
    let card = Card::new(features);
    let f = function(1, 0x1041);
    let regs = card.clone();
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(regs.clone(), BAR_SIZE))).unwrap();
    let net = Net::new(transport, 0, [0x02, 0, 0, 0, 0, 1], &mut alloc).unwrap();
    (card, net)
}

fn frame(n: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| n.wrapping_add(i as u8)).collect()
}

#[test]
fn sets_up_with_the_cards_mac_and_every_receive_slot_given() {
    let (card, net) = setup(FEATURES);
    let info = net.info();
    assert_eq!(info.mac, MAC);
    assert!(info.link_status && net.link_up());
    assert_eq!((info.receive_slots, info.transmit_slots), (256, 256));
    assert_eq!(card.available(0), 256);
    let s = card.state.lock().unwrap();
    assert_eq!(s.driver_features & 1, 0, "no offload asked for");
    assert_ne!(s.status & status::DRIVER_OK, 0);
}

#[test]
fn takes_the_fallback_mac_and_an_up_link_without_the_features() {
    let (_card, net) = setup(common_feature::VERSION_1);
    assert_eq!(net.info().mac, [0x02, 0, 0, 0, 0, 1]);
    assert!(!net.info().link_status && net.link_up());
}

#[test]
fn says_when_the_link_goes_down() {
    let (card, net) = setup(FEATURES);
    card.state.lock().unwrap().link_up = false;
    assert!(!net.link_up());
}

#[test]
fn refuses_a_card_older_than_virtio_1() {
    let card = Card::new(feature::MAC);
    let f = function(1, 0x1000);
    let regs = card.clone();
    let transport =
        Transport::find(&f, &mut |_, _| Some(Mmio::fake(regs.clone(), BAR_SIZE))).unwrap();
    let err = Net::new(transport, 0, [0; 6], &mut alloc).err();
    assert_eq!(err, Some(Error::MissingFeature("virtio 1.0")));
}

#[test]
fn receives_frames_in_order_and_gives_their_slots_back() {
    let (card, mut net) = setup(FEATURES);
    let mut buf = [0u8; 2048];
    assert_eq!(net.receive(&mut buf), None);
    // More frames than slots, so that slots must come back to be reused.
    for n in 0..600u32 {
        let sent = frame(n as u8, 60 + (n as usize % 1400));
        assert!(card.deliver(&sent, None), "frame {n} found no slot");
        let len = net.receive(&mut buf).unwrap();
        assert_eq!(&buf[..len], &sent[..]);
        net.notify();
    }
    assert_eq!(net.receive(&mut buf), None);
    assert_eq!(card.available(0), 256 + 600);
    assert_eq!((net.dropped(), net.bad_entries()), (0, 0));
}

#[test]
fn drops_frames_too_short_or_longer_than_their_slot() {
    let (card, mut net) = setup(FEATURES);
    let mut buf = [0u8; 2048];
    assert!(card.deliver(&frame(1, 10), None));
    assert!(card.deliver(&frame(2, 60), Some(SLOT as u32 + 1)));
    let good = frame(3, 100);
    assert!(card.deliver(&good, None));
    let len = net.receive(&mut buf).unwrap();
    assert_eq!(&buf[..len], &good[..]);
    assert_eq!(net.dropped(), 2);
    // The dropped frames' slots went back too.
    assert_eq!(card.available(0), 256 + 3);
}

#[test]
fn cuts_a_frame_to_the_callers_buffer() {
    let (card, mut net) = setup(FEATURES);
    assert!(card.deliver(&frame(7, 1514), None));
    let mut small = [0u8; 100];
    assert_eq!(net.receive(&mut small), Some(100));
    assert_eq!(&small[..], &frame(7, 100)[..]);
}

#[test]
fn sends_frames_with_an_empty_header() {
    let (card, mut net) = setup(FEATURES);
    let frames: Vec<Vec<u8>> = (0..300u32).map(|n| frame(n as u8, 14 + n as usize)).collect();
    for f in &frames {
        net.send(f).unwrap();
        net.notify();
    }
    assert_eq!(card.state.lock().unwrap().sent, frames);
}

#[test]
fn refuses_frames_it_cannot_send() {
    let (_card, mut net) = setup(FEATURES);
    assert_eq!(net.send(&frame(0, 13)), Err(Error::BadRequest));
    assert_eq!(net.send(&frame(0, MAX_FRAME + 1)), Err(Error::BadRequest));
    assert_eq!(net.send(&frame(0, MAX_FRAME)), Ok(()));
}

#[test]
fn says_the_queue_is_full_until_the_card_sends() {
    let (card, mut net) = setup(FEATURES);
    card.state.lock().unwrap().stalled = true;
    for n in 0..256u32 {
        net.send(&frame(n as u8, 60)).unwrap();
    }
    net.notify();
    assert!(!net.can_send());
    assert_eq!(net.send(&frame(0, 60)), Err(Error::Full));
    {
        let mut s = card.state.lock().unwrap();
        s.stalled = false;
        Card::transmit(&mut s);
        assert_eq!(s.sent.len(), 256);
    }
    assert!(net.can_send());
    net.send(&frame(9, 60)).unwrap();
    net.notify();
    assert_eq!(card.state.lock().unwrap().sent.len(), 257);
}
