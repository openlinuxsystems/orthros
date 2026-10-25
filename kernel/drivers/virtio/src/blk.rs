// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The virtio block device (virtio 1.2, section 5.2).
//!
//! A request is a chain of three or more buffers: a 16-byte header the
//! device reads (the request type, and the first 512-byte sector), the
//! data (which the device writes for a read and reads for a write), and a
//! status byte it writes. Headers and status bytes live in one block of
//! DMA memory, in a slot per descriptor number: a chain's head is unique
//! among the chains in flight, so its slot is free. The data are the
//! caller's buffers, by physical address: nothing is copied (spec 6.3,
//! zero-copy I/O); a discard's one range lives in the slot too. The
//! driver uses one queue, with its completions on MSI-X entry `vector`,
//! and asks for no other feature than read-only disks, the block size,
//! cache flushes, discards and the segment limits.

use radix_hw::{Dma, device_barrier};

use crate::queue::{Buffer, Queue};
use crate::{Error, Transport, feature as common_feature, status};

/// Block device feature bits.
pub mod feature {
    /// The device limits a segment's size (`size_max`).
    pub const SIZE_MAX: u64 = 1 << 1;
    /// The device limits a request's segments (`seg_max`).
    pub const SEG_MAX: u64 = 1 << 2;
    /// The disk is read-only.
    pub const RO: u64 = 1 << 5;
    /// The device says its block size (`blk_size`).
    pub const BLK_SIZE: u64 = 1 << 6;
    /// The device has a write cache that a flush request empties.
    pub const FLUSH: u64 = 1 << 9;
    /// The device discards sectors (`max_discard_sectors`).
    pub const DISCARD: u64 = 1 << 13;
}

/// The size of a sector, the unit of block addresses.
pub const SECTOR: u64 = 512;

const TYPE_IN: u32 = 0;
const TYPE_OUT: u32 = 1;
const TYPE_FLUSH: u32 = 4;
const TYPE_DISCARD: u32 = 11;
const HEADER: usize = 16;
/// A discard's range: its first sector, its length and flags (none).
const RANGE: usize = 16;
/// The most data buffers in one request this driver sends, whatever the
/// device takes: the chain is built on the stack, and kernel stacks are
/// small.
pub const MAX_SEGMENTS: u32 = 32;

/// What a request does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Read sectors into the buffers.
    Read,
    /// Write the buffers to sectors.
    Write,
    /// Make every completed write durable.
    Flush,
    /// Discard this many sectors: the device may forget what they hold.
    Discard(u32),
}

/// How a request ended, by its status byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It worked.
    Done,
    /// The device failed to read or write.
    IoError,
    /// The device does not do this kind of request.
    Unsupported,
}

/// What the device is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Its capacity in 512-byte sectors.
    pub sectors: u64,
    /// Its block size, the smallest unit it writes without reading first.
    pub block_size: u32,
    /// Whether it refuses writes.
    pub read_only: bool,
    /// Whether it has a write cache to flush.
    pub flush: bool,
    /// The most data buffers in one request: the device's limit, but at
    /// most [`MAX_SEGMENTS`].
    pub max_segments: u32,
    /// The most bytes in one data buffer (0 for no limit).
    pub max_segment_size: u32,
    /// The most sectors one discard covers, or 0 if it does not discard.
    pub discard_sectors: u32,
}

/// A virtio block device, set up and running.
pub struct Blk {
    transport: Transport,
    queue: Queue,
    slots: Dma,
    info: Info,
}

impl Blk {
    /// Sets the device up: resets it, agrees on features, reads its
    /// configuration, sets up queue 0 with completions on MSI-X entry
    /// `vector`, and tells it the driver is ready. `alloc` gives zeroed
    /// DMA memory, in pages.
    pub fn new(
        transport: Transport,
        vector: u16,
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    ) -> Result<Blk, Error> {
        if !transport.reset() {
            return Err(Error::DeviceFailed);
        }
        transport.add_status(status::ACKNOWLEDGE);
        transport.add_status(status::DRIVER);
        let offered = transport.device_features();
        if offered & common_feature::VERSION_1 == 0 {
            transport.add_status(status::FAILED);
            return Err(Error::MissingFeature("virtio 1.0"));
        }
        let wanted = feature::SIZE_MAX
            | feature::SEG_MAX
            | feature::RO
            | feature::BLK_SIZE
            | feature::FLUSH
            | feature::DISCARD;
        let features = common_feature::VERSION_1 | (offered & wanted);
        if let Err(err) = transport.set_driver_features(features) {
            transport.add_status(status::FAILED);
            return Err(err);
        }
        transport.no_config_interrupt();
        let info = transport.read_config(|config| {
            let low = u64::from(config.read32(0));
            let high = u64::from(config.read32(4));
            let size_max = if features & feature::SIZE_MAX != 0 { config.read32(8) } else { 0 };
            let seg_max = if features & feature::SEG_MAX != 0 { config.read32(12) } else { 0 };
            let block_size =
                if features & feature::BLK_SIZE != 0 { config.read32(20) } else { 512 };
            // A device that discards takes at least one range a request.
            let discard_sectors = if features & feature::DISCARD != 0 && config.read32(40) > 0 {
                config.read32(36)
            } else {
                0
            };
            Info {
                sectors: (high << 32) | low,
                block_size: if block_size >= 512 && block_size.is_power_of_two() {
                    block_size
                } else {
                    512
                },
                read_only: features & feature::RO != 0,
                flush: features & feature::FLUSH != 0,
                max_segments: if seg_max == 0 { u32::MAX } else { seg_max },
                max_segment_size: size_max,
                discard_sectors,
            }
        });
        let queue = match Queue::new(&transport, 0, crate::queue::MAX_SIZE, vector, alloc) {
            Ok(queue) => queue,
            Err(err) => {
                transport.add_status(status::FAILED);
                return Err(err);
            }
        };
        // A request takes three descriptors at least: its header, a
        // buffer and its status.
        if queue.size() < 3 {
            transport.add_status(status::FAILED);
            return Err(Error::NoQueue);
        }
        let size = usize::from(queue.size());
        let Some(slots) = alloc((size * (HEADER + RANGE + 1)).div_ceil(4096)) else {
            transport.reset();
            return Err(Error::NoMemory);
        };
        let max_segments = info.max_segments.min(u32::from(queue.size()) - 2).min(MAX_SEGMENTS);
        let info = Info { max_segments, ..info };
        transport.add_status(status::DRIVER_OK);
        Ok(Blk { transport, queue, slots, info })
    }

    /// What the device is.
    pub fn info(&self) -> Info {
        self.info
    }

    /// Whether the device asks to be reset after an error.
    pub fn failed(&self) -> bool {
        self.transport.status() & status::NEEDS_RESET != 0
    }

    /// How many more requests of `segments` data buffers fit in the queue.
    pub fn room(&self, segments: usize) -> usize {
        usize::from(self.queue.free()) / (segments + 2)
    }

    /// Starts a request: `op` on the sectors from `sector`, with `data`
    /// buffers (physical address, length) that together cover whole
    /// sectors, none for a flush or a discard. Returns its token, which
    /// [`Blk::complete`] gives back. The buffers must stay the request's
    /// alone until then.
    pub fn submit(&mut self, op: Op, sector: u64, data: &[(u64, u32)]) -> Result<u16, Error> {
        let token = self.submit_held(op, sector, data)?;
        self.notify();
        Ok(token)
    }

    /// Adds a request to the queue as [`Blk::submit`] does, but without
    /// ringing the device's doorbell: [`Blk::notify`] does, for every
    /// request held back meanwhile, so that the device takes them
    /// together.
    pub fn submit_held(&mut self, op: Op, sector: u64, data: &[(u64, u32)]) -> Result<u16, Error> {
        let bytes: u64 = data.iter().map(|&(_, len)| u64::from(len)).sum();
        let fits =
            |sectors: u64| sector.checked_add(sectors).is_some_and(|end| end <= self.info.sectors);
        let valid = match op {
            Op::Flush => data.is_empty() && self.info.flush,
            Op::Discard(count) => {
                data.is_empty()
                    && count > 0
                    && count <= self.info.discard_sectors
                    && fits(u64::from(count))
                    && !self.info.read_only
            }
            Op::Read | Op::Write => {
                !data.is_empty()
                    && bytes.is_multiple_of(SECTOR)
                    && data.len() <= self.info.max_segments as usize
                    && data.iter().all(|&(_, len)| {
                        len > 0
                            && (self.info.max_segment_size == 0
                                || len <= self.info.max_segment_size)
                    })
                    && fits(bytes / SECTOR)
                    && !(op == Op::Write && self.info.read_only)
            }
        };
        if !valid {
            return Err(Error::BadRequest);
        }
        let buffers = if matches!(op, Op::Discard(_)) { 1 } else { data.len() };
        if usize::from(self.queue.free()) < buffers + 2 {
            return Err(Error::Full);
        }
        let mut chain =
            [Buffer { phys: 0, len: 0, device_writes: false }; MAX_SEGMENTS as usize + 2];
        let size = usize::from(self.queue.size());
        // The head is the next free descriptor, whose slot is free: fill
        // in the header and clear the status before handing them over.
        let head = self.peek_head();
        let header = usize::from(head) * HEADER;
        let range = size * HEADER + usize::from(head) * RANGE;
        let status = self.status_at(head);
        let kind = match op {
            Op::Read => TYPE_IN,
            Op::Write => TYPE_OUT,
            Op::Flush => TYPE_FLUSH,
            Op::Discard(_) => TYPE_DISCARD,
        };
        self.slots.write32(header, kind);
        self.slots.write32(header + 4, 0);
        // Only reads and writes say where in the header.
        let at = if matches!(op, Op::Read | Op::Write) { sector } else { 0 };
        self.slots.write64(header + 8, at);
        self.slots.write8(status, 0xff);
        chain[0] = Buffer {
            phys: self.slots.phys() + header as u64,
            len: HEADER as u32,
            device_writes: false,
        };
        if let Op::Discard(count) = op {
            self.slots.write64(range, sector);
            self.slots.write32(range + 8, count);
            self.slots.write32(range + 12, 0);
            chain[1] = Buffer {
                phys: self.slots.phys() + range as u64,
                len: RANGE as u32,
                device_writes: false,
            };
        }
        for (i, &(phys, len)) in data.iter().enumerate() {
            chain[1 + i] = Buffer { phys, len, device_writes: op == Op::Read };
        }
        chain[1 + buffers] =
            Buffer { phys: self.slots.phys() + status as u64, len: 1, device_writes: true };
        let added = self.queue.add(&chain[..buffers + 2])?;
        debug_assert_eq!(added, head);
        Ok(added)
    }

    /// Rings the device's doorbell for the requests added since it last
    /// rang.
    pub fn notify(&self) {
        self.queue.notify();
    }

    /// Where the status byte of the chain at `head` is in the slots: after
    /// the headers and the discards' ranges.
    fn status_at(&self, head: u16) -> usize {
        usize::from(self.queue.size()) * (HEADER + RANGE) + usize::from(head)
    }

    /// The descriptor [`Queue::add`] will use as the next chain's head.
    fn peek_head(&self) -> u16 {
        self.queue.next_free()
    }

    /// The next finished request: its token and how it ended.
    pub fn complete(&mut self) -> Option<(u16, Outcome)> {
        let (head, _) = self.queue.used()?;
        device_barrier();
        let outcome = match self.slots.read8(self.status_at(head)) {
            0 => Outcome::Done,
            2 => Outcome::Unsupported,
            _ => Outcome::IoError,
        };
        Some((head, outcome))
    }

    /// Resets the device: it stops using every buffer it was given.
    pub fn reset(&self) {
        self.transport.reset();
    }

    /// How many completions the device gave that named no request in
    /// flight, and were skipped: a device that misbehaves.
    pub fn bad_entries(&self) -> u32 {
        self.queue.bad_entries()
    }
}

impl Drop for Blk {
    /// Resets the device before its queue's memory and the requests'
    /// headers are given back, so that it writes to none of them after.
    fn drop(&mut self) {
        self.transport.reset();
    }
}
