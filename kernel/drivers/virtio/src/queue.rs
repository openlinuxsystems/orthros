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
//! (VIRTIO_F_INDIRECT_DESC, 2.7.5.3) a chain of more than one buffer goes
//! in a table of its own instead, from a pool the queue keeps, and takes
//! a single descriptor that points at it: the ring then holds as many
//! requests as there are tables, each of up to a table's length, as
//! Linux's driver sends them. The driver publishes the chain's head in
//! the available ring and moves the ring's index after a barrier, then
//! rings the queue's doorbell ([`Queue::notify`]). The device writes into
//! the used ring and moves its index; [`Queue::used`] reads past where it
//! read last, after a barrier, and puts the chain's descriptors and table
//! back. A used entry that names no chain in flight (a head out of range,
//! or one given back twice) is the device's mistake: it is skipped and
//! counted, and changes nothing. The indexes run freely and wrap at 2^16;
//! positions are taken modulo the size, a power of two.
//!
//! With event indexes (VIRTIO_F_EVENT_IDX, 2.7.7 and 2.7.10), each side
//! says up to where it has looked: the driver rings the doorbell only for
//! buffers past the index the device last asked about, and the device
//! interrupts only for used buffers past the one the driver asked about,
//! which [`Queue::used`] moves on only once it has read every used buffer.
//! The device's many completions of one batch then raise one interrupt,
//! not one each. Without them, the driver leaves the doorbell alone while
//! the device says it needs none (`VIRTQ_USED_F_NO_NOTIFY`).

use radix_hw::{Dma, Mmio, device_barrier};

use crate::{Error, NO_VECTOR, Transport, common, feature};

/// The most entries a queue is given.
pub const MAX_SIZE: u16 = 256;

/// The most indirect tables a queue keeps.
pub const MAX_TABLES: usize = 64;

const DESC_SIZE: usize = 16;
const FLAG_NEXT: u16 = 1;
const FLAG_WRITE: u16 = 2;
const FLAG_INDIRECT: u16 = 4;
/// In the used ring's flags: the device needs no doorbell.
const USED_NO_NOTIFY: u16 = 1;
/// No table, in `table_of`.
const NO_TABLE: u8 = u8::MAX;

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
    /// The pool of tables for indirect chains, `table_len` descriptors
    /// each, if the device takes them.
    tables: Option<Dma>,
    table_len: u16,
    /// The free tables: the first `free_tables` entries, a stack.
    free_list: [u8; MAX_TABLES],
    free_tables: usize,
    /// For the head of each chain in flight, its table, or [`NO_TABLE`].
    table_of: [u8; MAX_SIZE as usize],
    /// Whether the driver and the device use event indexes.
    event_idx: bool,
    /// The available index at the last doorbell.
    kicked: u16,
    /// The used index after which the driver last asked for an
    /// interrupt, with event indexes.
    asked: u16,
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
    /// `features` are those the driver agreed: with indirect descriptors
    /// among them, `tables` (a count, at most [`MAX_TABLES`], and a length
    /// in descriptors, at most the queue's size) is the pool of tables
    /// that chains of more than one buffer go in, and with event indexes,
    /// the queue uses them. A pool that cannot be had leaves every chain
    /// in the ring. `alloc` gives zeroed DMA memory, in pages.
    pub fn new(
        transport: &Transport,
        index: u16,
        max_size: u16,
        vector: u16,
        features: u64,
        tables: (usize, u16),
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
        let (count, table_len) = (tables.0.min(MAX_TABLES), tables.1.min(size));
        let tables = (features & feature::INDIRECT_DESC != 0 && count > 0 && table_len > 1)
            .then(|| alloc((count * usize::from(table_len) * DESC_SIZE).div_ceil(4096)))
            .flatten();
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
            free_list: core::array::from_fn(|table| table as u8),
            free_tables: if tables.is_some() { count } else { 0 },
            table_of: [NO_TABLE; MAX_SIZE as usize],
            tables,
            event_idx: features & feature::EVENT_IDX != 0,
            kicked: 0,
            asked: 0,
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

    /// How many more chains of `buffers` buffers fit in the queue: as
    /// many as there are free tables, for chains that go in one.
    pub fn room(&self, buffers: usize) -> usize {
        if self.in_table(buffers) {
            self.free_tables.min(usize::from(self.free_count))
        } else {
            usize::from(self.free_count) / buffers.max(1)
        }
    }

    /// The most buffers a chain in a table has, or 0 without tables.
    pub fn table_len(&self) -> u16 {
        self.table_len
    }

    /// Whether a chain of `buffers` goes in a table.
    fn in_table(&self, buffers: usize) -> bool {
        self.tables.is_some() && buffers > 1 && buffers <= usize::from(self.table_len)
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

    /// Hands the device a chain of `count` buffers, `buffer(0)` first, in
    /// order: those it reads first, then those it writes. Returns the
    /// chain's head, which [`Queue::used`] gives back with it. Call
    /// [`Queue::notify`] after adding.
    pub fn add(&mut self, count: usize, buffer: impl Fn(usize) -> Buffer) -> Result<u16, Error> {
        if count == 0 || count > usize::from(self.size) {
            return Err(Error::BadRequest);
        }
        if self.room(count) == 0 {
            return Err(Error::Full);
        }
        let head = self.free_head;
        let taken = if self.in_table(count)
            && let Some(tables) = &self.tables
        {
            // The chain in a free table, linked by places in it.
            self.free_tables -= 1;
            let number = self.free_list[self.free_tables];
            self.table_of[usize::from(head)] = number;
            let table = usize::from(number) * usize::from(self.table_len) * DESC_SIZE;
            for i in 0..count {
                let Buffer { phys, len, device_writes } = buffer(i);
                let at = table + i * DESC_SIZE;
                let last = i + 1 == count;
                let mut flags = if device_writes { FLAG_WRITE } else { 0 };
                if !last {
                    flags |= FLAG_NEXT;
                }
                tables.write64(at, phys);
                tables.write32(at + 8, len);
                tables.write16(at + 12, flags);
                tables.write16(at + 14, if last { 0 } else { i as u16 + 1 });
            }
            let at = Self::desc(head);
            self.memory.write64(at, tables.phys() + table as u64);
            self.memory.write32(at + 8, (count * DESC_SIZE) as u32);
            self.memory.write16(at + 12, FLAG_INDIRECT);
            self.free_head = self.next(head);
            1
        } else {
            let mut desc = head;
            for i in 0..count {
                let Buffer { phys, len, device_writes } = buffer(i);
                let at = Self::desc(desc);
                let last = i + 1 == count;
                let mut flags = if device_writes { FLAG_WRITE } else { 0 };
                if !last {
                    flags |= FLAG_NEXT;
                }
                self.memory.write64(at, phys);
                self.memory.write32(at + 8, len);
                self.memory.write16(at + 12, flags);
                let next = self.next(desc);
                if last {
                    self.free_head = next;
                } else {
                    self.memory.write16(at + 14, next);
                    desc = next;
                }
            }
            count
        };
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

    /// Tells the device to look at the available ring, if it has not
    /// seen what was added since the last time and has not said it will.
    pub fn notify(&mut self) {
        let (old, new) = (self.kicked, self.avail_idx);
        if old == new {
            return;
        }
        // The new index before what the device said, which it may have
        // written before it saw that index.
        device_barrier();
        self.kicked = new;
        let needed = if self.event_idx {
            let event = self.memory.read16(self.used + 4 + 8 * usize::from(self.size));
            need_event(event, new, old)
        } else {
            self.memory.read16(self.used) & USED_NO_NOTIFY == 0
        };
        if needed {
            self.doorbell.write16(0, self.index);
        }
    }

    /// The next chain the device has used: its head and the bytes the
    /// device wrote. Its descriptors are free again. Entries that name no
    /// chain in flight are skipped ([`Queue::bad_entries`]), at most a
    /// queue's worth a call, so that a device writing nonsense cannot
    /// keep the driver here.
    pub fn used(&mut self) -> Option<(u16, u32)> {
        for _ in 0..self.size {
            let mut device_idx = self.memory.read16(self.used + 2);
            if device_idx == self.last_used {
                if !self.event_idx || self.asked == self.last_used {
                    return None;
                }
                // Every used buffer is read: ask for an interrupt at the
                // next one, then look again for one used meanwhile, which
                // the device may have put there before it saw the ask.
                self.asked = self.last_used;
                self.memory.write16(self.avail + 4 + 2 * usize::from(self.size), self.asked);
                device_barrier();
                device_idx = self.memory.read16(self.used + 2);
                if device_idx == self.last_used {
                    return None;
                }
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
        // A queue's worth skipped: the next used buffer, of whatever it
        // gives next, interrupts again.
        if self.event_idx {
            self.asked = self.last_used;
            self.memory.write16(self.avail + 4 + 2 * usize::from(self.size), self.asked);
        }
        None
    }

    /// Puts the chain from `head`, in flight, and its table back.
    fn release(&mut self, head: u16) {
        let table = core::mem::replace(&mut self.table_of[usize::from(head)], NO_TABLE);
        if table != NO_TABLE {
            self.free_list[self.free_tables] = table;
            self.free_tables += 1;
        }
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

/// Whether the other side asked to be told of index `new`, having asked
/// about `event`, when it was last told of `old` (virtio 1.2, 2.7.7.2 and
/// 2.7.10.2, as `vring_need_event`): whether `event` is in `old..new`,
/// with the indexes wrapping.
fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}
