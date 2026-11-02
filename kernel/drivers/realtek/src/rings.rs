// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The receive and transmit rings of C+ mode's 16-byte descriptors,
//! which every RTL8169, RTL8168 and RTL810x has, each with a [`Slots`]
//! buffer for every descriptor.
//!
//! A descriptor is a command and status word, a VLAN word, and its
//! buffer's address. Whoever owns it says so in the command word's top
//! bit (OWN): the driver sets it to hand a descriptor to the card, and
//! the card clears it once it has written a received frame there, or
//! sent the frame it held. The last descriptor of each ring has the end
//! of ring bit (EOR), which sends the card back to the first. There are
//! no head or tail registers: every receive descriptor is the card's
//! until it fills it, and the card looks for frames to send only when
//! the transmit poll register is written, which [`Rings::flush`] does
//! once per poll.

use radix_hw::{Dma, Mmio, device_barrier};
use radix_nic::{Error, MAX_FRAME, MIN_FRAME, PAGE, SLOT, Slots, next};

use crate::reg;

/// The bytes of a descriptor.
pub const DESCRIPTOR: usize = 16;

/// A descriptor's command and status: the card owns it.
const OWN: u32 = 0x8000_0000;
/// The command: the last descriptor of the ring.
const EOR: u32 = 0x4000_0000;
/// The status of a received buffer, and the command of one to send: it
/// starts a frame.
const SOF: u32 = 0x2000_0000;
/// The same: it ends a frame.
const EOF: u32 = 0x1000_0000;
/// A received frame's status: something was wrong with it (RXERRSUM:
/// a CRC error, a runt, a FIFO overflow), in the gigabit chips' layout,
/// one bit further left than the RTL8139C+'s.
const RX_ERROR: u32 = 0x0020_0000;
/// A received frame's length, with its frame check sequence.
const RX_LENGTH: u32 = 0x3fff;
/// The frame check sequence the card leaves at the end of a received
/// frame.
const FCS: usize = 4;
/// The shortest frame Ethernet sends, without its frame check sequence:
/// shorter ones are padded with zeros in their slots, as some of the
/// chips send the wrong bytes when they pad one themselves.
const PADDED: usize = 60;

/// The transmit poll register's bit that has the card look at its
/// normal-priority descriptors.
const POLL_NORMAL: u8 = 0x40;

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

    /// The command word's end of ring bit, for descriptor `index`.
    fn eor(&self, index: usize) -> u32 {
        if index + 1 == self.count { EOR } else { 0 }
    }

    /// Hands receive descriptor `index` to the card, empty.
    fn give(&self, index: usize) {
        let at = Ring::at(index);
        self.descriptors.write32(at + 4, 0);
        self.descriptors.write64(at + 8, self.slots.phys(index));
        // The address before the card can see the descriptor is its own.
        device_barrier();
        self.descriptors.write32(at, OWN | self.eor(index) | SLOT as u32);
    }
}

/// The two rings, as the card and the driver share them.
pub struct Rings {
    regs: Mmio,
    receive: Ring,
    /// The next receive descriptor the card fills.
    received: usize,
    /// Whether a frame's later buffers are being skipped, after one
    /// without EOF: a frame longer than its slot, which only the chips
    /// that take frames up to 16 KiB give.
    skipping: bool,
    transmit: Ring,
    /// The next transmit descriptor to fill.
    tail: usize,
    /// The oldest transmit descriptor the card may still own.
    unsent: usize,
    /// Whether frames were handed to the card since it was last polled.
    queued: bool,
    dropped: u64,
}

impl Rings {
    /// Allocates rings of `receive` and `transmit` descriptors and tells
    /// the card where they are, every receive descriptor the card's and
    /// every transmit descriptor the driver's. The card's receiver and
    /// transmitter must be off.
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
            transmit.descriptors.write32(at, transmit.eor(index));
        }
        // The rings are page-aligned, as the card needs them on 256 bytes.
        let (rx, tx) = (receive.descriptors.phys(), transmit.descriptors.phys());
        regs.write32(reg::RXLIST_HI, (rx >> 32) as u32);
        regs.write32(reg::RXLIST_LO, rx as u32);
        regs.write32(reg::TXLIST_HI, (tx >> 32) as u32);
        regs.write32(reg::TXLIST_LO, tx as u32);
        Ok(Rings {
            regs,
            receive,
            received: 0,
            skipping: false,
            transmit,
            tail: 0,
            unsent: 0,
            queued: false,
            dropped: 0,
        })
    }

    /// How many descriptors each ring has: receive, transmit.
    pub fn sizes(&self) -> (usize, usize) {
        (self.receive.count, self.transmit.count)
    }

    /// Copies the next received frame into `frame`, as
    /// [`radix_nic::Nic::receive`] says, giving its descriptor back.
    pub fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        for _ in 0..self.receive.count {
            let at = Ring::at(self.received);
            if self.receive.descriptors.read32(at) & OWN != 0 {
                return None;
            }
            device_barrier();
            // Read again after the barrier: the frame and the rest of the
            // word were written before OWN was cleared.
            let status = self.receive.descriptors.read32(at);
            let len = (status & RX_LENGTH) as usize;
            let whole = status & (SOF | EOF) == SOF | EOF;
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
            self.skipping = status & EOF == 0;
            self.receive.give(self.received);
            self.received = next(self.received, self.receive.count);
            if taken.is_some() {
                return taken;
            }
        }
        None
    }

    /// Takes back the slots of frames the card has sent.
    fn reclaim(&mut self) {
        while self.unsent != self.tail {
            if self.transmit.descriptors.read32(Ring::at(self.unsent)) & OWN != 0 {
                break;
            }
            self.unsent = next(self.unsent, self.transmit.count);
        }
    }

    /// Whether a frame can be sent now.
    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        next(self.tail, self.transmit.count) != self.unsent
    }

    /// Copies `frame` into the next transmit slot, padded to Ethernet's
    /// shortest frame, and hands its descriptor to the card, as
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
        ring.write32(at + 4, 0);
        ring.write64(at + 8, slots.phys(index));
        device_barrier();
        ring.write32(at, OWN | self.transmit.eor(index) | SOF | EOF | len as u32);
        self.tail = next(index, self.transmit.count);
        self.queued = true;
        true
    }

    /// Has the card look at its transmit descriptors when frames were
    /// handed to it, and again while it still owns some: a poll can be
    /// lost on the PCI Express chips when frames come close together, or
    /// ignored while the card is sending, which would leave frames
    /// waiting for good.
    pub fn flush(&mut self) {
        if !self.queued {
            self.reclaim();
        }
        if self.queued || self.unsent != self.tail {
            device_barrier();
            self.regs.write8(reg::TXPOLL, POLL_NORMAL);
            self.queued = false;
        }
    }

    /// Whether a received frame waits.
    pub fn received(&self) -> bool {
        self.receive.descriptors.read32(Ring::at(self.received)) & OWN == 0
    }

    /// How many received frames were dropped.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
