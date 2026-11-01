// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! SD card readers on PCI (`radix-sdhci`, Phase 11g, ADR 0247): the SD,
//! SDHC and SDXC memory cards in an SD host controller's slots, each named
//! `mmcblk0` and on, as Linux names them, and its partitions `mmcblk0p1`
//! and on.
//!
//! [`bind`] turns on the controller's registers and bus master and reads
//! where its slots are (the Slot Information register in configuration
//! space: the first slot's BAR, and how many follow it); each slot with a
//! card in it at boot is set up ([`radix_sdhci::Slot::identify`]) and is
//! one lane of a queue of its own ([`Disk`]), polled by the threads
//! waiting for it, as an IDE channel is. A card inserted later, or taken
//! out, is not seen yet.
//!
//! A controller whose descriptors are 32-bit (one before version 3.00, as
//! QEMU's `sdhci-pci` is by default) reaches only the first 4 GiB, which
//! page caches are not kept to: a request whose buffers all lie below it
//! goes to them, and any other through a bounce buffer of [`SEGMENTS`]
//! pages below 4 GiB, copied before a write and after a read.
//!
//! An SD card keeps no write cache the host can see: a flush is done at
//! once, and every write is through. Discarding (the cards' ERASE) is
//! left for later.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use radix_block::{DeviceRef, Error, Info};
use radix_hal::{Devices, UserMode};
use radix_hw::Dma;
use radix_hw::pci::{Bar, Function};
use radix_klog::{TextBuf, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sdhci::{Card, Op as SdOp, Slot};

use super::queue::{self, Controller, Disk, Op, Outcome, Part, Request, Volume};
use super::{Entry, Ident, Name, Size, partitions, register};
use crate::Arch;

/// The most segments of a request, each at most a page: the bounce
/// buffer's pages.
const SEGMENTS: usize = 32;
const PAGE: usize = 4096;
/// The most slots a controller has.
const MAX_SLOTS: u8 = 6;
/// The next card's number, in its name `mmcblk{n}`.
static NEXT: AtomicU32 = AtomicU32::new(0);

/// The request in flight.
#[derive(Clone, Copy)]
struct Flight {
    slot: u8,
    /// A read through the bounce buffer, whose bytes go to the request's
    /// buffers, kept in `buffers`, once it is done.
    bounced_read: bool,
}

/// A slot as the queue drives it.
pub struct SdSlot {
    slot: Slot,
    flight: Option<Flight>,
    /// A request done without the card: a flush.
    done: Option<(u8, Outcome)>,
    /// The bounce buffer, below 4 GiB, if the slot needs it.
    bounce: Option<Dma>,
    /// The buffers of a bounced read.
    buffers: Vec<(u64, u32), Kmalloc>,
    /// The buffers the descriptors point at.
    descriptors: Vec<(u64, u32), Kmalloc>,
    /// A page between a request's buffer and the bounce buffer.
    copy: Vec<u8, Kmalloc>,
    /// Why the last request failed, for the log.
    why: Option<(u8, &'static str)>,
    /// The slot's place, for the log: the controller's address and the
    /// slot's number.
    place: TextBuf<40>,
}

impl SdSlot {
    /// Copies a bounced read's bytes to its buffers.
    fn copy_back(&mut self) {
        let Some(bounce) = &self.bounce else { return };
        let mut at = bounce.phys();
        for &(phys, len) in &self.buffers {
            let part = &mut self.copy[..len as usize];
            <Arch as UserMode>::read_frames(at, part);
            <Arch as UserMode>::write_frames(phys, part);
            at += u64::from(len);
        }
    }

    /// The descriptors' buffers for `data`: themselves when the slot
    /// reaches them, else the bounce buffer, filled for a write.
    fn place_buffers(&mut self, data: &[(u64, u32)], write: bool) -> Result<bool, Error> {
        self.descriptors.clear();
        self.buffers.clear();
        if data.len() > SEGMENTS || data.iter().any(|&(_, len)| len as usize > PAGE) {
            return Err(Error::OutOfRange);
        }
        let reached = |&(phys, len): &(u64, u32)| phys + u64::from(len) <= 1 << 32;
        let bounce = match &self.bounce {
            Some(bounce) if !data.iter().all(reached) => bounce.phys(),
            _ => {
                for &buffer in data {
                    let _ = self.descriptors.push_within_capacity(buffer);
                }
                return Ok(false);
            }
        };
        let mut at = bounce;
        for &(phys, len) in data {
            let _ = self.buffers.push_within_capacity((phys, len));
            let _ = self.descriptors.push_within_capacity((at, len));
            if write {
                let part = &mut self.copy[..len as usize];
                <Arch as UserMode>::read_frames(phys, part);
                <Arch as UserMode>::write_frames(at, part);
            }
            at += u64::from(len);
        }
        Ok(true)
    }
}

impl Controller for SdSlot {
    fn room(&self, _op: Op, _segments: usize) -> bool {
        self.flight.is_none() && self.done.is_none()
    }

    fn submit(&mut self, slot: u8, request: &Request<'_>, _more: bool) -> Result<(), Error> {
        if request.target != 0 {
            return Err(Error::OutOfRange);
        }
        let op = match request.op {
            Op::Read => SdOp::Read,
            Op::Write | Op::WriteThrough => SdOp::Write,
            Op::Flush => {
                self.done = Some((slot, Outcome::Done));
                return Ok(());
            }
            Op::Discard(_) | Op::Smart(_) => return Err(Error::Unsupported),
        };
        let bytes: u64 = request.data.iter().map(|&(_, len)| u64::from(len)).sum();
        let count =
            u32::try_from(bytes / u64::from(radix_sdhci::BLOCK)).map_err(|_| Error::OutOfRange)?;
        let bounced_read = self.place_buffers(request.data, op == SdOp::Write)? && op == SdOp::Read;
        let block = request.sector;
        self.slot.start(op, block, count, &self.descriptors, &mut queue::delay).map_err(|err| {
            match err {
                radix_sdhci::Error::BadRequest => Error::OutOfRange,
                _ => Error::Io,
            }
        })?;
        self.flight = Some(Flight { slot, bounced_read });
        Ok(())
    }

    fn complete(&mut self) -> Option<(u8, Outcome)> {
        if let Some(done) = self.done.take() {
            return Some(done);
        }
        let flight = self.flight?;
        let result = self.slot.poll()?;
        self.flight = None;
        match result {
            Ok(()) => {
                if flight.bounced_read {
                    self.copy_back();
                }
                Some((flight.slot, Outcome::Done))
            }
            Err(err) => {
                self.why = Some((flight.slot, err.as_str()));
                Some((flight.slot, Outcome::Failed))
            }
        }
    }

    fn why(&self, slot: u8) -> Option<&'static str> {
        self.why.filter(|&(failed, _)| failed == slot).map(|(_, why)| why)
    }

    fn reset(lanes: &mut [Part<SdSlot>]) -> bool {
        for lane in lanes {
            lane.slot.abandon(&mut queue::delay);
            if let Some(flight) = lane.flight.take() {
                lane.done = Some((flight.slot, Outcome::Retry));
            }
            warn!(
                "Disk: a request to the SD card in the reader at {} went unanswered for 30 s; \
                 the slot was reset, and what was in flight goes again",
                lane.place
            );
        }
        true
    }
}

/// Sets up the SD host controller `function` and the cards in its slots,
/// counting them in `next`. Kept out of the frame of `init`'s scan.
#[inline(never)]
pub fn bind(function: &Function, next: &mut usize) -> Result<(), &'static str> {
    let information = function.read8(usize::from(radix_sdhci::SLOT_INFORMATION));
    let first = usize::from(information & 0x7);
    let slots = (information >> 4 & 0x7) + 1;
    if first > 5 || slots > MAX_SLOTS {
        return Err("its Slot Information register names no BAR");
    }
    function.enable_memory_and_dma();
    for number in 0..slots {
        let mut place = TextBuf::<40>::new();
        let _ = write!(place, "{}, slot {}", function.address(), number + 1);
        match add_slot(function, first + usize::from(number), &place) {
            Ok(true) => *next += 1,
            Ok(false) => {}
            Err(why) => warn!("Disk: the SD card reader at {place}: {why}"),
        }
    }
    Ok(())
}

/// Sets up the slot whose registers are in BAR `bar` and the card in it;
/// whether there is one.
#[inline(never)]
fn add_slot(function: &Function, bar: usize, place: &TextBuf<40>) -> Result<bool, &'static str> {
    let Some(Bar::Memory { address, size, .. }) = function.bar(bar) else {
        return Err("its registers are not in memory");
    };
    if size < radix_sdhci::REGISTERS as u64 {
        return Err("its registers are too few");
    }
    let regs = <Arch as Devices>::map_device(address, radix_sdhci::REGISTERS)
        .ok_or("its registers could not be mapped")?;
    let source = function.address();
    let mut alloc = |pages: usize, below: bool| <Arch as Devices>::alloc_dma(source, pages, below);
    let mut slot = Slot::new(regs, &mut alloc, &mut queue::delay).map_err(|err| err.as_str())?;
    if !slot.present() {
        info!("Disk: the SD card reader at {place} is empty");
        return Ok(false);
    }
    let card = slot.identify(&mut queue::delay).map_err(|err| err.as_str())?;
    let bounce = if slot.reaches_64() {
        None
    } else {
        Some(alloc(SEGMENTS, true).ok_or("out of memory below 4 GiB")?)
    };
    let (major, minor) = slot.version();
    let controller = new_slot(slot, bounce, place.clone())?;
    let number = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = Name::new();
    let _ = write!(name, "mmcblk{number}");
    let info = Info {
        sectors: card.blocks,
        logical_block: radix_sdhci::BLOCK,
        block_size: radix_sdhci::BLOCK,
        read_only: card.read_only,
        max_segments: SEGMENTS,
        discard_sectors: 0,
        rotational: false,
    };
    let disk = new_disk(controller, info, name.clone(), source)?;
    let volume = Volume::new(disk, 0, info).ok_or("out of memory")?;
    let volume =
        Arc::try_new_in(volume, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")?;
    log_card(&name, &card, place, (major, minor));
    let device: DeviceRef = volume;
    register(Entry {
        name: name.clone(),
        device: device.clone(),
        place: None,
        ident: ident(&card),
    });
    partitions(&name, &device);
    Ok(true)
}

/// Says what `card`, named `name`, is.
fn log_card(name: &Name, card: &Card, place: &TextBuf<40>, (major, minor): (u8, u8)) {
    let product = card.name();
    info!(
        "Disk: {name} is an {} card of {} in the SD card reader at {place}: {}, {} MHz on a \
         {}-bit bus, a version {major}.{minor:02} reader{}",
        card.kind(),
        Size(card.blocks * radix_block::SECTOR),
        if product.is_empty() { "unnamed" } else { product },
        card.khz / 1000,
        card.width,
        if card.read_only { ", write-protected" } else { "" },
    );
}

/// What a card says it is, as Linux shows it: its product's name, its
/// serial number in hex and its revision.
fn ident(card: &Card) -> Option<Ident> {
    let mut serial = TextBuf::<16>::new();
    let _ = write!(serial, "0x{:08x}", card.cid.serial);
    let mut firmware = TextBuf::<8>::new();
    let _ = write!(firmware, "{}.{}", card.cid.revision >> 4, card.cid.revision & 0xf);
    Some(Ident {
        model: super::padded(card.name().as_bytes()),
        serial: super::padded(serial.as_str().as_bytes()),
        firmware: super::padded(firmware.as_str().as_bytes()),
    })
}

/// The slot's part of the queue, on the heap.
#[inline(never)]
fn new_slot(
    slot: Slot,
    bounce: Option<Dma>,
    place: TextBuf<40>,
) -> Result<Part<SdSlot>, &'static str> {
    let heap = || HEAP.with(Flags::MAY_SLEEP);
    let mut buffers = Vec::new_in(heap());
    let mut descriptors = Vec::new_in(heap());
    let mut copy = Vec::new_in(heap());
    buffers.try_reserve_exact(SEGMENTS).map_err(|_| "out of memory")?;
    descriptors.try_reserve_exact(SEGMENTS).map_err(|_| "out of memory")?;
    if bounce.is_some() {
        copy.try_reserve_exact(PAGE).map_err(|_| "out of memory")?;
        while copy.len() < PAGE {
            let _ = copy.push_within_capacity(0);
        }
    }
    let controller = SdSlot {
        slot,
        flight: None,
        done: None,
        bounce,
        buffers,
        descriptors,
        copy,
        why: None,
        place,
    };
    queue::part(controller).ok_or("out of memory")
}

/// The queue of a slot, on the heap: polled, with no write cache.
#[inline(never)]
fn new_disk(
    controller: Part<SdSlot>,
    info: Info,
    name: Name,
    source: radix_hw::pci::Address,
) -> Result<Arc<Disk<SdSlot>, Kmalloc>, &'static str> {
    let once = core::iter::once(controller);
    let disk = Disk::new(once, info, false, false, false, name, source).ok_or("out of memory")?;
    Arc::try_new_in(disk, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")
}
