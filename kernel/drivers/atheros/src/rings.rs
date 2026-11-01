// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The chip's three rings, each with a [`Slots`] buffer where it needs
//! one.
//!
//! - **Transmit**: 16-byte descriptors, a length, flags and a buffer's
//!   address; one frame a descriptor, marked as its end of packet. The
//!   driver tells the chip how far it filled the ring through a producer
//!   register, and the chip says how far it sent through a consumer one.
//! - **Receive**: 8-byte descriptors, a buffer's address each, written
//!   once; the driver gives buffers back by moving a producer register,
//!   which stops one short of the buffer the chip fills next, so that all
//!   but one are the chip's.
//! - **Return**: 16-byte descriptors the chip writes as it receives, each
//!   naming the first buffer of a frame, how many it took, its length
//!   with its frame check sequence, and its errors, under a valid bit the
//!   driver clears to give the descriptor back.

use radix_hw::{Dma, Mmio, device_barrier};
use radix_nic::{Error, MAX_FRAME, MIN_FRAME, PAGE, Slots, next};

use crate::{Chip, reg};

const TX_DESCRIPTOR: usize = 16;
const RX_DESCRIPTOR: usize = 8;
const RETURN_DESCRIPTOR: usize = 16;
/// Where the statistics block is in the page the coalescing block
/// starts.
pub const SMB_OFFSET: u64 = 2048;

/// A transmit descriptor's flags: the end of a frame.
const TD_EOP: u32 = 0x8000_0000;
/// A transmit descriptor's length.
const TD_LENGTH: u32 = 0x3fff;
/// A return descriptor's words, by their offset.
const RRD_INFO: usize = 0;
const RRD_STATUS: usize = 12;
/// What the first word says: how many buffers the frame took, and the
/// first.
const RRD_COUNT_SHIFT: u32 = 16;
const RRD_COUNT: u32 = 0xf;
const RRD_INDEX_SHIFT: u32 = 20;
/// What the status says: the frame's length, its errors, and that the
/// chip wrote the descriptor.
const RRD_LENGTH: u32 = 0x3fff;
const RRD_ERR_SUM: u32 = 0x0010_0000;
const RRD_ERR_CRC: u32 = 0x0020_0000;
const RRD_ERR_ALIGN: u32 = 0x0040_0000;
const RRD_ERR_TRUNC: u32 = 0x0080_0000;
const RRD_ERR_RUNT: u32 = 0x0100_0000;
const RRD_ERR_LENGTH: u32 = 0x4000_0000;
const RRD_VALID: u32 = 0x8000_0000;
/// The errors that make a frame damaged; a wrong checksum or a length
/// that is not its protocol's is the stack's to find.
const RRD_DAMAGED: u32 = RRD_ERR_CRC | RRD_ERR_ALIGN | RRD_ERR_TRUNC | RRD_ERR_RUNT;
/// The frame check sequence the chip leaves at the end of a frame.
const FCS: usize = 4;
/// How many buffers are given back at once, at most: the rest when no
/// frame is left.
const GIVE_BACK: usize = 32;

/// The rings, as the chip and the driver share them.
pub struct Rings {
    regs: Mmio,
    ar816x: bool,
    count: usize,
    transmit: Dma,
    transmit_slots: Slots,
    /// The next transmit descriptor to fill.
    tail: usize,
    /// The oldest transmit descriptor the chip may not have sent.
    unsent: usize,
    /// Whether frames were handed to the chip since its producer was
    /// last written.
    queued: bool,
    receive: Dma,
    receive_slots: Slots,
    returned: Dma,
    /// The next return descriptor the chip writes.
    next_return: usize,
    /// The receive producer: the last buffer given to the chip, and how
    /// many were taken since it was written.
    producer: usize,
    owed: usize,
    /// The coalescing and statistics blocks' page.
    blocks: Dma,
    dropped: u64,
    /// Whether the chip wrote a return descriptor that makes no sense.
    broken: bool,
}

fn ring(len: usize, alloc: &mut dyn FnMut(usize) -> Option<Dma>) -> Result<Dma, Error> {
    let dma = alloc(len.div_ceil(PAGE)).ok_or(Error::NoMemory)?;
    if dma.len() < len { Err(Error::NoMemory) } else { Ok(dma) }
}

impl Rings {
    /// Allocates rings of `count` descriptors, with every receive buffer
    /// the chip's but one, and tells the chip of the buffers. The chip
    /// must have been reset.
    pub fn new(
        regs: Mmio,
        chip: &Chip,
        count: usize,
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    ) -> Result<Rings, Error> {
        debug_assert!(count >= 2);
        let transmit = ring(count * TX_DESCRIPTOR, alloc)?;
        let transmit_slots = Slots::new(count, alloc).ok_or(Error::NoMemory)?;
        let receive = ring(count * RX_DESCRIPTOR, alloc)?;
        let receive_slots = Slots::new(count, alloc).ok_or(Error::NoMemory)?;
        let returned = ring(count * RETURN_DESCRIPTOR, alloc)?;
        let blocks = ring(PAGE, alloc)?;
        for index in 0..count {
            receive.write64(index * RX_DESCRIPTOR, receive_slots.phys(index));
        }
        let rings = Rings {
            regs,
            ar816x: chip.ar816x(),
            count,
            transmit,
            transmit_slots,
            tail: 0,
            unsent: 0,
            queued: false,
            receive,
            receive_slots,
            returned,
            next_return: 0,
            producer: count - 1,
            owed: 0,
            blocks,
            dropped: 0,
            broken: false,
        };
        device_barrier();
        rings.write_producer();
        Ok(rings)
    }

    /// The physical addresses of the transmit, receive and return rings.
    pub fn addresses(&self) -> (u64, u64, u64) {
        (self.transmit.phys(), self.receive.phys(), self.returned.phys())
    }

    /// The physical address of the blocks' page.
    pub fn blocks(&self) -> u64 {
        self.blocks.phys()
    }

    /// How many descriptors each ring has: receive, transmit.
    pub fn sizes(&self) -> (usize, usize) {
        (self.count, self.count)
    }

    fn write_producer(&self) {
        if self.ar816x {
            self.regs.write16(reg::MBOX_RD0_PROD_IDX, self.producer as u16);
        } else {
            self.regs.write32(reg::MBOX_RD0_PROD_IDX, self.producer as u32);
        }
    }

    /// Gives the chip back the buffers taken since it was last told.
    fn give_back(&mut self) {
        if self.owed > 0 {
            device_barrier();
            self.write_producer();
            self.owed = 0;
        }
    }

    /// Copies the next received frame into `frame`, as
    /// [`radix_nic::Nic::receive`] says, giving its buffers back.
    pub fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        for _ in 0..self.count {
            if self.broken {
                return None;
            }
            let at = self.next_return * RETURN_DESCRIPTOR;
            if self.returned.read32(at + RRD_STATUS) & RRD_VALID == 0 {
                break;
            }
            device_barrier();
            // Read again after the barrier: the frame and the rest of the
            // descriptor were written before the valid bit.
            let status = self.returned.read32(at + RRD_STATUS);
            let info = self.returned.read32(at + RRD_INFO);
            let buffers = ((info >> RRD_COUNT_SHIFT) & RRD_COUNT) as usize;
            let first = (info >> RRD_INDEX_SHIFT) as usize;
            if buffers == 0 {
                // FreeBSD resets the chip for this, which "should not
                // happen".
                self.broken = true;
                return None;
            }
            let len = (status & RRD_LENGTH) as usize;
            let damaged = status & (RRD_ERR_SUM | RRD_ERR_LENGTH) != 0 && status & RRD_DAMAGED != 0;
            let taken = if buffers != 1 || damaged || first >= self.count {
                None
            } else if (MIN_FRAME + FCS..=MAX_FRAME + FCS).contains(&len) {
                let len = (len - FCS).min(frame.len());
                self.receive_slots.read(first, &mut frame[..len]);
                Some(len)
            } else {
                None
            };
            if taken.is_none() {
                self.dropped += 1;
            }
            self.returned.write32(at + RRD_STATUS, 0);
            self.next_return = next(self.next_return, self.count);
            self.producer = (self.producer + buffers) % self.count;
            self.owed += buffers;
            if self.owed >= GIVE_BACK {
                self.give_back();
            }
            if taken.is_some() {
                return taken;
            }
        }
        self.give_back();
        None
    }

    /// Whether a received frame waits.
    pub fn received(&self) -> bool {
        let at = self.next_return * RETURN_DESCRIPTOR;
        !self.broken && self.returned.read32(at + RRD_STATUS) & RRD_VALID != 0
    }

    /// Takes back the slots of frames the chip has sent, as far as its
    /// consumer says, if that is among the frames it was handed.
    fn reclaim(&mut self) {
        let consumer = if self.ar816x {
            usize::from(self.regs.read16(reg::MBOX_TD_PRI0_CONS_IDX))
        } else {
            (self.regs.read32(reg::MBOX_TD_CONS_IDX) >> 16) as usize
        };
        let handed = (self.tail + self.count - self.unsent) % self.count;
        let sent = (consumer + self.count - self.unsent) % self.count;
        if consumer < self.count && sent <= handed {
            self.unsent = consumer;
        }
    }

    /// Whether a frame can be sent now.
    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        next(self.tail, self.count) != self.unsent
    }

    /// Copies `frame` into the next transmit slot and its descriptor, as
    /// [`radix_nic::Nic::send`] says; the MAC pads a short one.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) || !self.can_send() {
            return false;
        }
        let index = self.tail;
        self.transmit_slots.write(index, frame);
        let at = index * TX_DESCRIPTOR;
        self.transmit.write32(at, frame.len() as u32 & TD_LENGTH);
        self.transmit.write32(at + 4, TD_EOP);
        self.transmit.write64(at + 8, self.transmit_slots.phys(index));
        self.tail = next(index, self.count);
        self.queued = true;
        true
    }

    /// Tells the chip how far the transmit ring is filled, when frames
    /// were handed to it.
    pub fn flush(&mut self) {
        if !self.queued {
            return;
        }
        device_barrier();
        if self.ar816x {
            self.regs.write16(reg::MBOX_TD_PRI0_PROD_IDX, self.tail as u16);
        } else {
            self.regs.write32(reg::MBOX_TD_PROD_IDX, (self.tail as u32) << 16);
        }
        self.queued = false;
    }

    /// How many received frames were dropped.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Whether the chip wrote a return descriptor that makes no sense.
    pub fn broken(&self) -> bool {
        self.broken
    }
}
