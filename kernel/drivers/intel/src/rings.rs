// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A receive and a transmit ring of 16-byte descriptors, each with a
//! [`Slots`] buffer for every descriptor: the legacy descriptors every
//! e1000 and e1000e controller has (8254x Software Developer's Manual,
//! sections 3.2.3 and 3.3.3), or the advanced ones of igb's (82576
//! datasheet, sections 7.1.5 and 7.2.2), whose fields sit elsewhere.
//!
//! A receive descriptor is handed to the card with its slot's address;
//! the card writes a frame there and the descriptor's length and status
//! back, done (DD) and end of packet (EOP). The card may fill every
//! descriptor from the head (RDH) up to, not including, the tail (RDT),
//! so the tail starts one short of the ring's end, and a descriptor
//! whose frame has been copied out becomes the new tail. A transmit
//! descriptor carries its slot's address and the frame's length, with
//! the commands to add the frame check sequence and to report status
//! (RS), so the card writes DD back once it has sent it and its slot can
//! be used again. Tails are written once per poll, in [`Rings::flush`].

use radix_hw::{Dma, Mmio, device_barrier};
use radix_nic::{Error, MAX_FRAME, MIN_FRAME, PAGE, SLOT, Slots, next};

/// The bytes of a descriptor.
pub const DESCRIPTOR: usize = 16;

/// A receive descriptor's status: the card is done with it.
const RX_DD: u32 = 0x01;
/// A receive descriptor's status: its buffer ends the frame.
const RX_EOP: u32 = 0x02;
/// A legacy receive descriptor's errors, in the byte after its status,
/// that make its frame worth nothing: a CRC or alignment error, a symbol
/// error, a sequence error, a carrier extension error and an RX data
/// error. The checksum errors are about offloads, which are off.
const LEGACY_RX_ERRORS: u32 = (0x01 | 0x02 | 0x04 | 0x10 | 0x80) << 8;
/// The same errors of an advanced receive descriptor, in the top bits of
/// its status word (`E1000_RXDEXT_ERR_FRAME_ERR_MASK`).
const ADVANCED_RX_ERRORS: u32 = 0x0100_0000 | 0x0200_0000 | 0x0400_0000 | 0x1000_0000 | 0x8000_0000;
/// ixgbe's, in other bits of the same word: a CRC, length, packet,
/// oversize or undersize error, the last the 82599's any MAC error
/// (`IXGBE_RXDADV_ERR_FRAME_ERR_MASK`).
const IXGBE_RX_ERRORS: u32 = 0x0100_0000 | 0x0200_0000 | 0x0800_0000 | 0x1000_0000 | 0x2000_0000;

/// A transmit descriptor's command: end of packet.
const TX_EOP: u64 = 0x01;
/// A transmit descriptor's command: insert the frame check sequence.
const TX_IFCS: u64 = 0x02;
/// A transmit descriptor's command: report its status.
const TX_RS: u64 = 0x08;
/// An advanced transmit descriptor's command: it is an advanced one.
const TX_DEXT: u64 = 0x20;
/// An advanced transmit descriptor's type: data.
const TX_DTYP_DATA: u64 = 0x3 << 20;
/// Where an advanced transmit descriptor holds the frame's length again,
/// as its payload's.
const TX_PAYLEN_SHIFT: u32 = 46;
/// A transmit descriptor's status: the card is done with it.
const TX_DD: u8 = 0x01;

/// Which descriptors a card's rings hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// The e1000's and e1000e's: a receive descriptor's length at byte
    /// 8, its status at 12 and its errors at 13.
    Legacy,
    /// igb's advanced descriptors, one buffer each: a receive
    /// descriptor's status and errors in the word at byte 8, its length
    /// at 12; a transmit descriptor of type data with its length twice.
    Advanced,
    /// ixgbe's, igb's but for the bits of a receive descriptor's errors.
    Ixgbe,
}

impl Format {
    /// Where a receive descriptor's status word is, and where its length.
    fn receive_fields(self) -> (usize, usize, u32) {
        match self {
            Format::Legacy => (12, 8, LEGACY_RX_ERRORS),
            Format::Advanced => (8, 12, ADVANCED_RX_ERRORS),
            Format::Ixgbe => (8, 12, IXGBE_RX_ERRORS),
        }
    }

    /// The second half of a transmit descriptor that sends `len` bytes.
    fn transmit_word(self, len: usize) -> u64 {
        let len = len as u64;
        match self {
            Format::Legacy => len | ((TX_EOP | TX_IFCS | TX_RS) << 24),
            Format::Advanced | Format::Ixgbe => {
                len | TX_DTYP_DATA
                    | ((TX_EOP | TX_IFCS | TX_RS | TX_DEXT) << 24)
                    | (len << TX_PAYLEN_SHIFT)
            }
        }
    }
}

/// Where a card keeps its first queue pair's registers.
#[derive(Clone, Copy, Debug)]
pub struct Offsets {
    pub rdbal: usize,
    pub rdbah: usize,
    pub rdlen: usize,
    pub rdh: usize,
    pub rdt: usize,
    pub tdbal: usize,
    pub tdbah: usize,
    pub tdlen: usize,
    pub tdh: usize,
    pub tdt: usize,
}

/// The e1000 family's: queue 0's registers at 0x2800 and 0x3800.
pub const EM: Offsets = Offsets {
    rdbal: 0x2800,
    rdbah: 0x2804,
    rdlen: 0x2808,
    rdh: 0x2810,
    rdt: 0x2818,
    tdbal: 0x3800,
    tdbah: 0x3804,
    tdlen: 0x3808,
    tdh: 0x3810,
    tdt: 0x3818,
};

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
}

/// The two rings, as the card and the driver share them.
pub struct Rings {
    regs: Mmio,
    offsets: Offsets,
    format: Format,
    receive: Ring,
    /// The next receive descriptor the card fills.
    received: usize,
    /// The receive tail as the card has it, and as it should be.
    receive_tail: (usize, usize),
    /// Whether a frame's later buffers are being skipped, after one
    /// without EOP: none should come, with buffers longer than any frame.
    skipping: bool,
    transmit: Ring,
    /// The next transmit descriptor to fill.
    transmit_tail: usize,
    /// The transmit tail as the card has it.
    transmit_told: usize,
    /// The oldest transmit descriptor the card may still be sending.
    unsent: usize,
    dropped: u64,
}

impl Rings {
    /// Allocates rings of `receive` and `transmit` descriptors of
    /// `format` (each a multiple of 8, so that a ring is a multiple of 128
    /// bytes) and tells the card where they are, both empty: once its
    /// queues are on, [`Rings::hand_over`] gives it the receive
    /// descriptors. The card's receiver and transmitter must be off.
    pub fn new(
        regs: Mmio,
        offsets: Offsets,
        format: Format,
        receive: usize,
        transmit: usize,
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    ) -> Result<Rings, Error> {
        debug_assert!(
            receive.is_multiple_of(8)
                && transmit.is_multiple_of(8)
                && receive >= 8
                && transmit >= 8
        );
        let receive = Ring::new(receive, alloc)?;
        let transmit = Ring::new(transmit, alloc)?;
        for index in 0..receive.count {
            receive.descriptors.write64(Ring::at(index), receive.slots.phys(index));
        }
        let o = offsets;
        let phys = receive.descriptors.phys();
        regs.write32(o.rdbal, phys as u32);
        regs.write32(o.rdbah, (phys >> 32) as u32);
        regs.write32(o.rdlen, (receive.count * DESCRIPTOR) as u32);
        regs.write32(o.rdh, 0);
        regs.write32(o.rdt, 0);
        let tail = receive.count - 1;
        let phys = transmit.descriptors.phys();
        regs.write32(o.tdbal, phys as u32);
        regs.write32(o.tdbah, (phys >> 32) as u32);
        regs.write32(o.tdlen, (transmit.count * DESCRIPTOR) as u32);
        regs.write32(o.tdh, 0);
        regs.write32(o.tdt, 0);
        Ok(Rings {
            regs,
            offsets,
            format,
            receive,
            received: 0,
            receive_tail: (0, tail),
            skipping: false,
            transmit,
            transmit_tail: 0,
            transmit_told: 0,
            unsent: 0,
            dropped: 0,
        })
    }

    /// Hands the card every receive descriptor but one, by its tail.
    pub fn hand_over(&mut self) {
        self.flush();
    }

    /// How many descriptors each ring has: receive, transmit.
    pub fn sizes(&self) -> (usize, usize) {
        (self.receive.count, self.transmit.count)
    }

    /// Copies the next received frame into `frame`, as
    /// [`radix_nic::Nic::receive`] says, giving its descriptor back.
    pub fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        let (status_at, len_at, errors) = self.format.receive_fields();
        for _ in 0..self.receive.count {
            let at = Ring::at(self.received);
            let ring = &self.receive.descriptors;
            let status = ring.read32(at + status_at);
            if status & RX_DD == 0 {
                return None;
            }
            device_barrier();
            // Read again after the barrier: the rest of the word may have
            // been written after DD was seen.
            let status = ring.read32(at + status_at);
            let len = usize::from(ring.read16(at + len_at));
            let whole = status & RX_EOP != 0;
            let taken = if !whole || self.skipping || status & errors != 0 {
                None
            } else if (MIN_FRAME..=MAX_FRAME.min(SLOT)).contains(&len) {
                let len = len.min(frame.len());
                self.receive.slots.read(self.received, &mut frame[..len]);
                Some(len)
            } else {
                None
            };
            if taken.is_none() {
                self.dropped += 1;
            }
            // A buffer without EOP starts a frame too long for its slot,
            // whose rest is skipped up to the buffer with EOP.
            self.skipping = !whole;
            // Hand it back: its address again, its status cleared (in the
            // advanced format, the header buffer's address, none).
            ring.write64(at, self.receive.slots.phys(self.received));
            ring.write64(at + 8, 0);
            self.receive_tail.1 = self.received;
            self.received = next(self.received, self.receive.count);
            if taken.is_some() {
                return taken;
            }
        }
        None
    }

    /// Takes back the slots of frames the card has sent.
    fn reclaim(&mut self) {
        while self.unsent != self.transmit_tail {
            let status = self.transmit.descriptors.read8(Ring::at(self.unsent) + 12);
            if status & TX_DD == 0 {
                break;
            }
            self.unsent = next(self.unsent, self.transmit.count);
        }
    }

    /// Whether a frame can be sent now.
    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        next(self.transmit_tail, self.transmit.count) != self.unsent
    }

    /// Copies `frame` into the next transmit slot and fills in its
    /// descriptor, as [`radix_nic::Nic::send`] says.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) || !self.can_send() {
            return false;
        }
        let index = self.transmit_tail;
        self.transmit.slots.write(index, frame);
        let at = Ring::at(index);
        let ring = &self.transmit.descriptors;
        ring.write64(at, self.transmit.slots.phys(index));
        // Legacy: length, checksum offset, command; then status, checksum
        // start and special, all zero. Advanced: length, type, command,
        // then status and the length again.
        ring.write64(at + 8, self.format.transmit_word(frame.len()));
        self.transmit_tail = next(index, self.transmit.count);
        true
    }

    /// Writes the tails that changed since the last time.
    pub fn flush(&mut self) {
        let (told, tail) = self.receive_tail;
        let changed = (told != tail, self.transmit_told != self.transmit_tail);
        if changed.0 || changed.1 {
            device_barrier();
        }
        if changed.0 {
            self.regs.write32(self.offsets.rdt, tail as u32);
            self.receive_tail.0 = tail;
        }
        if changed.1 {
            self.regs.write32(self.offsets.tdt, self.transmit_tail as u32);
            self.transmit_told = self.transmit_tail;
        }
    }

    /// Whether a received frame waits.
    pub fn received(&self) -> bool {
        let (status_at, ..) = self.format.receive_fields();
        self.receive.descriptors.read32(Ring::at(self.received) + status_at) & RX_DD != 0
    }

    /// How many received frames were dropped.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
