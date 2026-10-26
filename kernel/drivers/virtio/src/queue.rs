// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Split virtqueues (virtio 1.2, section 2.7).
//!
//! A queue of `size` entries is three areas in one block of DMA memory:
//!
//! - the descriptor table: `size` descriptors of 16 bytes (a buffer's
//!   physical address, length, flags and the next descriptor of its
//!   chain), which only the driver writes;
//! - the available ring: flags, an index, and `size` descriptor numbers,
//!   heads of chains the driver hands to the device;
//! - the used ring: flags, an index, and `size` (head, length) pairs the
//!   device hands back.
//!
//! The driver keeps its free descriptors as a list, and each chain's
//! links and length, in its own memory, never reading back what is
//! shared with the device (as Linux keeps `desc_extra`). A request is a
//! chain of descriptors ([`Queue::add`]). With indirect descriptors
//! (VIRTIO_F_INDIRECT_DESC, 2.7.5.3) the chain goes in a table of its
//! own instead, one for each descriptor of the queue, and takes a single
//! descriptor that points at it: a queue of 256 entries then holds 256
//! requests of any length rather than 256 buffers. The driver publishes the
//! chain's head in the available ring and moves the ring's index after a
//! barrier, then rings the queue's doorbell ([`Queue::notify`]). The
//! device writes into the used ring and moves its index; [`Queue::used`]
//! reads past where it read last, after a barrier, and puts the chain's
//! descriptors back on the free list. A used entry that names no chain in
//! flight (a head out of range, or one given back twice) is the device's
//! mistake: it is skipped and counted, and changes nothing. The indexes
//! run freely and wrap at 2^16; positions are taken modulo the size, a
//! power of two.

use radix_hw::{Dma, Mmio, device_barrier};

use crate::{Error, NO_VECTOR, Transport, common};

/// The most entries a queue is given.
pub const MAX_SIZE: u16 = 256;

const DESC_SIZE: usize = 16;
const FLAG_NEXT: u16 = 1;
const FLAG_WRITE: u16 = 2;
const FLAG_INDIRECT: u16 = 4;

/// A buffer of a request: physical memory the device reads or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffer {
    /// Its physical address.
    pub phys: u64,
    /// Its length in bytes.
    pub len: u32,
    /// Whether the device writes it (else it reads it).
    pub device_writes: bool,
}

/// A split virtqueue.
pub struct Queue {
    size: u16,
    memory: Dma,
    avail: usize,
    used: usize,
    doorbell: Mmio,
    index: u16,
    free_head: u16,
    free_count: u16,
    /// The available index as last published.
    avail_idx: u16,
    /// The used index up to which completions were taken.
    last_used: u16,
    /// Each descriptor's next: in the free list, or in its chain.
    next: [u16; MAX_SIZE as usize],
    /// For the head of each chain in flight, how many descriptors the
    /// chain has; 0 for every other descriptor.
    in_flight: [u16; MAX_SIZE as usize],
    /// Used entries skipped as naming no chain in flight.
    bad: u32,
    /// The tables of indirect chains, one of `table_len` descriptors for
    /// each descriptor of the queue, if the device takes them.
    tables: Option<Dma>,
    table_len: u16,
}

/// Where the available and used rings start, and the bytes all three
/// areas need, for a queue of `size` entries.
pub fn layout(size: u16) -> (usize, usize, usize) {
    let size = usize::from(size);
    let avail = DESC_SIZE * size;
    let used = (avail + 6 + 2 * size).next_multiple_of(64);
    let end = used + 6 + 8 * size;
    (avail, used, end)
}

impl Queue {
    /// Sets up queue `index` of the device with at most `max_size`
    /// entries, raising MSI-X entry `vector` when the device uses buffers.
    /// With `table_len`, the device takes indirect descriptors (the driver
    /// agreed VIRTIO_F_INDIRECT_DESC), and chains of up to that many
    /// buffers go in tables. `alloc` gives zeroed DMA memory, in pages.
    pub fn new(
        transport: &Transport,
        index: u16,
        max_size: u16,
        vector: u16,
        table_len: Option<u16>,
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    ) -> Result<Queue, Error> {
        let config = transport.common();
        if index >= transport.queues() {
            return Err(Error::NoQueue);
        }
        config.write16(common::QUEUE_SELECT, index);
        let offered = config.read16(common::QUEUE_SIZE);
        let mut size = offered.min(max_size).min(MAX_SIZE);
        if size == 0 {
            return Err(Error::NoQueue);
        }
        if !size.is_power_of_two() {
            size = 1 << size.ilog2();
        }
        let (avail, used, end) = layout(size);
        let memory = alloc(end.div_ceil(4096)).ok_or(Error::NoMemory)?;
        memory.zero();
        let table_len = table_len.unwrap_or(0).min(size);
        let tables = match table_len {
            0 | 1 => None,
            len => {
                let bytes = usize::from(size) * usize::from(len) * DESC_SIZE;
                Some(alloc(bytes.div_ceil(4096)).ok_or(Error::NoMemory)?)
            }
        };
        let notify_off = config.read16(common::QUEUE_NOTIFY_OFF);
        let doorbell =
            transport.doorbell(notify_off).ok_or(Error::MissingStructure("notification"))?;
        config.write16(common::QUEUE_SIZE, size);
        config.write64(common::QUEUE_DESC, memory.phys());
        config.write64(common::QUEUE_DRIVER, memory.phys() + avail as u64);
        config.write64(common::QUEUE_DEVICE, memory.phys() + used as u64);
        config.write16(common::QUEUE_MSIX_VECTOR, vector);
        if vector != NO_VECTOR && config.read16(common::QUEUE_MSIX_VECTOR) != vector {
            return Err(Error::NoVector);
        }
        let queue = Queue {
            size,
            memory,
            avail,
            used,
            doorbell,
            index,
            free_head: 0,
            free_count: size,
            avail_idx: 0,
            last_used: 0,
            next: core::array::from_fn(|desc| (desc as u16 + 1) % size),
            in_flight: [0; MAX_SIZE as usize],
            bad: 0,
            table_len: if tables.is_some() { table_len } else { 0 },
            tables,
        };
        config.write16(common::QUEUE_ENABLE, 1);
        Ok(queue)
    }

    /// How many entries the queue has.
    pub fn size(&self) -> u16 {
        self.size
    }

    /// How many descriptors are free.
    pub fn free(&self) -> u16 {
        self.free_count
    }

    /// The descriptor the next [`Queue::add`] uses as its chain's head.
    pub fn next_free(&self) -> u16 {
        self.free_head
    }

    /// How many more chains of `buffers` buffers fit in the queue.
    pub fn room(&self, buffers: usize) -> usize {
        usize::from(self.free_count) / self.descriptors(buffers).max(1)
    }

    /// How many of the queue's descriptors a chain of `buffers` takes: one
    /// in a table, if it fits one.
    fn descriptors(&self, buffers: usize) -> usize {
        if self.tables.is_some() && buffers > 1 && buffers <= usize::from(self.table_len) {
            1
        } else {
            buffers
        }
    }

    /// How many used entries were skipped as naming no chain in flight.
    pub fn bad_entries(&self) -> u32 {
        self.bad
    }

    fn desc(index: u16) -> usize {
        usize::from(index) * DESC_SIZE
    }

    fn next(&self, index: u16) -> u16 {
        self.next[usize::from(index)]
    }

    /// Hands the device a chain of `buffers`, in order: those it reads
    /// first, then those it writes. Returns the chain's head, which
    /// [`Queue::used`] gives back with it. Call [`Queue::notify`] after
    /// adding.
    pub fn add(&mut self, buffers: &[Buffer]) -> Result<u16, Error> {
        if buffers.is_empty() || buffers.len() > usize::from(self.size) {
            return Err(Error::BadRequest);
        }
        let taken = self.descriptors(buffers.len());
        if taken > usize::from(self.free_count) {
            return Err(Error::Full);
        }
        let head = self.free_head;
        if taken == 1
            && buffers.len() > 1
            && let Some(tables) = &self.tables
        {
            // The chain in the head's table, linked by places in it.
            let table = usize::from(head) * usize::from(self.table_len) * DESC_SIZE;
            for (i, buffer) in buffers.iter().enumerate() {
                let at = table + i * DESC_SIZE;
                let last = i + 1 == buffers.len();
                let mut flags = if buffer.device_writes { FLAG_WRITE } else { 0 };
                if !last {
                    flags |= FLAG_NEXT;
                }
                tables.write64(at, buffer.phys);
                tables.write32(at + 8, buffer.len);
                tables.write16(at + 12, flags);
                tables.write16(at + 14, if last { 0 } else { i as u16 + 1 });
            }
            let at = Self::desc(head);
            self.memory.write64(at, tables.phys() + table as u64);
            self.memory.write32(at + 8, (buffers.len() * DESC_SIZE) as u32);
            self.memory.write16(at + 12, FLAG_INDIRECT);
            self.free_head = self.next(head);
        } else {
            let mut desc = head;
            for (i, buffer) in buffers.iter().enumerate() {
                let at = Self::desc(desc);
                let last = i + 1 == buffers.len();
                let mut flags = if buffer.device_writes { FLAG_WRITE } else { 0 };
                if !last {
                    flags |= FLAG_NEXT;
                }
                self.memory.write64(at, buffer.phys);
                self.memory.write32(at + 8, buffer.len);
                self.memory.write16(at + 12, flags);
                let next = self.next(desc);
                if last {
                    self.free_head = next;
                } else {
                    self.memory.write16(at + 14, next);
                    desc = next;
                }
            }
        }
        // At most the queue's size, checked above.
        self.in_flight[usize::from(head)] = taken as u16;
        self.free_count -= taken as u16;
        let slot = self.avail + 4 + 2 * usize::from(self.avail_idx % self.size);
        self.memory.write16(slot, head);
        device_barrier();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.memory.write16(self.avail + 2, self.avail_idx);
        Ok(head)
    }

    /// Tells the device to look at the available ring.
    pub fn notify(&self) {
        device_barrier();
        self.doorbell.write16(0, self.index);
    }

    /// The next chain the device has used: its head and the bytes the
    /// device wrote. Its descriptors are free again. Entries that name no
    /// chain in flight are skipped ([`Queue::bad_entries`]), at most a
    /// queue's worth a call, so that a device writing nonsense cannot
    /// keep the driver here.
    pub fn used(&mut self) -> Option<(u16, u32)> {
        for _ in 0..self.size {
            let device_idx = self.memory.read16(self.used + 2);
            if device_idx == self.last_used {
                return None;
            }
            device_barrier();
            let slot = self.used + 4 + 8 * usize::from(self.last_used % self.size);
            let head = self.memory.read32(slot);
            let len = self.memory.read32(slot + 4);
            self.last_used = self.last_used.wrapping_add(1);
            match u16::try_from(head) {
                Ok(head) if head < self.size && self.in_flight[usize::from(head)] != 0 => {
                    self.release(head);
                    return Some((head, len));
                }
                _ => self.bad = self.bad.saturating_add(1),
            }
        }
        None
    }

    /// Puts the chain from `head`, in flight, back on the free list.
    fn release(&mut self, head: u16) {
        let count = core::mem::take(&mut self.in_flight[usize::from(head)]);
        let mut last = head;
        for _ in 1..count {
            last = self.next(last);
        }
        self.next[usize::from(last)] = self.free_head;
        self.free_head = head;
        self.free_count += count;
    }
}
