// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The link between the kernel's network stack and a card's driver that
//! runs as a program (ADR 0198, radix-device(2)).
//!
//! The two share [`SIZE`] bytes of memory: a header page, then
//! [`ENTRIES`] slots of [`SLOT`] bytes for the frames the card received,
//! then as many for the frames the stack sends. Each direction is a ring
//! of those slots with two free-running indexes: the side that fills it
//! moves its tail, the side that empties it moves its head, and each side
//! writes only its own. A frame's length is in the header, beside its
//! ring's indexes. The driver also writes the card's link there.
//!
//! Neither side trusts the other. Each keeps its own index to itself and
//! only publishes it; it reads the other's once, and a ring whose other
//! index has run more than [`ENTRIES`] past its own, or behind it, is
//! [`Broken`]: the side that sees it stops using the link. A length is
//! taken only up to a slot, and a frame is copied out of the shared
//! memory before anything looks at it, so whatever the other side writes
//! there, and whenever, confuses at most the frames of its own link.
//!
//! The memory is reached through [`Memory`], which the kernel implements
//! over its shared pages and the driver over its mapping of them.

use crate::{Link, MAX_FRAME, MIN_FRAME};

/// The version of the layout, the header's first word.
pub const VERSION: u32 = 1;
/// The slots of each direction.
pub const ENTRIES: u32 = 256;
/// The bytes of a slot.
pub const SLOT: usize = 2048;
/// The bytes of the header, before the first slot.
pub const HEADER: usize = 4096;
/// The bytes of the whole link: the header and both directions' slots.
pub const SIZE: usize = HEADER + 2 * ENTRIES as usize * SLOT;

/// The header's words, by their byte offsets.
mod at {
    /// [`super::VERSION`], which the kernel writes.
    pub const VERSION: usize = 0;
    /// [`super::ENTRIES`], which the kernel writes.
    pub const ENTRIES: usize = 4;
    /// [`super::SLOT`], which the kernel writes.
    pub const SLOT: usize = 8;
    /// The card's link, which the driver writes: bit 31 set when it is
    /// up, bit 30 when it is full duplex, its speed in megabits a second
    /// below.
    pub const LINK: usize = 64;
    /// The received ring's tail, which the driver moves.
    pub const RECEIVED_TAIL: usize = 128;
    /// The received ring's head, which the kernel moves.
    pub const RECEIVED_HEAD: usize = 192;
    /// The sending ring's tail, which the kernel moves.
    pub const SENDING_TAIL: usize = 256;
    /// The sending ring's head, which the driver moves.
    pub const SENDING_HEAD: usize = 320;
    /// The received frames' lengths, a word for each slot.
    pub const RECEIVED_LENGTHS: usize = 1024;
    /// The frames to send's lengths, a word for each slot.
    pub const SENDING_LENGTHS: usize = 2048;
}

/// Where the received frames' slots start.
const RECEIVED: usize = HEADER;
/// Where the frames to send's slots start.
const SENDING: usize = HEADER + ENTRIES as usize * SLOT;
/// The link word's bit for a link that is up.
const UP: u32 = 1 << 31;
/// The link word's bit for a full-duplex link.
const FULL_DUPLEX: u32 = 1 << 30;
/// The link word's speed.
const MBPS: u32 = FULL_DUPLEX - 1;

/// The shared memory, as one side reaches it. Offsets are within
/// [`SIZE`]; a word's are 4-aligned.
pub trait Memory {
    /// Reads the word at `offset`, with acquire ordering: what the other
    /// side wrote before it is seen too.
    fn load(&self, offset: usize) -> u32;
    /// Writes the word at `offset`, with release ordering: what this side
    /// wrote before is seen by the other with it.
    fn store(&self, offset: usize, value: u32);
    /// Copies `out.len()` bytes from `offset` into `out`.
    fn read(&self, offset: usize, out: &mut [u8]);
    /// Copies `data` to `offset`.
    fn write(&self, offset: usize, data: &[u8]);
}

/// The other side moved an index where it cannot be: the link is of no
/// more use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Broken;

/// Writes a new link's header: the kernel does, before the driver sees
/// it. The memory is zeroed, so the rings start empty and the link down.
pub fn init(memory: &impl Memory) {
    memory.store(at::VERSION, VERSION);
    memory.store(at::ENTRIES, ENTRIES);
    memory.store(at::SLOT, SLOT as u32);
}

/// Whether the header is one this side knows: the driver checks it.
pub fn valid(memory: &impl Memory) -> bool {
    memory.load(at::VERSION) == VERSION
        && memory.load(at::ENTRIES) == ENTRIES
        && memory.load(at::SLOT) == SLOT as u32
}

/// How far `ahead` is past `behind` on a ring, if it can be: from 0, an
/// empty ring, to [`ENTRIES`], a full one.
fn distance(ahead: u32, behind: u32) -> Result<u32, Broken> {
    let gap = ahead.wrapping_sub(behind);
    if gap <= ENTRIES { Ok(gap) } else { Err(Broken) }
}

/// A ring index's slot.
fn slot(index: u32) -> usize {
    (index % ENTRIES) as usize
}

/// The kernel's side: it takes the frames the driver received and hands
/// it frames to send.
#[derive(Debug, Default)]
pub struct Stack {
    /// The next received frame to take.
    received: u32,
    /// The next slot to put a frame to send in.
    sending: u32,
    /// Frames put in since the driver was last told.
    unsent: u32,
    /// Received frames taken whose length was not a frame's.
    dropped: u64,
    broken: bool,
}

impl Stack {
    /// The kernel's side of a link whose header [`init`] wrote.
    pub fn new() -> Stack {
        Stack::default()
    }

    /// Whether the driver broke the link: nothing more is taken from it
    /// or given to it.
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// Received frames thrown away, as their lengths were not frames'.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Copies the next received frame into `frame`, and returns its
    /// length; `None` when none waits. A frame longer than `frame` is cut
    /// to it, as a card cuts one longer than its buffer.
    pub fn receive(&mut self, memory: &impl Memory, frame: &mut [u8]) -> Option<usize> {
        loop {
            if self.broken {
                return None;
            }
            let tail = memory.load(at::RECEIVED_TAIL);
            match distance(tail, self.received) {
                Err(Broken) => {
                    self.broken = true;
                    return None;
                }
                Ok(0) => return None,
                Ok(_) => {}
            }
            let index = slot(self.received);
            let len = memory.load(at::RECEIVED_LENGTHS + 4 * index) as usize;
            let taken = len.min(SLOT).min(frame.len());
            let usable = (MIN_FRAME..=SLOT).contains(&len);
            if usable {
                memory.read(RECEIVED + index * SLOT, &mut frame[..taken]);
            }
            self.received = self.received.wrapping_add(1);
            memory.store(at::RECEIVED_HEAD, self.received);
            if usable {
                return Some(taken);
            }
            self.dropped += 1;
        }
    }

    /// Whether a frame to send has a slot now.
    pub fn can_send(&mut self, memory: &impl Memory) -> bool {
        if self.broken {
            return false;
        }
        match distance(self.sending, memory.load(at::SENDING_HEAD)) {
            Ok(used) => used < ENTRIES,
            Err(Broken) => {
                self.broken = true;
                false
            }
        }
    }

    /// Puts `frame` in the next slot to send: false if there is none, or
    /// the frame is no frame. The driver sees it at the next
    /// [`Stack::flush`].
    pub fn send(&mut self, memory: &impl Memory, frame: &[u8]) -> bool {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) || !self.can_send(memory) {
            return false;
        }
        let index = slot(self.sending);
        memory.write(SENDING + index * SLOT, frame);
        memory.store(at::SENDING_LENGTHS + 4 * index, frame.len() as u32);
        self.sending = self.sending.wrapping_add(1);
        self.unsent += 1;
        true
    }

    /// Publishes the frames put in since the last time; returns whether
    /// there were any, so that the driver is woken.
    pub fn flush(&mut self, memory: &impl Memory) -> bool {
        if self.unsent == 0 || self.broken {
            return false;
        }
        memory.store(at::SENDING_TAIL, self.sending);
        self.unsent = 0;
        true
    }

    /// The card's link, as the driver last wrote it: down on a broken
    /// link.
    pub fn link(&self, memory: &impl Memory) -> Link {
        if self.broken {
            return Link::default();
        }
        let word = memory.load(at::LINK);
        if word & UP == 0 {
            return Link::default();
        }
        Link { up: true, mbps: word & MBPS, full_duplex: word & FULL_DUPLEX != 0 }
    }
}

/// The driver's side: it hands the kernel the frames the card received
/// and takes the frames to send.
#[derive(Debug, Default)]
pub struct Driver {
    /// The next slot to put a received frame in.
    received: u32,
    /// The next frame to send to take.
    sending: u32,
    broken: bool,
}

impl Driver {
    /// The driver's side of a link it has just attached.
    pub fn new() -> Driver {
        Driver::default()
    }

    /// Whether the kernel broke the link.
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// Whether a received frame has a slot now.
    pub fn can_receive(&mut self, memory: &impl Memory) -> bool {
        if self.broken {
            return false;
        }
        match distance(self.received, memory.load(at::RECEIVED_HEAD)) {
            Ok(used) => used < ENTRIES,
            Err(Broken) => {
                self.broken = true;
                false
            }
        }
    }

    /// Puts a received frame in the next slot, and publishes it at once:
    /// false if no slot is free, and the frame is lost, as on a wire.
    pub fn received(&mut self, memory: &impl Memory, frame: &[u8]) -> bool {
        if frame.len() > SLOT || !self.can_receive(memory) {
            return false;
        }
        let index = slot(self.received);
        memory.write(RECEIVED + index * SLOT, frame);
        memory.store(at::RECEIVED_LENGTHS + 4 * index, frame.len() as u32);
        self.received = self.received.wrapping_add(1);
        memory.store(at::RECEIVED_TAIL, self.received);
        true
    }

    /// Whether a frame to send waits, without taking it.
    pub fn sending(&mut self, memory: &impl Memory) -> bool {
        if self.broken {
            return false;
        }
        match distance(memory.load(at::SENDING_TAIL), self.sending) {
            Ok(waiting) => waiting > 0,
            Err(Broken) => {
                self.broken = true;
                false
            }
        }
    }

    /// Copies the next frame to send into `frame` and returns its length;
    /// `None` when none waits. A length that is not a frame's is cut to a
    /// slot, so the copy stays in the link.
    pub fn take(&mut self, memory: &impl Memory, frame: &mut [u8]) -> Option<usize> {
        if !self.sending(memory) {
            return None;
        }
        let index = slot(self.sending);
        let len = (memory.load(at::SENDING_LENGTHS + 4 * index) as usize).min(SLOT).min(frame.len());
        memory.read(SENDING + index * SLOT, &mut frame[..len]);
        self.sending = self.sending.wrapping_add(1);
        memory.store(at::SENDING_HEAD, self.sending);
        Some(len)
    }

    /// Writes the card's link for the kernel.
    pub fn set_link(&self, memory: &impl Memory, link: Link) {
        let duplex = if link.full_duplex { FULL_DUPLEX } else { 0 };
        let word = if link.up { UP | duplex | link.mbps.min(MBPS) } else { 0 };
        memory.store(at::LINK, word);
    }
}

#[cfg(test)]
mod tests;
