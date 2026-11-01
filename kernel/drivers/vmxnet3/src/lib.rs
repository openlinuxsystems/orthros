// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! VMware's paravirtual network card, VMXNET3 (ADR 0197), which VMware's
//! hypervisors give a guest when asked for their fastest card, and QEMU
//! emulates as its `vmxnet3`.
//!
//! The card has no registers of its own beyond a few doorbells (BAR 0)
//! and commands (BAR 1): the driver describes itself and its queues in
//! memory both share, the driver-shared area of revision 1 of the
//! interface, gives the card that area's address and activates it. Each
//! ring's ownership goes by generation bits rather than head and tail
//! registers: a descriptor whose bit matches the ring's current one
//! belongs to whoever is to take it next, and the bit flips each time
//! the ring wraps. The layouts and the sequences here follow FreeBSD's
//! if_vmx (ISC license, credits/freebsd-network-drivers.md) and VMware's
//! public headers.
//!
//! One queue pair: a command ring of [`RECEIVE`] receive buffers, each a
//! [`Slots`] slot (the second ring, for the bodies of frames longer than
//! a buffer, gets none: no frame is), with its completion ring, and a
//! command ring of [`TRANSMIT`] frames to send with its own. Every frame
//! sent asks for a completion, so that its slot comes back. With an MSI,
//! the card masks its one interrupt as it raises it, for frames received
//! and sent and for events, and the driver unmasks it once nothing is
//! left to take; polled, the interrupt stays masked and off.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use radix_hw::{Dma, Mmio, device_barrier};
use radix_nic::{
    Error, Info, Interrupt, Link, MAX_FRAME, MIN_FRAME, Nic, PAGE, SLOT, Slots, next, usable_mac,
};

/// VMware's PCI vendor ID.
pub const VENDOR: u16 = 0x15ad;
/// The card's PCI device ID.
pub const DEVICE: u16 = 0x07b0;

/// BAR 0's doorbells: an interrupt's mask, the transmit ring's producer
/// index and the receive rings'.
mod bar0 {
    pub const IMASK0: usize = 0x000;
    pub const TXPROD0: usize = 0x600;
    pub const RXPROD0: usize = 0x800;
    pub const RXPROD2_0: usize = 0xa00;
    pub const END: usize = RXPROD2_0 + 4;
}

/// BAR 1's registers: the revisions the card has, the driver-shared
/// area's address, commands, the MAC address and the events.
mod bar1 {
    pub const VRRS: usize = 0x000;
    pub const UVRS: usize = 0x008;
    pub const DSAL: usize = 0x010;
    pub const DSAH: usize = 0x018;
    pub const CMD: usize = 0x020;
    pub const MACL: usize = 0x028;
    pub const MACH: usize = 0x030;
    pub const ECR: usize = 0x040;
    pub const END: usize = ECR + 4;
}

/// Commands, written to BAR 1's CMD; those that answer do so in CMD.
mod cmd {
    pub const ACTIVATE: u32 = 0xcafe_0000;
    pub const QUIESCE: u32 = 0xcafe_0001;
    pub const RESET: u32 = 0xcafe_0002;
    pub const UPDATE_RX_MODE: u32 = 0xcafe_0003;
    pub const UPDATE_MAC_FILTERS: u32 = 0xcafe_0004;
    pub const UPDATE_VLAN_FILTERS: u32 = 0xcafe_0005;
    pub const GET_LINK: u32 = 0xf00d_0002;
}

/// The driver-shared area (`struct vmxnet3_driver_shared`, 720 bytes)
/// and where its fields are.
mod shared {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 8;
    pub const GUEST: usize = 12;
    pub const REVISION: usize = 16;
    pub const UPT_VERSION: usize = 20;
    pub const QUEUE_SHARED: usize = 40;
    pub const QUEUE_SHARED_LEN: usize = 52;
    pub const MTU: usize = 56;
    pub const NRXSG_MAX: usize = 60;
    pub const NTXQUEUE: usize = 62;
    pub const NRXQUEUE: usize = 63;
    pub const AUTOMASK: usize = 80;
    pub const NINTR: usize = 81;
    pub const EVINTR: usize = 82;
    pub const MODLEVEL: usize = 83;
    pub const ICTRL: usize = 108;
    pub const RXMODE: usize = 120;
    pub const VLAN_FILTER: usize = 136;
    pub const EVENT: usize = 696;
    pub const SIZE: usize = 720;
    /// Where the queues' descriptions start, after it: one transmit
    /// queue's then one receive queue's, 256 bytes each.
    pub const QUEUES: usize = 1024;
    pub const QUEUES_LEN: usize = 512;
}

// The queues' descriptions come after the area, in its page.
const _: () =
    assert!(shared::SIZE <= shared::QUEUES && shared::QUEUES + shared::QUEUES_LEN <= PAGE);

/// A transmit queue's description (`struct vmxnet3_txq_shared`).
mod txq {
    pub const INTR_THRESHOLD: usize = 4;
    pub const CMD_RING: usize = 16;
    pub const DATA_RING: usize = 24;
    pub const COMP_RING: usize = 32;
    pub const CMD_RING_LEN: usize = 56;
    pub const DATA_RING_LEN: usize = 60;
    pub const COMP_RING_LEN: usize = 64;
    pub const INTR_IDX: usize = 72;
    pub const STOPPED: usize = 80;
}

/// A receive queue's (`struct vmxnet3_rxq_shared`), after it.
mod rxq {
    pub const AT: usize = 256;
    pub const UPDATE_RXHEAD: usize = 0;
    pub const CMD_RING: usize = 16;
    pub const CMD_RING_2: usize = 24;
    pub const COMP_RING: usize = 32;
    pub const CMD_RING_LEN: usize = 56;
    pub const CMD_RING_2_LEN: usize = 60;
    pub const COMP_RING_LEN: usize = 64;
    pub const INTR_IDX: usize = 72;
    pub const STOPPED: usize = 80;
}

/// The magic the driver-shared area starts with, for revision 1.
const MAGIC: u32 = 0xbabe_fee1;
/// The driver's version, which the card only shows its host.
const DRIVER_VERSION: u32 = 0x0001_0000;
/// The guest: a 64-bit one of no type the interface names.
const GUEST_64BIT: u32 = 0x02;
/// Interrupt moderation: adaptive.
const MODERATION_ADAPTIVE: u8 = 8;
/// The interrupt control word's bit that turns every interrupt off.
const ICTRL_DISABLE_ALL: u32 = 0x01;
/// The receive filter: its own address, broadcasts and every multicast
/// frame, which IPv6 and mDNS need.
const RXMODE: u32 = 0x01 | 0x04 | 0x08;

/// The receive buffers the card holds.
pub const RECEIVE: usize = 256;
/// The descriptors of the receive body ring, which gets no buffers but
/// must have a size.
const BODIES: usize = 32;
/// The transmit ring's descriptors, one of which is always free.
pub const TRANSMIT: usize = 256;
/// The bytes of every descriptor, of each kind, and of each entry of the
/// transmit data ring.
const DESCRIPTOR: usize = 16;
const DATA_DESCRIPTOR: usize = 128;

/// A receive descriptor's second word: the buffer's length, its type
/// (bit 14: 0 for a head), and its generation (bit 31).
const RX_GEN: u32 = 1 << 31;
/// A receive completion's first word: the descriptor's index, end and
/// start of packet, and the ring (its queue's number, or that plus the
/// queues for the body ring).
const RCD_EOP: u32 = 1 << 14;
const RCD_SOP: u32 = 1 << 15;
/// Its third: the length, and whether the frame is in error.
const RCD_ERROR: u32 = 1 << 14;
/// A completion's last word holds its generation, in bit 31.
const COMP_GEN: u32 = 1 << 31;
/// A transmit descriptor's third word: its generation (bit 14), and its
/// fourth: end of packet and completion requested.
const TX_GEN: u32 = 1 << 14;
const TX_EOP: u32 = 1 << 12;
const TX_COMPREQ: u32 = 1 << 13;

/// The card at `bar0` and `bar1`, reset and running, with interrupts as
/// `interrupt` says. `fallback_mac` is its address if it has none;
/// `alloc` gives zeroed DMA memory in pages.
pub fn start(
    bar0: Mmio,
    bar1: Mmio,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
) -> Result<Vmxnet3, Error> {
    if bar0.len() < bar0::END || bar1.len() < bar1::END {
        return Err(Error::Registers);
    }
    let revisions = bar1.read32(bar1::VRRS);
    if revisions == u32::MAX {
        return Err(Error::Device("its registers read as all ones"));
    }
    if revisions & 1 == 0 || bar1.read32(bar1::UVRS) & 1 == 0 {
        return Err(Error::Unsupported("revision 1 of the interface"));
    }
    bar1.write32(bar1::VRRS, 1);
    bar1.write32(bar1::UVRS, 1);
    bar1.write32(bar1::CMD, cmd::RESET);
    let mac = {
        let low = bar1.read32(bar1::MACL).to_le_bytes();
        let high = bar1.read32(bar1::MACH).to_le_bytes();
        let address = [low[0], low[1], low[2], low[3], high[0], high[1]];
        if usable_mac(address) { address } else { fallback_mac }
    };
    let rings = Rings::new(alloc)?;
    let area = alloc(1).ok_or(Error::NoMemory)?;
    if area.len() < shared::QUEUES + shared::QUEUES_LEN {
        return Err(Error::NoMemory);
    }
    describe(&area, &rings, interrupt);
    let phys = area.phys();
    bar1.write32(bar1::DSAL, phys as u32);
    bar1.write32(bar1::DSAH, (phys >> 32) as u32);
    device_barrier();
    if command(&bar1, cmd::ACTIVATE) != 0 {
        bar1.write32(bar1::CMD, cmd::RESET);
        return Err(Error::Device("it would not activate"));
    }
    bar0.write32(bar0::RXPROD0, 0);
    bar0.write32(bar0::RXPROD2_0, 0);
    // Its own address, and the filters the driver-shared area holds, as
    // Linux's driver has the card take them.
    bar1.write32(bar1::MACL, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
    bar1.write32(bar1::MACH, u32::from(u16::from_le_bytes([mac[4], mac[5]])));
    bar1.write32(bar1::CMD, cmd::UPDATE_MAC_FILTERS);
    bar1.write32(bar1::CMD, cmd::UPDATE_RX_MODE);
    bar1.write32(bar1::CMD, cmd::UPDATE_VLAN_FILTERS);
    // Polled, the interrupt stays masked, so that the card does not
    // raise its line either.
    bar0.write32(bar0::IMASK0, u32::from(interrupt == Interrupt::Polled));
    let info = Info {
        model: "VMXNET3",
        mac,
        receive_slots: RECEIVE as u16,
        transmit_slots: (TRANSMIT - 1) as u16,
    };
    Ok(Vmxnet3 { bar0, bar1, area, rings, info, interrupt, stopped: false })
}

/// Fills the driver-shared area and the queues' descriptions in `area`.
fn describe(area: &Dma, rings: &Rings, interrupt: Interrupt) {
    area.write32(shared::MAGIC, MAGIC);
    area.write32(shared::VERSION, DRIVER_VERSION);
    area.write32(shared::GUEST, GUEST_64BIT);
    area.write32(shared::REVISION, 1);
    area.write32(shared::UPT_VERSION, 1);
    // No offloads (upt_features 0), and no driver data.
    area.write64(shared::QUEUE_SHARED, area.phys() + shared::QUEUES as u64);
    area.write32(shared::QUEUE_SHARED_LEN, shared::QUEUES_LEN as u32);
    area.write32(shared::MTU, 1500);
    area.write16(shared::NRXSG_MAX, 1);
    area.write8(shared::NTXQUEUE, 1);
    area.write8(shared::NRXQUEUE, 1);
    // One interrupt, for both queues and the events, masked by the card
    // as it raises it; all of them off unless there is an MSI.
    area.write8(shared::AUTOMASK, 1);
    area.write8(shared::NINTR, 1);
    area.write8(shared::EVINTR, 0);
    area.write8(shared::MODLEVEL, MODERATION_ADAPTIVE);
    let ictrl = if interrupt == Interrupt::Msi { 0 } else { ICTRL_DISABLE_ALL };
    area.write32(shared::ICTRL, ictrl);
    // The filters: every frame without a VLAN tag (VLAN 0's bit), every
    // multicast one, broadcasts; no multicast table.
    area.write32(shared::RXMODE, RXMODE);
    area.write32(shared::VLAN_FILTER, 1);
    let tx = shared::QUEUES;
    area.write32(tx + txq::INTR_THRESHOLD, 1);
    area.write64(tx + txq::CMD_RING, rings.transmit.phys());
    area.write64(tx + txq::DATA_RING, rings.transmit_data.phys());
    area.write64(tx + txq::COMP_RING, rings.transmit_done.phys());
    area.write32(tx + txq::CMD_RING_LEN, TRANSMIT as u32);
    area.write32(tx + txq::DATA_RING_LEN, TRANSMIT as u32);
    area.write32(tx + txq::COMP_RING_LEN, TRANSMIT as u32);
    area.write8(tx + txq::INTR_IDX, 0);
    let rx = shared::QUEUES + rxq::AT;
    area.write64(rx + rxq::CMD_RING, rings.receive.phys());
    area.write64(rx + rxq::CMD_RING_2, rings.bodies.phys());
    area.write64(rx + rxq::COMP_RING, rings.received.phys());
    area.write32(rx + rxq::CMD_RING_LEN, RECEIVE as u32);
    area.write32(rx + rxq::CMD_RING_2_LEN, BODIES as u32);
    area.write32(rx + rxq::COMP_RING_LEN, (RECEIVE + BODIES) as u32);
    area.write8(rx + rxq::INTR_IDX, 0);
}

/// Runs command `command`, which answers: its answer.
fn command(bar1: &Mmio, command: u32) -> u32 {
    bar1.write32(bar1::CMD, command);
    bar1.read32(bar1::CMD)
}

/// The rings, and where each stands.
struct Rings {
    receive: Dma,
    bodies: Dma,
    received: Dma,
    receive_slots: Slots,
    transmit: Dma,
    /// The data ring, for headers copied next to the descriptors, which
    /// the driver does not use but every driver gives the card.
    transmit_data: Dma,
    transmit_done: Dma,
    transmit_slots: Slots,
    /// The next receive descriptor to hand back, and the generation it
    /// is handed back with.
    fill: (usize, bool),
    /// The next receive completion to look at, and the generation it has
    /// once the card wrote it.
    completion: (usize, bool),
    /// Whether the receive descriptors handed back since the last flush
    /// need telling the card about.
    filled: bool,
    /// The next transmit descriptor to fill, and its generation.
    send: (usize, bool),
    /// The transmit producer index as the card was last told it.
    told: usize,
    /// The oldest transmit descriptor the card may still be sending.
    unsent: usize,
    /// The next transmit completion to look at, and its generation.
    sent: (usize, bool),
    dropped: u64,
}

/// `count` descriptors' worth of zeroed DMA memory.
fn ring(count: usize, alloc: &mut dyn FnMut(usize) -> Option<Dma>) -> Result<Dma, Error> {
    let memory = alloc((count * DESCRIPTOR).div_ceil(PAGE)).ok_or(Error::NoMemory)?;
    if memory.len() < count * DESCRIPTOR {
        return Err(Error::NoMemory);
    }
    Ok(memory)
}

impl Rings {
    /// The rings, every receive buffer handed to the card.
    fn new(alloc: &mut dyn FnMut(usize) -> Option<Dma>) -> Result<Rings, Error> {
        let receive = ring(RECEIVE, alloc)?;
        let receive_slots = Slots::new(RECEIVE, alloc).ok_or(Error::NoMemory)?;
        for index in 0..RECEIVE {
            receive.write64(index * DESCRIPTOR, receive_slots.phys(index));
            receive.write32(index * DESCRIPTOR + 8, SLOT as u32 | RX_GEN);
        }
        Ok(Rings {
            receive,
            bodies: ring(BODIES, alloc)?,
            received: ring(RECEIVE + BODIES, alloc)?,
            receive_slots,
            transmit: ring(TRANSMIT, alloc)?,
            transmit_data: alloc((TRANSMIT * DATA_DESCRIPTOR).div_ceil(PAGE))
                .filter(|memory| memory.len() >= TRANSMIT * DATA_DESCRIPTOR)
                .ok_or(Error::NoMemory)?,
            transmit_done: ring(TRANSMIT, alloc)?,
            transmit_slots: Slots::new(TRANSMIT, alloc).ok_or(Error::NoMemory)?,
            // All of them handed over: the next goes back after a wrap.
            fill: (0, false),
            completion: (0, true),
            filled: false,
            send: (0, true),
            told: 0,
            unsent: 0,
            sent: (0, true),
            dropped: 0,
        })
    }

    /// Hands receive descriptors back to the card up to and including
    /// `index`: the card may skip some, which go back as they are.
    fn give_back(&mut self, index: usize) {
        loop {
            let (at, generation) = self.fill;
            let word = SLOT as u32 | if generation { RX_GEN } else { 0 };
            self.receive.write32(at * DESCRIPTOR + 8, word);
            let following = next(at, RECEIVE);
            self.fill = (following, if following == 0 { !generation } else { generation });
            self.filled = true;
            if at == index {
                return;
            }
        }
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        for _ in 0..RECEIVE + BODIES {
            let (at, generation) = self.completion;
            let base = at * DESCRIPTOR;
            if (self.received.read32(base + 12) & COMP_GEN != 0) != generation {
                return None;
            }
            device_barrier();
            let first = self.received.read32(base);
            let third = self.received.read32(base + 8);
            let following = next(at, RECEIVE + BODIES);
            self.completion = (following, if following == 0 { !generation } else { generation });
            let index = (first & 0xfff) as usize;
            let ring = (first >> 16) & 0x3ff;
            let whole = first & (RCD_SOP | RCD_EOP) == RCD_SOP | RCD_EOP;
            let len = (third & 0x3fff) as usize;
            if ring != 0 || index >= RECEIVE {
                // Not a buffer it was given.
                self.dropped += 1;
                continue;
            }
            // A frame in more than one buffer can only be one longer than
            // the driver takes: each of its pieces is dropped.
            let good =
                whole && third & RCD_ERROR == 0 && (MIN_FRAME..=MAX_FRAME.min(SLOT)).contains(&len);
            let taken = good.then(|| {
                let len = len.min(frame.len());
                self.receive_slots.read(index, &mut frame[..len]);
                len
            });
            self.give_back(index);
            match taken {
                Some(len) => return Some(len),
                None => self.dropped += 1,
            }
        }
        None
    }

    /// Takes back the slots of frames the card has sent.
    fn reclaim(&mut self) {
        loop {
            let (at, generation) = self.sent;
            let base = at * DESCRIPTOR;
            if (self.transmit_done.read32(base + 12) & COMP_GEN != 0) != generation {
                return;
            }
            device_barrier();
            let last = (self.transmit_done.read32(base) & 0xfff) as usize;
            if last < TRANSMIT {
                self.unsent = next(last, TRANSMIT);
            }
            let following = next(at, TRANSMIT);
            self.sent = (following, if following == 0 { !generation } else { generation });
        }
    }

    fn can_send(&mut self) -> bool {
        self.reclaim();
        next(self.send.0, TRANSMIT) != self.unsent
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) || !self.can_send() {
            return false;
        }
        let (at, generation) = self.send;
        self.transmit_slots.write(at, frame);
        let base = at * DESCRIPTOR;
        self.transmit.write64(base, self.transmit_slots.phys(at));
        self.transmit.write32(base + 12, TX_EOP | TX_COMPREQ);
        // The generation last: it hands the descriptor over.
        device_barrier();
        let word = frame.len() as u32 | if generation { TX_GEN } else { 0 };
        self.transmit.write32(base + 8, word);
        let following = next(at, TRANSMIT);
        self.send = (following, if following == 0 { !generation } else { generation });
        true
    }
}

/// The card, running.
pub struct Vmxnet3 {
    bar0: Mmio,
    bar1: Mmio,
    area: Dma,
    rings: Rings,
    info: Info,
    interrupt: Interrupt,
    stopped: bool,
}

impl Vmxnet3 {
    /// Acknowledges the events the card raised: a link change, a queue
    /// stopped.
    fn acknowledge_events(&self) {
        let events = self.area.read32(shared::EVENT);
        if events != 0 {
            self.bar1.write32(bar1::ECR, events);
        }
    }
}

impl Nic for Vmxnet3 {
    fn info(&self) -> Info {
        self.info
    }

    fn link(&self) -> Link {
        let answer = command(&self.bar1, cmd::GET_LINK);
        let up = answer & 1 != 0 && answer != u32::MAX;
        Link { up, mbps: if up { answer >> 16 } else { 0 }, full_duplex: up }
    }

    /// A queue the card stopped, as it says in its description: in
    /// memory, so looking costs nothing.
    fn failed(&self) -> bool {
        let tx = shared::QUEUES;
        let rx = shared::QUEUES + rxq::AT;
        self.area.read8(tx + txq::STOPPED) != 0 || self.area.read8(rx + rxq::STOPPED) != 0
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.stopped {
            return None;
        }
        if let Some(len) = self.rings.receive(frame) {
            return Some(len);
        }
        self.acknowledge_events();
        if self.interrupt == Interrupt::Msi {
            // Unmask the interrupt the card masked as it raised it: if a
            // completion came meanwhile, it raises it again.
            self.bar0.write32(bar0::IMASK0, 0);
        }
        None
    }

    fn can_send(&mut self) -> bool {
        !self.stopped && self.rings.can_send()
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        !self.stopped && self.rings.send(frame)
    }

    fn flush(&mut self) {
        if self.stopped {
            return;
        }
        let rings = &mut self.rings;
        let transmit = rings.send.0 != rings.told;
        let receive =
            rings.filled && self.area.read8(shared::QUEUES + rxq::AT + rxq::UPDATE_RXHEAD) != 0;
        if transmit || receive {
            device_barrier();
        }
        if transmit {
            self.bar0.write32(bar0::TXPROD0, rings.send.0 as u32);
            rings.told = rings.send.0;
        }
        if receive {
            self.bar0.write32(bar0::RXPROD0, rings.fill.0 as u32);
        }
        rings.filled = false;
    }

    fn dropped(&self) -> u64 {
        self.rings.dropped
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.bar0.write32(bar0::IMASK0, 1);
        self.bar1.write32(bar1::CMD, cmd::QUIESCE);
        self.bar1.write32(bar1::CMD, cmd::RESET);
    }
}

impl Drop for Vmxnet3 {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
