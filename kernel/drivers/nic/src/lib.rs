// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the kernel's network card drivers share (ADR 0197).
//!
//! The kernel's network stack (`radix-net`) drives every card the same
//! way, through [`Nic`]: it takes received frames out, hands frames to
//! send in, and rings the card's doorbells once a poll is done. A driver
//! crate (`radix-intel-net`, `radix-vmxnet3`, `radix-ena`, `radix-gve`)
//! implements it for its cards, over the registers and DMA memory that
//! `radix-hw` hands out, and nothing else: like the block drivers, the
//! network drivers are compartments that forbid unsafe code, take no
//! locks and never sleep. The kernel passes in how to allocate DMA memory
//! and how to wait where a card needs time ([`wait_for`]).
//!
//! Frames are copied, as virtio-net's are (ADR 0181): each ring of a
//! card has [`Slots`], a block of DMA memory with a buffer of [`SLOT`]
//! bytes for each descriptor, which the card reads a frame to send from
//! or writes a received one into. A frame is copied once on its way in,
//! from its slot to the stack's buffer, and once on its way out, so the
//! card never reaches memory the stack or a program owns, and its slots
//! are the only memory its IOMMU domain maps.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

use radix_hw::Dma;

pub mod link;

/// The bytes of a frame's slot: an Ethernet frame of up to 1518 bytes,
/// with a VLAN tag, and its checksum, with room to spare. Cards that
/// write received frames in buffers of a fixed size are told this one.
pub const SLOT: usize = 2048;
/// The longest frame a driver sends or takes: 1514 bytes and a VLAN tag,
/// without the frame check sequence, which the cards add and strip.
pub const MAX_FRAME: usize = 1518;
/// The shortest frame worth taking or sending: its two addresses and its
/// type. Cards pad a shorter one to Ethernet's 60 bytes as they send it.
pub const MIN_FRAME: usize = 14;
/// The size of a page of DMA memory, as the kernel allocates it.
pub const PAGE: usize = 4096;

/// What a card is, once it is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Its model, as its maker names it: "82574L", "I210", "VMXNET3".
    pub model: &'static str,
    /// Its MAC address: the card's own, or the one the driver was given
    /// when the card has none it can read.
    pub mac: [u8; 6],
    /// How many frames the card holds for the driver at once.
    pub receive_slots: u16,
    /// How many frames the driver can have waiting to be sent.
    pub transmit_slots: u16,
}

/// The interrupt the kernel set up for a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interrupt {
    /// None: the kernel looks at the card from time to time, and the
    /// card raises nothing.
    Polled,
    /// A message-signalled interrupt (MSI), which the card raises for
    /// every cause it has.
    Msi,
}

/// The state of a card's link, as the card says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Link {
    /// Whether the link is up.
    pub up: bool,
    /// Its speed in megabits a second, when it is up and the card says.
    pub mbps: u32,
    /// Whether it is full duplex.
    pub full_duplex: bool,
}

/// Why a card could not be set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The DMA memory a ring needs could not be had.
    NoMemory,
    /// The card did not finish something within its time: what.
    Timeout(&'static str),
    /// The card said it failed, or answered with nonsense: what.
    Device(&'static str),
    /// The card is of a kind the driver knows, without a feature it
    /// needs: which.
    Unsupported(&'static str),
    /// A register window was smaller than the card's registers.
    Registers,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoMemory => f.write_str("out of DMA memory"),
            Error::Timeout(what) => write!(f, "the card timed out: {what}"),
            Error::Device(what) => write!(f, "the card failed: {what}"),
            Error::Unsupported(what) => write!(f, "not supported: {what}"),
            Error::Registers => f.write_str("its registers are smaller than the card's"),
        }
    }
}

/// A network card, set up and running, as the stack drives it.
///
/// The stack's thread calls these one at a time, under its lock: none of
/// them waits for the card. A driver stops its card when it is dropped,
/// before its DMA memory is given back, if [`Nic::stop`] has not.
pub trait Nic: Send {
    /// What the card is.
    fn info(&self) -> Info;
    /// Its link, as the card says now.
    fn link(&self) -> Link;
    /// Whether the card stopped with an error, so that it does nothing
    /// more until it is set up again.
    fn failed(&self) -> bool;
    /// Copies the next frame the card received into `frame`, cut to its
    /// length, and returns how long it is; `None` when no frame waits.
    /// Frames the card marked as damaged, or too short, are dropped and
    /// counted. With nothing more to take, it acknowledges the card's
    /// interrupt, so that the card raises the next.
    fn receive(&mut self, frame: &mut [u8]) -> Option<usize>;
    /// Whether the card can take a frame to send now, once it has taken
    /// back the slots of frames it sent.
    fn can_send(&mut self) -> bool;
    /// Copies `frame` into a slot to send: false, and the frame is lost,
    /// as on a wire, if no slot is free or its length is not
    /// [`MIN_FRAME`]..=[`MAX_FRAME`].
    fn send(&mut self, frame: &[u8]) -> bool;
    /// Tells the card of the frames handed to it and the slots given back
    /// since the last time: its doorbells.
    fn flush(&mut self);
    /// How many received frames it dropped: damaged, too short, or with
    /// no slot to give back.
    fn dropped(&self) -> u64;
    /// Stops the card: it raises no more interrupts, and reaches no more
    /// memory, so that its interrupt and its DMA memory can be let go.
    /// Nothing more is sent or received; a second call does nothing.
    fn stop(&mut self);
}

/// A slot for each of a ring's descriptors: `count` buffers of [`SLOT`]
/// bytes in one block of DMA memory.
pub struct Slots {
    memory: Dma,
    count: usize,
}

impl Slots {
    /// `count` zeroed slots from `alloc`, which gives zeroed DMA memory in
    /// pages; `None` without the memory.
    pub fn new(count: usize, alloc: &mut dyn FnMut(usize) -> Option<Dma>) -> Option<Slots> {
        let memory = alloc((count * SLOT).div_ceil(PAGE))?;
        (memory.len() >= count * SLOT).then_some(Slots { memory, count })
    }

    /// How many slots there are.
    pub fn count(&self) -> usize {
        self.count
    }

    /// The physical address of slot `index`, which the card uses.
    ///
    /// # Panics
    ///
    /// If `index` is not below [`Slots::count`].
    pub fn phys(&self, index: usize) -> u64 {
        assert!(index < self.count, "slot {index} of {}", self.count);
        self.memory.phys() + (index * SLOT) as u64
    }

    /// Copies the first `out.len()` bytes of slot `index` into `out`.
    ///
    /// # Panics
    ///
    /// If `index` is not below [`Slots::count`] or `out` is longer than a
    /// slot.
    pub fn read(&self, index: usize, out: &mut [u8]) {
        self.read_at(index, 0, out);
    }

    /// Copies the `out.len()` bytes from `offset` in slot `index` into
    /// `out`: for a card that writes a frame further into its slot.
    ///
    /// # Panics
    ///
    /// If `index` is not below [`Slots::count`] or the bytes run past the
    /// end of the slot.
    pub fn read_at(&self, index: usize, offset: usize, out: &mut [u8]) {
        assert!(index < self.count && offset.checked_add(out.len()).is_some_and(|end| end <= SLOT));
        self.memory.read(index * SLOT + offset, out);
    }

    /// Copies `data` to the start of slot `index`.
    ///
    /// # Panics
    ///
    /// If `index` is not below [`Slots::count`] or `data` is longer than
    /// a slot.
    pub fn write(&self, index: usize, data: &[u8]) {
        assert!(index < self.count && data.len() <= SLOT);
        self.memory.write(index * SLOT, data);
    }
}

/// Waits for `done` to say true, looking every `step_us` microseconds
/// for up to `total_us`, with `delay` waiting the given number of
/// microseconds: whether it did. It looks once more at the end, so that a
/// slow `delay` cannot make it give up early.
pub fn wait_for(
    delay: &mut dyn FnMut(u32),
    total_us: u32,
    step_us: u32,
    mut done: impl FnMut() -> bool,
) -> bool {
    let step_us = step_us.max(1);
    let mut waited = 0u32;
    while waited < total_us {
        if done() {
            return true;
        }
        delay(step_us);
        waited = waited.saturating_add(step_us);
    }
    done()
}

/// Whether `mac` can be a card's own address: not all zeros, all ones or
/// a multicast address, as an empty or unprogrammed EEPROM gives.
pub fn usable_mac(mac: [u8; 6]) -> bool {
    mac != [0; 6] && mac != [0xff; 6] && mac[0] & 0x01 == 0
}

/// A locally administered, unicast address made from `random` bytes, for
/// a card whose own cannot be read.
pub fn local_mac(random: [u8; 6]) -> [u8; 6] {
    let mut mac = random;
    mac[0] = (mac[0] & !0x01) | 0x02;
    mac
}

/// Counts the frames of a ring as they go round: the next index after
/// `index` in a ring of `count`.
pub fn next(index: usize, count: usize) -> usize {
    if index + 1 == count { 0 } else { index + 1 }
}

#[cfg(test)]
mod tests;
