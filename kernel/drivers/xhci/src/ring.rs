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
//!   barrier, so the controller never sees half of one.
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

/// A ring the driver fills: the command ring or a transfer ring.
pub struct Ring {
    memory: Dma,
    /// Where the next TRB goes, and the cycle state it is marked with.
    enqueue: usize,
    cycle: bool,
}

impl Ring {
    /// A ring in `memory`, a page or more, of which it uses the first
    /// page; the Link TRB at its end points back to its start.
    pub fn new(memory: Dma) -> Option<Ring> {
        if memory.len() < PAGE_SIZE {
            return None;
        }
        memory.zero();
        let link = (RING_TRBS - 1) * TRB_SIZE;
        memory.write64(link, memory.phys());
        // Not handed over yet: its cycle bit is not the ring's state.
        memory.write32(link + 12, control::kind(kind::LINK) | control::TOGGLE_CYCLE);
        Some(Ring { memory, enqueue: 0, cycle: true })
    }

    /// The physical address of its start.
    pub fn phys(&self) -> u64 {
        self.memory.phys()
    }

    /// Where the controller should read next, with the cycle state in bit
    /// 0: what CRCR and an endpoint's TR Dequeue Pointer take.
    pub fn dequeue_pointer(&self) -> u64 {
        self.memory.phys() + (self.enqueue * TRB_SIZE) as u64 | u64::from(self.cycle)
    }

    /// Hands `trb` to the controller (once its doorbell is rung), marked
    /// with the cycle state; returns the physical address it went to,
    /// which the controller's events name. Moves through the Link TRB
    /// when it is reached, handing it over and toggling the state.
    pub fn push(&mut self, trb: Trb) -> u64 {
        let at = self.enqueue * TRB_SIZE;
        self.memory.write64(at, trb.parameter);
        self.memory.write32(at + 8, trb.status);
        device_barrier();
        self.memory.write32(at + 12, (trb.control & !control::CYCLE) | u32::from(self.cycle));
        let phys = self.memory.phys() + at as u64;
        self.enqueue += 1;
        if self.enqueue == RING_TRBS - 1 {
            let link = self.enqueue * TRB_SIZE;
            device_barrier();
            self.memory.write32(
                link + 12,
                control::kind(kind::LINK) | control::TOGGLE_CYCLE | u32::from(self.cycle),
            );
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }
        phys
    }

    /// Whether `phys` is the address of one of its TRBs.
    pub fn holds(&self, phys: u64) -> bool {
        phys >= self.memory.phys()
            && phys < self.memory.phys() + PAGE_SIZE as u64
            && (phys - self.memory.phys()).is_multiple_of(TRB_SIZE as u64)
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
        segment.zero();
        table.zero();
        table.write64(0, segment.phys());
        table.write32(8, EVENT_TRBS as u32);
        Some(EventRing { segment, table, dequeue: 0, cycle: true })
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
    pub fn next(&mut self) -> Option<Trb> {
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
