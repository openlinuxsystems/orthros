// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chip's rings and its status block, with a [`Slots`] buffer for
//! each ring that carries frames.
//!
//! - **Standard receive ring**: 512 descriptors of 32 bytes, each a
//!   buffer's address, length and index, of which the driver keeps 256
//!   given to the chip, one for each receive slot: a buffer the chip
//!   filled is given back in the next free descriptor. The driver says
//!   how far it filled the ring through a mailbox, one short of the last
//!   descriptor it wrote, as FreeBSD's does.
//! - **Return ring**: descriptors of the same size the chip writes as it
//!   receives, each naming the buffer by its index, the frame's length
//!   with its frame check sequence, and its errors; the status block
//!   says how far the chip wrote, and a mailbox how far the driver read.
//! - **Send ring**: 512 descriptors of 16 bytes, a buffer's address, its
//!   length and flags, in host memory; at most 255 frames are queued at
//!   once, so that the 256 transmit slots serve it, the slot of a
//!   descriptor being its index modulo 256. A mailbox says how far the
//!   driver filled it, and the status block how far the chip sent.
//! - **Status block**: 32 bytes the chip writes when its coalescing
//!   timers or counts say: a status word, a tag that counts the
//!   updates, and the two indices.

use radix_hw::{Dma, Mmio, device_barrier};
use radix_nic::{Error, MAX_FRAME, MIN_FRAME, PAGE, Slots, next};

use crate::reg;

/// The standard receive ring's and the send ring's sizes, which the
/// chips' ring control blocks are given.
pub const STD_RING: usize = 512;
pub const SEND_RING: usize = 512;
/// How many slots each way.
pub const SLOTS: usize = 256;
/// How long a receive buffer is said to be: FreeBSD's
/// `BGE_MAX_FRAMELEN`, which the standard ring's control block says too.
pub const BUFFER: u32 = 1536;

const RX_DESCRIPTOR: usize = 32;
const TX_DESCRIPTOR: usize = 16;
/// A receive descriptor's words, by their offset: the address's two
/// halves, high first, the length and index, the flags and type, and the
/// error flags beside the VLAN tag.
const BD_ADDR_HI: usize = 0;
const BD_ADDR_LO: usize = 4;
const BD_LEN_INDEX: usize = 8;
const BD_FLAGS: usize = 12;
/// The end of a frame, in both kinds of descriptor.
const BD_END: u32 = 0x0004;
/// A return descriptor's flags: a buffer of another ring, or an error.
const RX_JUMBO_RING: u16 = 0x0020;
const RX_ERROR: u16 = 0x0400;
const RX_MINI_RING: u16 = 0x0800;
/// The frame check sequence the chip leaves at the end of a frame.
const FCS: usize = 4;
/// The shortest frame Ethernet sends, without its FCS: shorter ones
/// are padded with zeros.
const PADDED: usize = 60;
/// How many buffers are given back at once, at most: the rest when no
/// frame is left.
const GIVE_BACK: usize = 32;

/// The status block's words, by their offset.
const STATUS_WORD: usize = 0;
const STATUS_TAG: usize = 4;
const STATUS_RX_PRODUCED: usize = 16;
const STATUS_TX_CONSUMED: usize = 18;
/// What the status word says.
pub const STATUS_LINK_CHANGED: u32 = 0x0000_0002;

/// The rings, as the chip and the driver share them.
pub struct Rings {
    regs: Mmio,
    standard: Dma,
    receive_slots: Slots,
    /// The next standard descriptor to write, and how many buffers were
    /// given back since the mailbox was last written.
    standard_next: usize,
    owed: usize,
    returned: Dma,
    return_count: usize,
    /// The next return descriptor to read.
    next_return: usize,
    transmit: Dma,
    transmit_slots: Slots,
    /// The next send descriptor to fill, and the oldest the chip may
    /// not have sent.
    tail: usize,
    unsent: usize,
    /// Whether frames were queued since the mailbox was last written.
    queued: bool,
    /// How many frames the chip has sent.
    sent: u64,
    status: Dma,
    dropped: u64,
    /// Whether the chip said something that makes no sense.
    broken: bool,
}

fn ring(len: usize, alloc: &mut dyn FnMut(usize) -> Option<Dma>) -> Result<Dma, Error> {
    let dma = alloc(len.div_ceil(PAGE)).ok_or(Error::NoMemory)?;
    if dma.len() < len { Err(Error::NoMemory) } else { Ok(dma) }
}

fn write_address(dma: &Dma, at: usize, phys: u64) {
    dma.write32(at + BD_ADDR_HI, (phys >> 32) as u32);
    dma.write32(at + BD_ADDR_LO, phys as u32);
}

impl Rings {
    /// Allocates the rings, with a return ring of `return_count`
    /// descriptors, and fills the standard ring's first 256 descriptors.
    /// Nothing is told to the chip.
    pub fn new(
        regs: Mmio,
        return_count: usize,
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    ) -> Result<Rings, Error> {
        let standard = ring(STD_RING * RX_DESCRIPTOR, alloc)?;
        let receive_slots = Slots::new(SLOTS, alloc).ok_or(Error::NoMemory)?;
        let returned = ring(return_count * RX_DESCRIPTOR, alloc)?;
        let transmit = ring(SEND_RING * TX_DESCRIPTOR, alloc)?;
        let transmit_slots = Slots::new(SLOTS, alloc).ok_or(Error::NoMemory)?;
        let status = ring(PAGE, alloc)?;
        let mut rings = Rings {
            regs,
            standard,
            receive_slots,
            standard_next: 0,
            owed: 0,
            returned,
            return_count,
            next_return: 0,
            transmit,
            transmit_slots,
            tail: 0,
            unsent: 0,
            queued: false,
            sent: 0,
            status,
            dropped: 0,
            broken: false,
        };
        for slot in 0..SLOTS {
            rings.give(slot);
        }
        rings.owed = 0;
        Ok(rings)
    }

    /// The physical addresses of the standard, return and send rings and
    /// of the status block.
    pub fn addresses(&self) -> (u64, u64, u64, u64) {
        (self.standard.phys(), self.returned.phys(), self.transmit.phys(), self.status.phys())
    }

    /// What the standard ring's mailbox should say now: one short of the
    /// next descriptor to write.
    pub fn standard_producer(&self) -> u32 {
        ((self.standard_next + STD_RING - 1) % STD_RING) as u32
    }

    /// Writes receive slot `slot` into the next standard descriptor.
    fn give(&mut self, slot: usize) {
        let at = self.standard_next * RX_DESCRIPTOR;
        write_address(&self.standard, at, self.receive_slots.phys(slot));
        self.standard.write32(at + BD_LEN_INDEX, BUFFER | (slot as u32) << 16);
        self.standard.write32(at + BD_FLAGS, BD_END);
        self.standard_next = next(self.standard_next, STD_RING);
        self.owed += 1;
    }

    /// Tells the chip of the buffers given back and the return
    /// descriptors read since it was last told.
    fn give_back(&mut self) {
        if self.owed > 0 {
            device_barrier();
            self.regs.write32(reg::MBX_RX_CONS0_LO, self.next_return as u32);
            self.regs.write32(reg::MBX_RX_STD_PROD_LO, self.standard_producer());
            self.owed = 0;
        }
    }

    /// How far the chip wrote the return ring, if that is in it.
    fn produced(&mut self) -> Option<usize> {
        let produced = usize::from(self.status.read16(STATUS_RX_PRODUCED));
        if produced >= self.return_count {
            self.broken = true;
            return None;
        }
        Some(produced)
    }

    /// Copies the next received frame into `frame`, as
    /// [`radix_nic::Nic::receive`] says, giving its buffer back.
    pub fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.broken {
            return None;
        }
        let produced = self.produced()?;
        device_barrier();
        while self.next_return != produced {
            let at = self.next_return * RX_DESCRIPTOR;
            let len_index = self.returned.read32(at + BD_LEN_INDEX);
            let flags = self.returned.read16(at + BD_FLAGS);
            let len = (len_index & 0xffff) as usize;
            let slot = (len_index >> 16) as usize;
            self.next_return = next(self.next_return, self.return_count);
            if slot >= SLOTS {
                self.broken = true;
                return None;
            }
            let good = flags & (RX_ERROR | RX_JUMBO_RING | RX_MINI_RING) == 0
                && (MIN_FRAME + FCS..=MAX_FRAME + FCS).contains(&len);
            let taken = good.then(|| {
                let len = (len - FCS).min(frame.len());
                self.receive_slots.read(slot, &mut frame[..len]);
                len
            });
            self.give(slot);
            if self.owed >= GIVE_BACK {
                self.give_back();
            }
            match taken {
                Some(len) => return Some(len),
                None => self.dropped += 1,
            }
        }
        self.give_back();
        None
    }

    /// Whether a received frame waits.
    pub fn received(&mut self) -> bool {
        !self.broken && self.produced().is_some_and(|produced| produced != self.next_return)
    }

    /// The status block's word and tag, and the status word cleared, so
    /// that the next update shows.
    pub fn take_status(&self) -> (u32, u32) {
        let word = self.status.read32(STATUS_WORD);
        let tag = self.status.read32(STATUS_TAG) & 0xff;
        self.status.write32(STATUS_WORD, 0);
        (word, tag)
    }

    /// The status block's tag.
    pub fn tag(&self) -> u32 {
        self.status.read32(STATUS_TAG) & 0xff
    }

    /// Takes back the slots of frames the chip has sent, as far as the
    /// status block says, if that is among the frames it was handed.
    fn reclaim(&mut self) {
        let consumed = usize::from(self.status.read16(STATUS_TX_CONSUMED));
        let handed = (self.tail + SEND_RING - self.unsent) % SEND_RING;
        let sent = (consumed + SEND_RING - self.unsent) % SEND_RING;
        if consumed < SEND_RING && sent <= handed {
            self.unsent = consumed;
            self.sent += sent as u64;
        }
    }

    /// How many frames the chip has sent, and whether some it was handed
    /// it has not.
    pub fn progress(&mut self) -> (u64, bool) {
        self.reclaim();
        (self.sent, self.unsent != self.tail)
    }

    /// Whether a frame can be queued now.
    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        (self.tail + SEND_RING - self.unsent) % SEND_RING < SLOTS - 1
    }

    /// Copies `frame` into the next transmit slot and its descriptor, as
    /// [`radix_nic::Nic::send`] says, padded with zeros to Ethernet's
    /// shortest.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) || !self.can_send() {
            return false;
        }
        let index = self.tail;
        let slot = index % SLOTS;
        let len = if frame.len() < PADDED {
            let mut padded = [0u8; PADDED];
            padded[..frame.len()].copy_from_slice(frame);
            self.transmit_slots.write(slot, &padded);
            PADDED
        } else {
            self.transmit_slots.write(slot, frame);
            frame.len()
        };
        let at = index * TX_DESCRIPTOR;
        write_address(&self.transmit, at, self.transmit_slots.phys(slot));
        self.transmit.write32(at + BD_LEN_INDEX, BD_END | (len as u32) << 16);
        self.transmit.write32(at + BD_FLAGS, 0);
        self.tail = next(index, SEND_RING);
        self.queued = true;
        true
    }

    /// Tells the chip how far the send ring is filled, when frames were
    /// queued.
    pub fn flush(&mut self) {
        if self.queued {
            device_barrier();
            self.regs.write32(reg::MBX_TX_HOST_PROD0_LO, self.tail as u32);
            self.queued = false;
        }
    }

    /// How many received frames were dropped.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Whether the chip said something that makes no sense.
    pub fn broken(&self) -> bool {
        self.broken
    }
}
