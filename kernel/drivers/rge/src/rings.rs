// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The receive and transmit rings of the chips' first descriptor
//! format, 32 bytes each, with a [`Slots`] buffer for every descriptor.
//!
//! As on Realtek's gigabit chips, whoever owns a descriptor says so in
//! its command and status word's top bit (OWN): the driver sets it to
//! hand a descriptor to the chip, and the chip clears it once it has
//! written a received frame there, or sent the frame it held; the last
//! descriptor of each ring has the end of ring bit (EOR). A receive
//! descriptor holds its buffer's address in its third quadword and its
//! status in its fourth, a transmit descriptor its command in its first
//! word and its buffer's address in its second quadword. The chip looks
//! for frames to send when the transmit start register is written, which
//! [`Rings::flush`] does once per poll.

use radix_hw::{Dma, Mmio, device_barrier};
use radix_nic::{Error, MAX_FRAME, MIN_FRAME, PAGE, SLOT, Slots, next};

use crate::reg;

/// The bytes of a descriptor.
pub const DESCRIPTOR: usize = 32;

/// Where a receive descriptor's buffer address and status are.
const RX_ADDRESS: usize = 16;
const RX_EXTENDED: usize = 24;
const RX_STATUS: usize = 28;
/// Where a transmit descriptor's command and buffer address are.
const TX_COMMAND: usize = 0;
const TX_EXTENDED: usize = 4;
const TX_ADDRESS: usize = 8;

/// A descriptor's command and status: the chip owns it.
const OWN: u32 = 0x8000_0000;
/// The last descriptor of the ring.
const EOR: u32 = 0x4000_0000;
/// A received buffer's status: it starts a frame, and it ends one.
const RX_SOF: u32 = 0x0200_0000;
const RX_EOF: u32 = 0x0100_0000;
/// A received frame's status: something was wrong with it (a CRC error,
/// a runt, a FIFO overflow).
const RX_ERROR: u32 = 0x0010_0000;
/// A received buffer's length, with the frame check sequence in the
/// last buffer of a frame.
const RX_LENGTH: u32 = 0x3fff;
/// A frame to send: it starts and ends in this buffer.
const TX_SOF: u32 = 0x2000_0000;
const TX_EOF: u32 = 0x1000_0000;
/// The frame check sequence the chip leaves at the end of a received
/// frame.
const FCS: usize = 4;
/// The shortest frame Ethernet sends, without its frame check sequence:
/// shorter ones are padded with zeros in their slots.
const PADDED: usize = 60;

struct Ring {
    descriptors: Dma,
    slots: Slots,
    count: usize,
}

impl Ring {
    fn new(count: usize, alloc: &mut dyn FnMut(usize) -> Option<Dma>) -> Result<Ring, Error> {
        let descriptors = alloc((count * DESCRIPTOR).div_ceil(PAGE)).ok_or(Error::NoMemory)?;
        if descriptors.len() < count * DESCRIPTOR {
            return Err(Error::NoMemory);
        }
        let slots = Slots::new(count, alloc).ok_or(Error::NoMemory)?;
        Ok(Ring { descriptors, slots, count })
    }

    fn at(index: usize) -> usize {
        index * DESCRIPTOR
    }

    /// The end of ring bit, for descriptor `index`.
    fn eor(&self, index: usize) -> u32 {
        if index + 1 == self.count { EOR } else { 0 }
    }

    /// Hands receive descriptor `index` to the chip, empty, with its
    /// slot's length.
    fn give(&self, index: usize) {
        let at = Ring::at(index);
        self.descriptors.write32(at + RX_EXTENDED, 0);
        self.descriptors.write64(at + RX_ADDRESS, self.slots.phys(index));
        // The address before the chip can see the descriptor is its own.
        device_barrier();
        self.descriptors.write32(at + RX_STATUS, OWN | self.eor(index) | SLOT as u32);
    }
}

/// The two rings, as the chip and the driver share them.
pub struct Rings {
    regs: Mmio,
    receive: Ring,
    /// The next receive descriptor the chip fills.
    received: usize,
    /// Whether a frame's later buffers are being skipped, after one
    /// without EOF: a frame longer than its slot.
    skipping: bool,
    transmit: Ring,
    /// The next transmit descriptor to fill.
    tail: usize,
    /// The oldest transmit descriptor the chip may still own.
    unsent: usize,
    /// Whether frames were handed to the chip since it was last told.
    queued: bool,
    /// How many frames the chip has sent.
    sent: u64,
    dropped: u64,
}

impl Rings {
    /// Allocates rings of `receive` and `transmit` descriptors, every
    /// receive descriptor the chip's and every transmit descriptor the
    /// driver's; [`Rings::addresses`] says where they are, for the chip.
    pub fn new(
        regs: Mmio,
        receive: usize,
        transmit: usize,
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    ) -> Result<Rings, Error> {
        debug_assert!(receive >= 2 && transmit >= 2);
        let receive = Ring::new(receive, alloc)?;
        let transmit = Ring::new(transmit, alloc)?;
        for index in 0..receive.count {
            receive.give(index);
        }
        for index in 0..transmit.count {
            let at = Ring::at(index);
            transmit.descriptors.write32(at + TX_COMMAND, transmit.eor(index));
        }
        Ok(Rings {
            regs,
            receive,
            received: 0,
            skipping: false,
            transmit,
            tail: 0,
            unsent: 0,
            queued: false,
            sent: 0,
            dropped: 0,
        })
    }

    /// Where the rings are: receive, transmit. They are page-aligned,
    /// as the chip needs them on 256 bytes.
    pub fn addresses(&self) -> (u64, u64) {
        (self.receive.descriptors.phys(), self.transmit.descriptors.phys())
    }

    /// How many descriptors each ring has: receive, transmit.
    pub fn sizes(&self) -> (usize, usize) {
        (self.receive.count, self.transmit.count)
    }

    /// Copies the next received frame into `frame`, as
    /// [`radix_nic::Nic::receive`] says, giving its descriptor back.
    pub fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        for _ in 0..self.receive.count {
            let at = Ring::at(self.received) + RX_STATUS;
            if self.receive.descriptors.read32(at) & OWN != 0 {
                return None;
            }
            device_barrier();
            // Read again after the barrier: the frame and the rest of the
            // word were written before OWN was cleared.
            let status = self.receive.descriptors.read32(at);
            let len = (status & RX_LENGTH) as usize;
            let whole = status & (RX_SOF | RX_EOF) == RX_SOF | RX_EOF;
            let taken = if !whole || self.skipping || status & RX_ERROR != 0 {
                None
            } else if (MIN_FRAME + FCS..=MAX_FRAME + FCS).contains(&len) {
                let len = (len - FCS).min(frame.len());
                self.receive.slots.read(self.received, &mut frame[..len]);
                Some(len)
            } else {
                None
            };
            if taken.is_none() {
                self.dropped += 1;
            }
            // A buffer without EOF starts or continues a frame too long
            // for its slot, whose rest is skipped up to the one with EOF.
            self.skipping = status & RX_EOF == 0;
            self.receive.give(self.received);
            self.received = next(self.received, self.receive.count);
            if taken.is_some() {
                return taken;
            }
        }
        None
    }

    /// Takes back the slots of frames the chip has sent.
    fn reclaim(&mut self) {
        while self.unsent != self.tail {
            let at = Ring::at(self.unsent) + TX_COMMAND;
            if self.transmit.descriptors.read32(at) & OWN != 0 {
                break;
            }
            self.unsent = next(self.unsent, self.transmit.count);
            self.sent += 1;
        }
    }

    /// Whether a frame can be sent now.
    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        next(self.tail, self.transmit.count) != self.unsent
    }

    /// Copies `frame` into the next transmit slot, padded to Ethernet's
    /// shortest frame, and hands its descriptor to the chip, as
    /// [`radix_nic::Nic::send`] says.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) || !self.can_send() {
            return false;
        }
        let index = self.tail;
        let slots = &self.transmit.slots;
        let len = frame.len().max(PADDED);
        if frame.len() < PADDED {
            let mut padded = [0u8; PADDED];
            padded[..frame.len()].copy_from_slice(frame);
            slots.write(index, &padded);
        } else {
            slots.write(index, frame);
        }
        let at = Ring::at(index);
        let ring = &self.transmit.descriptors;
        ring.write32(at + TX_EXTENDED, 0);
        ring.write64(at + TX_ADDRESS, slots.phys(index));
        device_barrier();
        let command = OWN | self.transmit.eor(index) | TX_SOF | TX_EOF | len as u32;
        ring.write32(at + TX_COMMAND, command);
        self.tail = next(index, self.transmit.count);
        self.queued = true;
        true
    }

    /// Tells the chip there are frames to send, once for all those handed
    /// to it since it was last told.
    pub fn flush(&mut self) {
        if self.queued {
            device_barrier();
            self.regs.write16(reg::TXSTART, reg::TXSTART_START);
            self.queued = false;
        }
    }

    /// Whether a received frame waits.
    pub fn received(&self) -> bool {
        let at = Ring::at(self.received) + RX_STATUS;
        self.receive.descriptors.read32(at) & OWN == 0
    }

    /// How many frames the chip has sent, and whether some wait to be.
    pub fn progress(&mut self) -> (u64, bool) {
        self.reclaim();
        (self.sent, self.unsent != self.tail)
    }

    /// How many received frames were dropped.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
