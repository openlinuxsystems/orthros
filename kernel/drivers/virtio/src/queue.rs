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
//! The driver keeps its free descriptors as a list threaded through their
//! `next` fields. A request is a chain of descriptors ([`Queue::add`]);
//! the driver publishes the chain's head in the available ring and moves
//! the ring's index after a barrier, then rings the queue's doorbell
//! ([`Queue::notify`]). The device writes into the used ring and moves its
//! index; [`Queue::used`] reads past where it read last, after a barrier,
//! and puts the chain's descriptors back on the free list. The indexes run
//! freely and wrap at 2^16; positions are taken modulo the size, a power
//! of two.

use radix_hw::{Dma, Mmio, device_barrier};

use crate::{Error, NO_VECTOR, Transport, common};

/// The most entries a queue is given.
pub const MAX_SIZE: u16 = 256;

const DESC_SIZE: usize = 16;
const FLAG_NEXT: u16 = 1;
const FLAG_WRITE: u16 = 2;

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
    /// `alloc` gives zeroed DMA memory, in pages.
    pub fn new(
        transport: &Transport,
        index: u16,
        max_size: u16,
        vector: u16,
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
            size = 1 << (15 - size.leading_zeros());
        }
        let (avail, used, end) = layout(size);
        let memory = alloc(end.div_ceil(4096)).ok_or(Error::NoMemory)?;
        memory.zero();
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
        };
        for desc in 0..size {
            queue.set_next(desc, (desc + 1) % size);
        }
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

    fn desc(&self, index: u16) -> usize {
        usize::from(index) * DESC_SIZE
    }

    fn next(&self, index: u16) -> u16 {
        self.memory.read16(self.desc(index) + 14)
    }

    fn set_next(&self, index: u16, next: u16) {
        self.memory.write16(self.desc(index) + 14, next);
    }

    /// Hands the device a chain of `buffers`, in order: those it reads
    /// first, then those it writes. Returns the chain's head, which
    /// [`Queue::used`] gives back with it. Call [`Queue::notify`] after
    /// adding.
    pub fn add(&mut self, buffers: &[Buffer]) -> Result<u16, Error> {
        if buffers.is_empty() || buffers.len() > usize::from(self.size) {
            return Err(Error::BadRequest);
        }
        if buffers.len() > usize::from(self.free_count) {
            return Err(Error::Full);
        }
        let head = self.free_head;
        let mut desc = head;
        for (i, buffer) in buffers.iter().enumerate() {
            let at = self.desc(desc);
            let last = i + 1 == buffers.len();
            let mut flags = if buffer.device_writes { FLAG_WRITE } else { 0 };
            if !last {
                flags |= FLAG_NEXT;
            }
            self.memory.write64(at, buffer.phys);
            self.memory.write32(at + 8, buffer.len);
            self.memory.write16(at + 12, flags);
            if last {
                self.free_head = self.next(desc);
            } else {
                desc = self.next(desc);
            }
        }
        self.free_count -= buffers.len() as u16;
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
    /// device wrote. Its descriptors are free again.
    pub fn used(&mut self) -> Option<(u16, u32)> {
        let device_idx = self.memory.read16(self.used + 2);
        if device_idx == self.last_used {
            return None;
        }
        device_barrier();
        let slot = self.used + 4 + 8 * usize::from(self.last_used % self.size);
        let head = self.memory.read32(slot) as u16;
        let len = self.memory.read32(slot + 4);
        self.last_used = self.last_used.wrapping_add(1);
        if head < self.size {
            self.release(head);
        }
        Some((head, len))
    }

    /// Puts the chain from `head` back on the free list.
    fn release(&mut self, head: u16) {
        let mut desc = head;
        let mut count = 1;
        while self.memory.read16(self.desc(desc) + 12) & FLAG_NEXT != 0 && count < self.size {
            desc = self.next(desc);
            count += 1;
        }
        self.set_next(desc, self.free_head);
        self.free_head = head;
        self.free_count += count;
    }
}
