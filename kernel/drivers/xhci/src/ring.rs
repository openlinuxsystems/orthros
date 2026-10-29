// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Transfer Request Blocks and the rings that hold them (xHCI 4.9, 6.4).
//!
//! A TRB is 16 bytes: a 64-bit parameter (a buffer's address, or the
//! eight bytes of a setup packet), a 32-bit status (a length), and a
//! 32-bit control word with its type and the cycle bit. Whoever fills a
//! ring marks each TRB it hands over with its *cycle state*, and the other
//! side takes TRBs up to the first whose cycle bit is not that state, so
//! neither needs to read the other's index.
//!
//! - [`Ring`] is a ring the driver fills and the controller reads: the
//!   command ring, and a transfer ring for each endpoint. It is one page,
//!   255 TRBs and a Link TRB back to the start that toggles the cycle
//!   state (TC). The driver writes a TRB's control word last, after a
//!   barrier, so the controller never sees half of one, and a TD's first
//!   TRB's last of all, so it never sees half of a TD.
//! - [`StreamRings`] are the rings of an endpoint's streams, 2 KiB each,
//!   and the Stream Context Array that lists them (4.12).
//! - [`EventRing`] is the ring the controller fills and the driver reads:
//!   one segment of a page, 256 events, listed in a one-entry Event Ring
//!   Segment Table. The controller wraps from the last event to the first
//!   and toggles its cycle state; the driver tells it how far it has read
//!   by writing the interrupter's ERDP.

use radix_hw::{Dma, PAGE_SIZE, device_barrier};

/// The bytes of a TRB.
pub const TRB_SIZE: usize = 16;
/// TRBs in a ring's page, the Link TRB included.
pub const RING_TRBS: usize = PAGE_SIZE / TRB_SIZE;
/// Events in the event ring's segment.
pub const EVENT_TRBS: usize = PAGE_SIZE / TRB_SIZE;

/// TRB types (xHCI table 6-91).
pub mod kind {
    /// Normal: a buffer of a bulk or interrupt transfer.
    pub const NORMAL: u32 = 1;
    /// A control transfer's setup stage.
    pub const SETUP_STAGE: u32 = 2;
    /// A control transfer's data stage.
    pub const DATA_STAGE: u32 = 3;
    /// A control transfer's status stage.
    pub const STATUS_STAGE: u32 = 4;
    /// Link: the ring goes on at the parameter's address.
    pub const LINK: u32 = 6;
    /// Event Data: raises an event that carries its parameter.
    pub const EVENT_DATA: u32 = 7;
    /// No Op, on a transfer ring.
    pub const NO_OP: u32 = 8;
    /// Enable Slot: gives a device slot.
    pub const ENABLE_SLOT: u32 = 9;
    /// Disable Slot: takes it back.
    pub const DISABLE_SLOT: u32 = 10;
    /// Address Device: sets up a slot from an input context and gives the
    /// device its address.
    pub const ADDRESS_DEVICE: u32 = 11;
    /// Configure Endpoint: adds and drops endpoints.
    pub const CONFIGURE_ENDPOINT: u32 = 12;
    /// Evaluate Context: takes changed fields of the slot and endpoint 0.
    pub const EVALUATE_CONTEXT: u32 = 13;
    /// Reset Endpoint: brings a halted endpoint back.
    pub const RESET_ENDPOINT: u32 = 14;
    /// Stop Endpoint: stops an endpoint's transfers.
    pub const STOP_ENDPOINT: u32 = 15;
    /// Set TR Dequeue Pointer: moves where a stopped endpoint reads.
    pub const SET_TR_DEQUEUE: u32 = 16;
    /// Reset Device: back to the addressed state after a port reset.
    pub const RESET_DEVICE: u32 = 17;
    /// No Op, on the command ring.
    pub const NO_OP_COMMAND: u32 = 23;
    /// Transfer Event.
    pub const TRANSFER_EVENT: u32 = 32;
    /// Command Completion Event.
    pub const COMMAND_COMPLETION: u32 = 33;
    /// Port Status Change Event.
    pub const PORT_STATUS_CHANGE: u32 = 34;
    /// Bandwidth Request Event.
    pub const BANDWIDTH_REQUEST: u32 = 35;
    /// Doorbell Event (virtualization).
    pub const DOORBELL: u32 = 36;
    /// Host Controller Event: the event ring filled, among others.
    pub const HOST_CONTROLLER: u32 = 37;
    /// Device Notification Event.
    pub const DEVICE_NOTIFICATION: u32 = 38;
    /// MFINDEX Wrap Event.
    pub const MFINDEX_WRAP: u32 = 39;
}

/// The control word's fields.
pub mod control {
    /// The cycle bit.
    pub const CYCLE: u32 = 1 << 0;
    /// Link TRB: toggle the cycle state when following it.
    pub const TOGGLE_CYCLE: u32 = 1 << 1;
    /// Interrupt on short packet: a transfer that ends short raises an
    /// event for this TRB.
    pub const ISP: u32 = 1 << 2;
    /// Chain: the TD goes on in the next TRB.
    pub const CHAIN: u32 = 1 << 4;
    /// Interrupt on completion: raise an event when this TRB is done.
    pub const IOC: u32 = 1 << 5;
    /// Immediate data: the parameter is the data (a setup packet).
    pub const IDT: u32 = 1 << 6;
    /// Address Device: block SET_ADDRESS.
    pub const BSR: u32 = 1 << 9;
    /// Configure Endpoint: deconfigure.
    pub const DECONFIGURE: u32 = 1 << 9;
    /// Reset Endpoint: transfer state preserved.
    pub const TSP: u32 = 1 << 9;
    /// Data and Status Stage: the direction is IN.
    pub const DIR_IN: u32 = 1 << 16;
    /// Setup Stage: no data stage.
    pub const TRT_NONE: u32 = 0;
    /// Setup Stage: an OUT data stage.
    pub const TRT_OUT: u32 = 2 << 16;
    /// Setup Stage: an IN data stage.
    pub const TRT_IN: u32 = 3 << 16;
    /// Stop Endpoint: suspend.
    pub const SUSPEND: u32 = 1 << 23;

    /// A TRB of type `kind`, as its control word holds it.
    pub const fn kind(kind: u32) -> u32 {
        kind << 10
    }

    /// The slot a command names.
    pub const fn slot(slot: u8) -> u32 {
        (slot as u32) << 24
    }

    /// The endpoint (device context index) a command names.
    pub const fn endpoint(dci: u8) -> u32 {
        (dci as u32) << 16
    }
}

/// A Transfer Request Block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trb {
    /// The parameter: an address, or immediate data.
    pub parameter: u64,
    /// The status: a length, or an event's completion code.
    pub status: u32,
    /// The control word, without the cycle bit when the driver makes one.
    pub control: u32,
}

impl Trb {
    /// Its type.
    pub fn kind(&self) -> u32 {
        (self.control >> 10) & 0x3f
    }

    /// An event's completion code.
    pub fn code(&self) -> u8 {
        (self.status >> 24) as u8
    }

    /// An event's slot.
    pub fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }

    /// A transfer event's endpoint (device context index).
    pub fn endpoint(&self) -> u8 {
        ((self.control >> 16) & 0x1f) as u8
    }
}

/// Where the driver puts a ring's next TRB, and the cycle state it marks
/// TRBs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cursor {
    enqueue: u8,
    cycle: bool,
}

impl Cursor {
    const START: Cursor = Cursor { enqueue: 0, cycle: true };
}

/// A ring of `trbs` TRBs at `offset` in `memory`, the last of them a Link
/// TRB back to its start, as its driver fills it from `cursor`.
struct Span<'a> {
    memory: &'a Dma,
    offset: usize,
    trbs: usize,
    cursor: &'a mut Cursor,
}

impl Span<'_> {
    /// The physical address of its start.
    fn phys(&self) -> u64 {
        self.memory.phys() + self.offset as u64
    }

    /// Empties it, as new.
    fn reset(&mut self) {
        let (offset, link) = (self.offset, self.offset + (self.trbs - 1) * TRB_SIZE);
        for at in (offset..link + TRB_SIZE).step_by(8) {
            self.memory.write64(at, 0);
        }
        self.memory.write64(link, self.phys());
        // Not handed over yet: its cycle bit is not the ring's state.
        self.memory.write32(link + 12, control::kind(kind::LINK) | control::TOGGLE_CYCLE);
        *self.cursor = Cursor::START;
    }

    /// Where the next TRB goes, in bytes from the start of `memory`.
    fn at(&self) -> usize {
        self.offset + usize::from(self.cursor.enqueue) * TRB_SIZE
    }

    /// Moves to the next TRB; at the Link TRB, hands it over, with Chain
    /// set if `chain`, and goes on from the start in the other state.
    fn advance(&mut self, chain: u32) {
        self.cursor.enqueue += 1;
        if usize::from(self.cursor.enqueue) == self.trbs - 1 {
            let link = self.at();
            device_barrier();
            self.memory.write32(
                link + 12,
                control::kind(kind::LINK)
                    | control::TOGGLE_CYCLE
                    | chain
                    | u32::from(self.cursor.cycle),
            );
            self.cursor.enqueue = 0;
            self.cursor.cycle = !self.cursor.cycle;
        }
    }

    fn push(&mut self, trb: Trb) -> u64 {
        let at = self.at();
        self.memory.write64(at, trb.parameter);
        self.memory.write32(at + 8, trb.status);
        device_barrier();
        self.memory
            .write32(at + 12, (trb.control & !control::CYCLE) | u32::from(self.cursor.cycle));
        let phys = self.memory.phys() + at as u64;
        self.advance(0);
        phys
    }

    fn push_td(&mut self, count: usize, trb: impl Fn(usize) -> Trb) -> (u64, u64) {
        let first_at = self.at();
        let first_cycle = self.cursor.cycle;
        let chain = |i: usize| if i + 1 < count { control::CHAIN } else { 0 };
        let mut last = 0;
        for i in 0..count {
            let this = trb(i);
            let at = self.at();
            self.memory.write64(at, this.parameter);
            self.memory.write32(at + 8, this.status);
            // The first stays the controller's until the rest are written.
            let cycle = if i == 0 { !self.cursor.cycle } else { self.cursor.cycle };
            device_barrier();
            self.memory
                .write32(at + 12, (this.control & !control::CYCLE) | chain(i) | u32::from(cycle));
            last = self.memory.phys() + at as u64;
            self.advance(chain(i));
        }
        if count > 0 {
            device_barrier();
            let control = (trb(0).control & !control::CYCLE) | chain(0) | u32::from(first_cycle);
            self.memory.write32(first_at + 12, control);
        }
        (self.memory.phys() + first_at as u64, last)
    }
}

/// The dequeue pointer of the ring at `offset` in `memory` that `cursor`
/// fills: where the controller should read next, with the cycle state in
/// bit 0.
fn dequeue_pointer(memory: &Dma, offset: usize, cursor: Cursor) -> u64 {
    (memory.phys() + (offset + usize::from(cursor.enqueue) * TRB_SIZE) as u64)
        | u64::from(cursor.cycle)
}

/// A ring the driver fills: the command ring or a transfer ring.
pub struct Ring {
    memory: Dma,
    cursor: Cursor,
}

impl Ring {
    /// A ring in `memory`, a page or more, of which it uses the first
    /// page; the Link TRB at its end points back to its start.
    pub fn new(memory: Dma) -> Option<Ring> {
        if memory.len() < PAGE_SIZE {
            return None;
        }
        memory.zero();
        let mut ring = Ring { memory, cursor: Cursor::START };
        ring.reset();
        Some(ring)
    }

    fn span(&mut self) -> Span<'_> {
        Span { memory: &self.memory, offset: 0, trbs: RING_TRBS, cursor: &mut self.cursor }
    }

    /// Empties it, as new: for a ring the controller has let go of, which
    /// it starts reading again from the start.
    pub fn reset(&mut self) {
        self.span().reset();
    }

    /// The physical address of its start.
    pub fn phys(&self) -> u64 {
        self.memory.phys()
    }

    /// The memory it is in, of which it uses the first page.
    pub fn memory(&self) -> &Dma {
        &self.memory
    }

    /// Where the controller should read next, with the cycle state in bit
    /// 0: what CRCR and an endpoint's TR Dequeue Pointer take.
    pub fn dequeue_pointer(&self) -> u64 {
        dequeue_pointer(&self.memory, 0, self.cursor)
    }

    /// Hands `trb` to the controller (once its doorbell is rung), marked
    /// with the cycle state; returns the physical address it went to,
    /// which the controller's events name. Moves through the Link TRB
    /// when it is reached, handing it over and toggling the state.
    pub fn push(&mut self, trb: Trb) -> u64 {
        self.span().push(trb)
    }

    /// Hands a TD of `count` TRBs, the `i`th `trb(i)`, to the controller
    /// as [`Ring::push`] does each TRB, chained: Chain on every TRB but the
    /// last, and on a Link TRB the TD goes through (4.11.5.1). Its first
    /// TRB's cycle bit is written last, so that a controller reading the
    /// ring never starts on half of it. Returns the physical addresses of
    /// its first and last TRBs.
    pub fn push_td(&mut self, count: usize, trb: impl Fn(usize) -> Trb) -> (u64, u64) {
        self.span().push_td(count, trb)
    }

    /// Whether `phys` is the address of one of its TRBs.
    pub fn holds(&self, phys: u64) -> bool {
        phys >= self.memory.phys()
            && phys < self.memory.phys() + PAGE_SIZE as u64
            && (phys - self.memory.phys()).is_multiple_of(TRB_SIZE as u64)
    }
}

/// The bytes of a stream's ring.
pub const STREAM_RING_SIZE: usize = 2048;
/// TRBs in a stream's ring, the Link TRB included.
pub const STREAM_RING_TRBS: usize = STREAM_RING_SIZE / TRB_SIZE;
/// The most streams an endpoint is given: as many as fit, after the Stream
/// Context Array, in 64 KiB.
pub const MAX_STREAMS: usize = 31;
/// The bytes of a Stream Context.
const STREAM_CONTEXT_SIZE: usize = 16;
/// A Stream Context's type: a Primary Transfer Ring (6.2.4.1).
const SCT_PRIMARY: u64 = 1 << 1;

/// The rings of an endpoint's streams (xHCI 4.12), which a USB Attached
/// SCSI device moves each command's status and data on, and its Stream
/// Context Array, which the controller finds them through: the array in
/// the first 2 KiB of the memory, and each stream's ring after it, stream
/// `s`'s at `s` times 2 KiB. A stream carries one TD at a time, so a
/// ring of 128 TRBs is plenty.
pub struct StreamRings {
    memory: Dma,
    /// The streams, numbered from 1.
    count: u8,
    cursors: [Cursor; MAX_STREAMS + 1],
}

impl StreamRings {
    /// The bytes `count` streams take.
    pub fn bytes(count: usize) -> usize {
        (count + 1) * STREAM_RING_SIZE
    }

    /// `count` streams (at most [`MAX_STREAMS`]) in `memory`, at least
    /// [`StreamRings::bytes`] of it, each ring empty and in its context.
    pub fn new(memory: Dma, count: usize) -> Option<StreamRings> {
        if count == 0 || count > MAX_STREAMS || memory.len() < StreamRings::bytes(count) {
            return None;
        }
        let mut rings =
            StreamRings { memory, count: count as u8, cursors: [Cursor::START; MAX_STREAMS + 1] };
        rings.reset();
        Some(rings)
    }

    /// How many streams it has, numbered from 1.
    pub fn count(&self) -> usize {
        usize::from(self.count)
    }

    /// The entries of its Stream Context Array: a power of two, stream 0
    /// (which the controller keeps) among them.
    pub fn entries(&self) -> usize {
        (self.count() + 1).next_power_of_two()
    }

    /// The Stream Context Array's physical address, for the endpoint's
    /// context.
    pub fn context_array(&self) -> u64 {
        self.memory.phys()
    }

    fn span(&mut self, stream: usize) -> Span<'_> {
        Span {
            memory: &self.memory,
            offset: stream * STREAM_RING_SIZE,
            trbs: STREAM_RING_TRBS,
            cursor: &mut self.cursors[stream],
        }
    }

    /// Empties every ring, as new, and points each stream's context at
    /// its ring's start: for streams the controller has let go of.
    pub fn reset(&mut self) {
        for at in (0..STREAM_RING_SIZE).step_by(8) {
            self.memory.write64(at, 0);
        }
        for stream in 1..=self.count() {
            self.span(stream).reset();
            let pointer = self.dequeue_pointer(stream).unwrap_or(0);
            self.memory.write64(stream * STREAM_CONTEXT_SIZE, pointer | SCT_PRIMARY);
        }
    }

    /// Where the controller should read stream `stream`'s ring next, with
    /// the cycle state in bit 0, as Set TR Dequeue Pointer takes it; `None`
    /// if it has no such stream.
    pub fn dequeue_pointer(&self, stream: usize) -> Option<u64> {
        (1..=self.count())
            .contains(&stream)
            .then(|| dequeue_pointer(&self.memory, stream * STREAM_RING_SIZE, self.cursors[stream]))
    }

    /// The physical address of the start of stream `stream`'s ring.
    pub fn ring_phys(&self, stream: usize) -> u64 {
        self.memory.phys() + (stream * STREAM_RING_SIZE) as u64
    }

    /// Hands a TD to the controller on stream `stream`'s ring, as
    /// [`Ring::push_td`] does; `None` if it has no such stream.
    pub fn push_td(
        &mut self,
        stream: usize,
        count: usize,
        trb: impl Fn(usize) -> Trb,
    ) -> Option<(u64, u64)> {
        if !(1..=self.count()).contains(&stream) {
            return None;
        }
        Some(self.span(stream).push_td(count, trb))
    }
}

/// The ring the controller fills with events, and its segment table.
pub struct EventRing {
    /// The segment, a page, and the table that lists it.
    segment: Dma,
    table: Dma,
    dequeue: usize,
    cycle: bool,
}

impl EventRing {
    /// An event ring in `segment` (a page) listed in `table` (a page, of
    /// which the first 16 bytes are used).
    pub fn new(segment: Dma, table: Dma) -> Option<EventRing> {
        if segment.len() < PAGE_SIZE || table.len() < 16 {
            return None;
        }
        table.zero();
        table.write64(0, segment.phys());
        table.write32(8, EVENT_TRBS as u32);
        let mut ring = EventRing { segment, table, dequeue: 0, cycle: true };
        ring.reset();
        Some(ring)
    }

    /// Empties it, as new: for a ring the controller writes again from
    /// the start once it is given the table.
    pub fn reset(&mut self) {
        self.segment.zero();
        self.dequeue = 0;
        self.cycle = true;
    }

    /// The table's physical address, for ERSTBA.
    pub fn table_phys(&self) -> u64 {
        self.table.phys()
    }

    /// Where the driver has read up to, for ERDP.
    pub fn dequeue_phys(&self) -> u64 {
        self.segment.phys() + (self.dequeue * TRB_SIZE) as u64
    }

    /// The next event, if the controller has written one.
    pub fn pop(&mut self) -> Option<Trb> {
        let at = self.dequeue * TRB_SIZE;
        let control = self.segment.read32(at + 12);
        if (control & control::CYCLE != 0) != self.cycle {
            return None;
        }
        // The rest of the TRB was written before its cycle bit.
        device_barrier();
        let trb = Trb {
            parameter: self.segment.read64(at),
            status: self.segment.read32(at + 8),
            control,
        };
        self.dequeue += 1;
        if self.dequeue == EVENT_TRBS {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        Some(trb)
    }
}
