// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The loopback interface's link (ADR 0183): every packet sent on it
//! comes back to be received, in order, and none leaves the machine.
//!
//! The packets wait in a ring of the link's own, where the stack writes
//! and reads them in place (ADR 0185): a packet sent over loopback is
//! not copied out of the stack's buffer for sending, nor into its buffer
//! for receiving, as a card's frames are. Each packet has a slot of the
//! ring, which starts on a multiple of [`HEADER`] bytes: a header with its
//! length, then its bytes. A slot that does not fit before the ring's end
//! goes at its start, and a header that says so ([`WRAP`]) is left where
//! it would have gone, when there is room for one.

use crate::{Link, Medium};

/// The bytes before each packet in its slot, which hold its length, and
/// what each slot's length is a multiple of.
const HEADER: usize = 8;

/// The length in a header that says the next slot is at the ring's start.
const WRAP: u32 = u32::MAX;

/// A link that hands back what is sent on it: IP packets, each in a slot
/// of a ring of bytes until the stack receives it.
pub struct Loopback {
    ring: &'static mut [u8],
    /// Where the oldest packet's slot starts.
    head: usize,
    /// Where the next packet's slot goes, unless it wraps.
    tail: usize,
    /// How many packets have been put in the ring, and how many taken out
    /// of it, counted round: the ring is empty when they are the same.
    put: usize,
    taken: usize,
    /// The longest packet it takes.
    mtu: usize,
}

/// The bytes of the slot of a packet of `len` bytes.
fn slot(len: usize) -> usize {
    HEADER + len.next_multiple_of(HEADER)
}

/// Where a slot of `size` bytes goes in a ring of `len` bytes that holds
/// packets, the oldest at `head`, the next to go at `tail`: at the tail,
/// or at the start if the tail is too near the end. `None` if it does not
/// fit.
fn place(len: usize, head: usize, tail: usize, size: usize) -> Option<usize> {
    if tail > head {
        if tail + size <= len { Some(tail) } else { (size <= head).then_some(0) }
    } else {
        // The slots wrap round the end, or fill the ring when the tail has
        // come round to the head.
        (tail + size <= head).then_some(tail)
    }
}

/// The length in the header at `at` in `ring`, or a wrap where there is no
/// room for a header.
fn header(ring: &[u8], at: usize) -> u32 {
    ring.get(at..at + HEADER)
        .map_or(WRAP, |bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Writes `len` into the header at the start of `slot`.
fn write_header(slot: &mut [u8], len: u32) {
    slot[..4].copy_from_slice(&len.to_le_bytes());
}

impl Loopback {
    /// A loopback link for packets of up to `mtu` bytes (at most 65,535),
    /// which wait in `ring`. [`Loopback::ring_for`] says how long a ring
    /// holds a number of the longest packets.
    pub fn new(ring: &'static mut [u8], mtu: usize) -> Loopback {
        // Slots start on a multiple of the header's length.
        let whole = ring.len() - ring.len() % HEADER;
        let ring = &mut ring[..whole];
        Loopback { ring, head: 0, tail: 0, put: 0, taken: 0, mtu: mtu.min(usize::from(u16::MAX)) }
    }

    /// The bytes of a ring that holds `packets` packets of `mtu` bytes.
    pub const fn ring_for(packets: usize, mtu: usize) -> usize {
        packets * (HEADER + mtu.next_multiple_of(HEADER))
    }

    fn is_empty(&self) -> bool {
        self.put == self.taken
    }

    /// Moves the head to the ring's start when the oldest packet is there,
    /// past a wrap. An empty ring starts again at its start, with all its
    /// room.
    fn settle(&mut self) {
        if self.is_empty() {
            self.head = 0;
            self.tail = 0;
        } else if header(self.ring, self.head) == WRAP {
            self.head = 0;
        }
    }

    /// Where a slot for a packet of `len` bytes goes, if there is room.
    fn room(&mut self, len: usize) -> Option<usize> {
        if len > self.mtu {
            return None;
        }
        self.settle();
        let size = slot(len);
        if self.is_empty() {
            return (size <= self.ring.len()).then_some(0);
        }
        place(self.ring.len(), self.head, self.tail, size)
    }

    /// Writes a packet of `len` bytes with `write`, in place, in the slot
    /// at `at` that [`Loopback::room`] gave, and puts it in the ring.
    fn put_at<R>(&mut self, at: usize, len: usize, write: impl FnOnce(&mut [u8]) -> R) -> R {
        if at != self.tail && self.tail + HEADER <= self.ring.len() {
            write_header(&mut self.ring[self.tail..], WRAP);
        }
        let slot = &mut self.ring[at..at + slot(len)];
        // A packet is no longer than the MTU, which fits in 16 bits.
        write_header(slot, u32::try_from(len).unwrap_or(WRAP));
        let result = write(&mut slot[HEADER..HEADER + len]);
        self.tail = at + slot.len();
        self.put = self.put.wrapping_add(1);
        result
    }

    /// Writes a packet of `len` bytes with `write`, in place, at the ring's
    /// tail. With no room for it, `write` writes it to `spare` instead, cut
    /// to its length, and the packet is lost, as on a card whose ring is
    /// full.
    pub(crate) fn send_with<R>(
        &mut self,
        len: usize,
        spare: &mut [u8],
        write: impl FnOnce(&mut [u8]) -> R,
    ) -> R {
        match self.room(len) {
            Some(at) => self.put_at(at, len, write),
            None => {
                let room = len.min(spare.len());
                write(&mut spare[..room])
            }
        }
    }

    /// The oldest packet in the ring, in place, and the room to send an
    /// answer to it in while it is read; `None` when the ring is empty.
    pub(crate) fn oldest(&mut self) -> Option<(Oldest<'_>, Answer<'_>)> {
        if self.is_empty() {
            return None;
        }
        self.settle();
        let len = self.ring.len();
        let head = self.head;
        // A header holds a packet's length, which fits in its slot.
        let packet = usize::try_from(header(self.ring, head))
            .map_or(0, |packet| packet.min(len.saturating_sub(head + HEADER)));
        let next = (head + slot(packet)).min(len);
        let Loopback { ring, head: oldest, tail, put, taken, mtu } = self;
        let (before, rest) = ring.split_at_mut(head);
        let (slot, after) = rest.split_at_mut(next - head);
        Some((
            Oldest { packet: &slot[HEADER..HEADER + packet], head: oldest, taken, next },
            Answer { before, after, next, head, tail, put, mtu: *mtu, len },
        ))
    }
}

/// The oldest packet in a loopback link's ring, while the stack reads it.
pub(crate) struct Oldest<'a> {
    packet: &'a [u8],
    head: &'a mut usize,
    taken: &'a mut usize,
    /// Where the next slot starts.
    next: usize,
}

impl Oldest<'_> {
    /// Reads the packet with `read`, then takes it out of the ring.
    pub(crate) fn read<R>(self, read: impl FnOnce(&[u8]) -> R) -> R {
        let result = read(self.packet);
        *self.head = self.next;
        *self.taken = self.taken.wrapping_add(1);
        result
    }
}

/// The room in a loopback link's ring around its oldest packet, while the
/// stack reads that packet: where an answer to it goes.
pub(crate) struct Answer<'a> {
    /// The ring before the oldest packet's slot, and after it.
    before: &'a mut [u8],
    after: &'a mut [u8],
    /// Where `after` starts, and where the oldest packet's slot does.
    next: usize,
    head: usize,
    tail: &'a mut usize,
    put: &'a mut usize,
    mtu: usize,
    /// The ring's length.
    len: usize,
}

impl Answer<'_> {
    /// Writes an answer of `len` bytes with `write`, in place, in the ring,
    /// as [`Loopback::send_with`] does.
    pub(crate) fn send_with<R>(
        self,
        len: usize,
        spare: &mut [u8],
        write: impl FnOnce(&mut [u8]) -> R,
    ) -> R {
        let size = slot(len);
        // The ring holds the oldest packet, so it is not empty.
        let at = if len <= self.mtu { place(self.len, self.head, *self.tail, size) } else { None };
        let Some(at) = at else {
            let room = len.min(spare.len());
            return write(&mut spare[..room]);
        };
        let tail = *self.tail;
        // A slot goes at the tail, after the oldest packet's, or before
        // the oldest packet's: at the ring's start, when the tail is too
        // near its end, or at a tail that has come round already.
        let slot = if at >= self.next {
            &mut self.after[at - self.next..at - self.next + size]
        } else {
            if at != tail && tail + HEADER <= self.len {
                write_header(&mut self.after[tail - self.next..], WRAP);
            }
            &mut self.before[at..at + size]
        };
        write_header(slot, u32::try_from(len).unwrap_or(WRAP));
        let result = write(&mut slot[HEADER..HEADER + len]);
        *self.tail = at + size;
        *self.put = self.put.wrapping_add(1);
        result
    }
}

impl Link for Loopback {
    fn medium(&self) -> Medium {
        Medium::Ip
    }

    fn pending(&self) -> bool {
        !self.is_empty()
    }

    fn mac(&self) -> [u8; 6] {
        [0; 6]
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        let (oldest, _) = self.oldest()?;
        // What `frame` has no room for is lost, as on a card.
        Some(oldest.read(|packet| {
            let kept = packet.len().min(frame.len());
            frame[..kept].copy_from_slice(&packet[..kept]);
            kept
        }))
    }

    fn can_send(&mut self) -> bool {
        self.room(self.mtu).is_some()
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        let Some(at) = self.room(frame.len()) else { return false };
        self.put_at(at, frame.len(), |slot| slot.copy_from_slice(frame));
        true
    }

    fn flush(&mut self) {}

    fn needs_checksums(&self) -> bool {
        false
    }

    fn loopback(&mut self) -> Option<&mut Loopback> {
        Some(self)
    }
}
