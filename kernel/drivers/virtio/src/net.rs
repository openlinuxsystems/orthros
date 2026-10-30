// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The virtio network card (virtio 1.2, section 5.1).
//!
//! The card has a receive queue (0) and a transmit queue (1). Every
//! Ethernet frame on either is preceded by a 12-byte header
//! (`virtio_net_hdr` with `num_buffers`, which virtio 1.0 always has),
//! which says what offloads apply to it; the driver asks for none, so
//! the header of a frame it sends is zeros, and the one of a frame it
//! receives is skipped.
//!
//! Frames are copied, not lent: each queue has a block of DMA memory
//! with a slot of [`SLOT`] bytes for each of its descriptors, which is
//! a frame's buffer while the descriptor is the head of a chain in
//! flight. Without mergeable receive buffers and with no offloads, the
//! device sends at most a header and 1514 bytes (plus a VLAN tag) in one
//! buffer (5.1.6.3.1), which a slot holds. Every receive slot is handed
//! to the device at the start; a frame taken out is copied to the
//! caller's buffer and its slot handed back at once. A frame to send is
//! copied into the slot of the descriptor that carries it; slots of
//! frames the device has sent are taken back as more frames are sent.
//!
//! The driver asks for the card's MAC address and link status and
//! nothing else: no checksum or segmentation offloads, no mergeable
//! buffers, no control queue and no more queues than the first pair.
//! Both queues raise the same MSI-X entry.

use radix_hw::{Dma, device_barrier};

use crate::queue::{Buffer, Queue};
use crate::{Error, Transport, feature as common_feature, status};

/// Network card feature bits.
pub mod feature {
    /// The card has a MAC address in its configuration.
    pub const MAC: u64 = 1 << 5;
    /// The card says whether its link is up.
    pub const STATUS: u64 = 1 << 16;
}

/// The bytes of the header in front of every frame.
pub const HEADER: usize = 12;
/// The bytes of a frame's slot: a header and an Ethernet frame of up to
/// 1514 bytes, with a VLAN tag, and room to spare.
pub const SLOT: usize = 2048;
/// The longest frame, without its header, that a slot holds.
pub const MAX_FRAME: usize = SLOT - HEADER;
/// The shortest Ethernet frame worth taking: its two addresses and its
/// type.
pub const MIN_FRAME: usize = 14;

/// The link status bit in the configuration's `status`.
const LINK_UP: u16 = 1;
/// Where the configuration keeps the MAC address and the status.
const CONFIG_MAC: usize = 0;
const CONFIG_STATUS: usize = 6;

const RECEIVE: u16 = 0;
const TRANSMIT: u16 = 1;

/// What the card is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Its MAC address: the card's own, or the one the driver was given
    /// when the card has none.
    pub mac: [u8; 6],
    /// Whether the card says whether its link is up; without it, the link
    /// is taken to be up.
    pub link_status: bool,
    /// How many frames the card holds for the driver at once.
    pub receive_slots: u16,
    /// How many frames the driver can have waiting to be sent.
    pub transmit_slots: u16,
}

/// A virtio network card, set up and running.
pub struct Net {
    transport: Transport,
    receive: Queue,
    transmit: Queue,
    /// A slot for each receive descriptor.
    received: Dma,
    /// A slot for each transmit descriptor.
    sending: Dma,
    info: Info,
    /// Frames received too short or too long, and dropped.
    dropped: u64,
}

impl Net {
    /// Sets the card up: resets it, agrees on features, reads its MAC
    /// address (taking `fallback_mac` when it has none, which should be a
    /// random, locally administered address), sets up its first pair of
    /// queues with their interrupts on MSI-X entry `vector`, hands every
    /// receive slot to the card and tells it the driver is ready. `alloc`
    /// gives zeroed DMA memory, in pages.
    pub fn new(
        transport: Transport,
        vector: u16,
        fallback_mac: [u8; 6],
        alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    ) -> Result<Net, Error> {
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
        let wanted = feature::MAC
            | feature::STATUS
            | common_feature::EVENT_IDX
            | common_feature::ACCESS_PLATFORM;
        let features = common_feature::VERSION_1 | (offered & wanted);
        if let Err(err) = transport.set_driver_features(features) {
            transport.add_status(status::FAILED);
            return Err(err);
        }
        transport.no_config_interrupt();
        let mac = if features & feature::MAC != 0 {
            transport.read_config(|config| core::array::from_fn(|i| config.read8(CONFIG_MAC + i)))
        } else {
            fallback_mac
        };
        let fail = |err: Error| {
            transport.add_status(status::FAILED);
            err
        };
        let size = crate::queue::MAX_SIZE;
        let mut receive =
            Queue::new(&transport, RECEIVE, size, vector, features, (0, 0), alloc).map_err(fail)?;
        let transmit = Queue::new(&transport, TRANSMIT, size, vector, features, (0, 0), alloc)
            .map_err(fail)?;
        let slots = |queue: &Queue| (usize::from(queue.size()) * SLOT).div_ceil(4096);
        let Some(received) = alloc(slots(&receive)) else {
            transport.reset();
            return Err(Error::NoMemory);
        };
        let Some(sending) = alloc(slots(&transmit)) else {
            transport.reset();
            return Err(Error::NoMemory);
        };
        // Every receive slot to the card, before it may use the queue.
        for _ in 0..receive.size() {
            Net::give(&mut receive, &received)?;
        }
        let info = Info {
            mac,
            link_status: features & feature::STATUS != 0,
            receive_slots: receive.size(),
            transmit_slots: transmit.size(),
        };
        transport.add_status(status::DRIVER_OK);
        receive.notify();
        Ok(Net { transport, receive, transmit, received, sending, info, dropped: 0 })
    }

    /// Hands the card the receive slot of the next free descriptor.
    fn give(receive: &mut Queue, slots: &Dma) -> Result<(), Error> {
        let head = receive.next_free();
        let phys = slots.phys() + (usize::from(head) * SLOT) as u64;
        receive.add(1, |_| Buffer { phys, len: SLOT as u32, device_writes: true })?;
        Ok(())
    }

    /// What the card is.
    pub fn info(&self) -> Info {
        self.info
    }

    /// Whether the link is up, as the card says it is.
    pub fn link_up(&self) -> bool {
        !self.info.link_status
            || self.transport.read_config(|config| config.read16(CONFIG_STATUS)) & LINK_UP != 0
    }

    /// Whether the card asks to be reset after an error.
    pub fn failed(&self) -> bool {
        self.transport.status() & status::NEEDS_RESET != 0
    }

    /// Copies the next frame the card received into `frame`, without its
    /// header, and hands its slot back to the card. Returns its length, or
    /// `None` when no frame waits. A frame longer than `frame` is cut to
    /// it; frames shorter than an Ethernet header, or that the card says
    /// are longer than their slot, are dropped and counted
    /// ([`Net::dropped`]). Call [`Net::notify`] after taking frames, so
    /// that the card sees the slots handed back.
    pub fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        // At most a queue's worth of frames dropped in one call.
        for _ in 0..self.receive.size() {
            let (head, written) = self.receive.used()?;
            device_barrier();
            let written = written as usize;
            let taken = if (HEADER + MIN_FRAME..=SLOT).contains(&written) {
                let len = (written - HEADER).min(frame.len());
                let start = usize::from(head) * SLOT + HEADER;
                self.received.read(start, &mut frame[..len]);
                Some(len)
            } else {
                self.dropped += 1;
                None
            };
            // The chain was one descriptor, now free again: the next head
            // handed out is it, so the slot handed back is its own.
            if Net::give(&mut self.receive, &self.received).is_err() {
                // Cannot happen: a descriptor was just freed.
                self.dropped += 1;
            }
            if taken.is_some() {
                return taken;
            }
        }
        None
    }

    /// Whether a frame can be sent now: a transmit descriptor is free
    /// once the frames the card sent are taken back.
    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        self.transmit.room(1) > 0
    }

    /// Takes back the slots of the frames the card has sent.
    fn reclaim(&mut self) {
        while self.transmit.used().is_some() {}
    }

    /// Copies `frame` (an Ethernet frame without its header) into a free
    /// transmit slot and hands it to the card. Call [`Net::notify`] after
    /// the frames of a batch, so that the card takes them together.
    pub fn send(&mut self, frame: &[u8]) -> Result<(), Error> {
        if frame.len() < MIN_FRAME || frame.len() > MAX_FRAME {
            return Err(Error::BadRequest);
        }
        if !self.can_send() {
            return Err(Error::Full);
        }
        let head = self.transmit.next_free();
        let start = usize::from(head) * SLOT;
        self.sending.write(start, &[0; HEADER]);
        self.sending.write(start + HEADER, frame);
        let phys = self.sending.phys() + start as u64;
        let len = (HEADER + frame.len()) as u32;
        let added = self.transmit.add(1, |_| Buffer { phys, len, device_writes: false })?;
        debug_assert_eq!(added, head);
        Ok(())
    }

    /// Rings the card's doorbells for the frames sent and the receive
    /// slots handed back since the last time.
    pub fn notify(&mut self) {
        self.receive.notify();
        self.transmit.notify();
    }

    /// How many frames were dropped as too short or too long.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// How many used entries the card gave that named no buffer in
    /// flight, on both queues: a card that misbehaves.
    pub fn bad_entries(&self) -> u32 {
        self.receive.bad_entries().saturating_add(self.transmit.bad_entries())
    }

    /// Resets the card: it stops using every buffer it was given.
    pub fn reset(&self) {
        self.transport.reset();
    }
}

impl Drop for Net {
    /// Resets the card before its queues and slots are given back, so
    /// that it writes to none of them after.
    fn drop(&mut self) {
        self.transport.reset();
    }
}
