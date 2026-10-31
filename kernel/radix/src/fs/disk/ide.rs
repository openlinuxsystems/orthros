// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! PCI IDE controllers (`radix-ide`, Phase 11g, ADR 0244): the ATA disks
//! on their channels, and the SATA disks of a chipset whose firmware sets
//! its SATA controller to IDE mode. Each disk is named `sda` and on, as
//! the SATA disks on AHCI controllers are, and as Linux names them.
//!
//! [`bind`] turns on the controller's I/O ports and bus master, finds each
//! channel's ports (the legacy ones, or BARs 0 to 3 in native mode) and
//! its bus-master registers (BAR 4), and asks both ends of each channel
//! what they are. A channel takes one command at a time for its two
//! disks, so it is one lane of one queue ([`Disk`]), and each disk is a
//! volume of it, as an NVMe controller's namespaces are: the master is
//! target 0, the slave 1. The channels are polled, by the threads waiting
//! for them: the kernel routes neither the legacy IRQs 14 and 15 nor an
//! IDE controller's INTx.
//!
//! The bus master reaches only the first 4 GiB, which page caches are not
//! kept to: a request whose buffers all lie below it goes to them, and
//! any other through a bounce buffer of [`SEGMENTS`] pages below 4 GiB,
//! copied before a write and after a read.
//!
//! A SATA controller in IDE mode runs one command at a time on each
//! channel, with no queuing, hot-plug or link power management: when a
//! disk says it is Serial ATA, the log suggests setting the controller to
//! AHCI in the firmware's setup, as the installer will.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_ahci::ata::trimmed;
use radix_block::{DeviceRef, Error, Info};
use radix_hal::{Devices, UserMode};
use radix_hw::pci::{Bar, Function, command};
use radix_hw::Dma;
use radix_ide::{Channel, Device, Op as IdeOp, interface};
use radix_klog::{TextBuf, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};

use super::queue::{self, Controller, Disk, Op, Outcome, Part, Request, Volume};
use super::{Entry, Ident, Name, Size, partitions, register};
use crate::Arch;
use crate::ioports::IoWindow;

/// The most segments of a request, each at most a page: the bounce
/// buffer's pages.
const SEGMENTS: usize = 32;
const PAGE: usize = 4096;
/// The legacy ports are taken: by the first controller in compatibility
/// mode on each channel.
static LEGACY: [AtomicBool; 2] = [const { AtomicBool::new(false) }; 2];

/// What the channel knows of a disk on it.
#[derive(Clone, Copy, Default)]
struct Drive {
    present: bool,
    /// It has a write cache that is on, which FLUSH CACHE EXT empties.
    flush: bool,
}

/// The request in flight.
#[derive(Clone, Copy)]
struct Flight {
    slot: u8,
    /// A read through the bounce buffer, whose bytes go to the request's
    /// buffers, kept in `buffers`, once it is done.
    bounced_read: bool,
}

/// A channel as the queue drives it.
pub struct IdeChannel {
    channel: Channel<IoWindow>,
    drives: [Drive; 2],
    flight: Option<Flight>,
    /// A request done without the disk: a flush of one with no cache.
    done: Option<(u8, Outcome)>,
    /// The bounce buffer, below 4 GiB.
    bounce: Dma,
    /// The buffers of a bounced read; or the PRDs' buffers of one that
    /// goes to its own.
    buffers: Vec<(u64, u32), Kmalloc>,
    prds: Vec<(u32, u32), Kmalloc>,
    /// A page between a request's buffer and the bounce buffer.
    copy: Vec<u8, Kmalloc>,
    /// Why the last request failed, for the log.
    why: Option<(u8, &'static str)>,
    /// The channel's place, for the log: `primary` or `secondary`, and
    /// the controller's address.
    place: TextBuf<40>,
}

impl IdeChannel {
    /// Copies a bounced read's bytes to its buffers.
    fn copy_back(&mut self) {
        let mut at = self.bounce.phys();
        for &(phys, len) in &self.buffers {
            let part = &mut self.copy[..len as usize];
            <Arch as UserMode>::read_frames(at, part);
            <Arch as UserMode>::write_frames(phys, part);
            at += u64::from(len);
        }
    }

    /// The PRDs' buffers for `data`: themselves when all lie below 4 GiB,
    /// else the bounce buffer, filled for a write.
    fn place_buffers(&mut self, data: &[(u64, u32)], write: bool) -> Result<bool, Error> {
        self.prds.clear();
        self.buffers.clear();
        if data.len() > SEGMENTS || data.iter().any(|&(_, len)| len as usize > PAGE) {
            return Err(Error::OutOfRange);
        }
        if data.iter().all(|&(phys, len)| phys + u64::from(len) <= 1 << 32) {
            for &(phys, len) in data {
                let _ = self.prds.push_within_capacity((phys as u32, len));
            }
            return Ok(false);
        }
        let mut at = self.bounce.phys();
        for &(phys, len) in data {
            let _ = self.buffers.push_within_capacity((phys, len));
            if write {
                let part = &mut self.copy[..len as usize];
                <Arch as UserMode>::read_frames(phys, part);
                <Arch as UserMode>::write_frames(at, part);
            }
            at += u64::from(len);
        }
        let total = (at - self.bounce.phys()) as u32;
        let _ = self.prds.push_within_capacity((self.bounce.phys() as u32, total));
        Ok(true)
    }
}

impl Controller for IdeChannel {
    fn room(&self, _op: Op, _segments: usize) -> bool {
        self.flight.is_none() && self.done.is_none()
    }

    fn submit(&mut self, slot: u8, request: &Request<'_>, _more: bool) -> Result<(), Error> {
        let drive = u8::try_from(request.target).map_err(|_| Error::OutOfRange)?;
        let known = self.drives.get(usize::from(drive)).copied().unwrap_or_default();
        if !known.present {
            return Err(Error::OutOfRange);
        }
        let op = match request.op {
            Op::Read => IdeOp::Read,
            // The disk is not said to write through: the queue flushes.
            Op::Write | Op::WriteThrough => IdeOp::Write,
            Op::Flush if !known.flush => {
                self.done = Some((slot, Outcome::Done));
                return Ok(());
            }
            Op::Flush => IdeOp::Flush,
            Op::Discard(_) | Op::Smart(_) => return Err(Error::Unsupported),
        };
        // The disk counts in its own sectors, the request in 512 bytes.
        let unit = request.unit.max(1);
        let lba = request.sector / unit;
        let bytes: u64 = request.data.iter().map(|&(_, len)| u64::from(len)).sum();
        let sector = unit * radix_block::SECTOR;
        let count = u32::try_from(bytes / sector).map_err(|_| Error::OutOfRange)?;
        let bounced_read = if op == IdeOp::Flush {
            self.prds.clear();
            false
        } else {
            self.place_buffers(request.data, op == IdeOp::Write)? && op == IdeOp::Read
        };
        self.channel.start(drive, op, lba, count, &self.prds).map_err(|err| match err {
            radix_ide::Error::BadRequest => Error::OutOfRange,
            _ => Error::Io,
        })?;
        self.flight = Some(Flight { slot, bounced_read });
        Ok(())
    }

    fn complete(&mut self) -> Option<(u8, Outcome)> {
        if let Some(done) = self.done.take() {
            return Some(done);
        }
        let flight = self.flight?;
        let result = self.channel.poll()?;
        self.flight = None;
        match result {
            Ok(()) => {
                if flight.bounced_read {
                    self.copy_back();
                }
                Some((flight.slot, Outcome::Done))
            }
            Err(err) => {
                let why = match err {
                    radix_ide::Error::Bus => "the bus master could not reach memory",
                    radix_ide::Error::Device { error, .. } if error & 1 << 6 != 0 => {
                        "the disk could not read a sector"
                    }
                    radix_ide::Error::Device { error, .. } if error & 1 << 2 != 0 => {
                        "the disk refused the command"
                    }
                    _ => "the disk failed the command",
                };
                self.why = Some((flight.slot, why));
                Some((flight.slot, Outcome::Failed))
            }
        }
    }

    fn why(&self, slot: u8) -> Option<&'static str> {
        self.why.filter(|&(failed, _)| failed == slot).map(|(_, why)| why)
    }

    fn reset(lanes: &mut [Part<IdeChannel>]) -> bool {
        for lane in lanes {
            let ready = lane.channel.reset(&mut queue::delay);
            if let Some(flight) = lane.flight.take() {
                lane.done = Some((flight.slot, Outcome::Retry));
            }
            warn!(
                "Disk: a request on the IDE controller's {} went unanswered for 30 s; the \
                 channel was reset{}",
                lane.place,
                if ready { ", and what was in flight goes again" } else { ", and is not ready" }
            );
        }
        true
    }
}

/// The I/O ports of a BAR, or `None` if it is not in I/O space.
fn io_bar(function: &Function, index: usize) -> Option<(u16, u16)> {
    match function.bar(index)? {
        Bar::Io { port, size } => {
            let port = u16::try_from(port).ok()?;
            Some((port, u16::try_from(size).unwrap_or(u16::MAX)))
        }
        Bar::Memory { .. } => None,
    }
}

/// Sets up the IDE controller `function` and the disks on its channels,
/// counting them in `next`. Kept out of the frame of `init`'s scan.
#[inline(never)]
pub fn bind(function: &Function, next: &mut usize) -> Result<(), &'static str> {
    let (_, _, programming) = function.class();
    if programming & interface::BUS_MASTER == 0 {
        return Err("it has no bus-master DMA, which the kernel needs");
    }
    let Some((bus_master, size)) = io_bar(function, radix_ide::BUS_MASTER_BAR) else {
        return Err("its bus-master registers (BAR 4) are not in I/O space");
    };
    if size < 2 * radix_ide::BUS_MASTER_PORTS {
        return Err("its bus-master registers (BAR 4) are too few");
    }
    function.set_command(function.command() | command::IO | command::BUS_MASTER);
    let mut serial = false;
    for number in 0..2u8 {
        let native = programming
            & if number == 0 { interface::PRIMARY_NATIVE } else { interface::SECONDARY_NATIVE }
            != 0;
        let mut place = TextBuf::<40>::new();
        let which = if number == 0 { "primary" } else { "secondary" };
        let _ = write!(place, "{which} channel at {}", function.address());
        let ports = if native {
            let bar = 2 * usize::from(number);
            match (io_bar(function, bar), io_bar(function, bar + 1)) {
                // The control port is the third of its block's four.
                (Some((command, _)), Some((control, _))) => Some((command, control + 2)),
                _ => None,
            }
        } else if LEGACY[usize::from(number)].swap(true, Ordering::AcqRel) {
            None
        } else {
            Some(radix_ide::LEGACY_PORTS[usize::from(number)])
        };
        let Some((command, control)) = ports else {
            info!("Disk: IDE {place}: its ports are not in I/O space, or taken; not used");
            continue;
        };
        let windows = (
            IoWindow::new(command, radix_ide::COMMAND_PORTS),
            IoWindow::new(control, 1),
            IoWindow::new(bus_master + u16::from(number) * 8, radix_ide::BUS_MASTER_PORTS),
        );
        match add_channel(function, windows, place.clone(), next) {
            Ok(found) => serial |= found,
            Err(why) => warn!("Disk: IDE {place}: {why}"),
        }
    }
    if serial {
        info!(
            "Disk: the SATA controller at {} is in IDE mode, which runs one command at a time \
             on each channel, with no queuing or hot-plug: setting it to AHCI in the \
             firmware's setup lets its disks run faster",
            function.address()
        );
    }
    Ok(())
}

/// What a channel's two ends are, and which are disks.
struct Found {
    devices: [Device; 2],
}

/// Sets up the channel of `windows` (its command block, control port and
/// bus-master registers) and its disks; whether one is Serial ATA.
#[inline(never)]
fn add_channel(
    function: &Function,
    (command, control, bus_master): (IoWindow, IoWindow, IoWindow),
    place: TextBuf<40>,
    next: &mut usize,
) -> Result<bool, &'static str> {
    let source = function.address();
    let mut alloc = |pages: usize, below: bool| <Arch as Devices>::alloc_dma(source, pages, below);
    let mut channel =
        Channel::new(command, control, bus_master, &mut alloc).ok_or("out of memory")?;
    if !channel.present() {
        info!("Disk: IDE {place}: empty");
        return Ok(false);
    }
    if !channel.reset(&mut queue::delay) {
        info!("Disk: IDE {place}: no device became ready after a reset");
    }
    let found = Found {
        devices: [channel.identify(0, &mut queue::delay), channel.identify(1, &mut queue::delay)],
    };
    let mut drives = [Drive::default(); 2];
    let mut serial = false;
    for (end, device) in found.devices.iter().enumerate() {
        let which = if end == 0 { "master" } else { "slave" };
        match device {
            Device::Absent => {}
            Device::Atapi => {
                info!("Disk: IDE {place}, {which}: an optical drive (ATAPI), not driven yet");
            }
            Device::Unsupported(why) => {
                let why = radix_ahci::Absent::Unsupported(*why);
                info!("Disk: IDE {place}, {which}: {why}; not used");
            }
            Device::Ata { identity, serial: sata } => {
                drives[end] = Drive { present: true, flush: identity.write_cache && identity.flush };
                serial |= sata;
            }
        }
    }
    if drives.iter().all(|drive| !drive.present) {
        return Ok(serial);
    }
    let bounce = alloc(SEGMENTS, true).ok_or("out of memory below 4 GiB")?;
    let controller = new_channel(channel, drives, bounce, place.clone())?;
    // Each disk's name; the queue is named after the first.
    let mut names: [Option<(u32, Name)>; 2] = [None, None];
    for (end, drive) in drives.iter().enumerate() {
        if drive.present {
            names[end] = super::take_sd_name();
        }
    }
    let give_back = |names: &[Option<(u32, Name)>; 2]| {
        for (letter, _) in names.iter().flatten() {
            super::give_back_sd_letter(*letter);
        }
    };
    if drives.iter().zip(&names).any(|(drive, name)| drive.present && name.is_none()) {
        give_back(&names);
        return Err("too many SATA and USB disks");
    }
    let first = names.iter().flatten().next().map(|(_, name)| name.clone()).unwrap_or_default();
    let cache = drives.iter().any(|drive| drive.flush);
    let template = Info {
        sectors: 0,
        logical_block: 512,
        block_size: 512,
        read_only: false,
        max_segments: SEGMENTS,
        discard_sectors: 0,
        rotational: true,
    };
    let disk = new_disk(controller, template, cache, first, source)
        .inspect_err(|_| give_back(&names))?;
    for (end, device) in found.devices.iter().enumerate() {
        let (Device::Ata { identity, serial }, Some((letter, name))) = (device, &names[end]) else {
            continue;
        };
        let which = if end == 0 { "master" } else { "slave" };
        if let Err(why) = add_disk(&disk, end as u32, identity, *serial, name, &place, which) {
            super::give_back_sd_letter(*letter);
            warn!("Disk: IDE {place}, {which}: {why}");
        } else {
            *next += 1;
        }
    }
    Ok(serial)
}

/// The channel's part of the queue, on the heap.
#[inline(never)]
fn new_channel(
    channel: Channel<IoWindow>,
    drives: [Drive; 2],
    bounce: Dma,
    place: TextBuf<40>,
) -> Result<Part<IdeChannel>, &'static str> {
    let heap = || HEAP.with(Flags::MAY_SLEEP);
    let mut buffers = Vec::new_in(heap());
    let mut prds = Vec::new_in(heap());
    let mut copy = Vec::new_in(heap());
    buffers.try_reserve_exact(SEGMENTS).map_err(|_| "out of memory")?;
    prds.try_reserve_exact(SEGMENTS).map_err(|_| "out of memory")?;
    copy.try_reserve_exact(PAGE).map_err(|_| "out of memory")?;
    while copy.len() < PAGE {
        let _ = copy.push_within_capacity(0);
    }
    let controller = IdeChannel {
        channel,
        drives,
        flight: None,
        done: None,
        bounce,
        buffers,
        prds,
        copy,
        why: None,
        place,
    };
    queue::part(controller).ok_or("out of memory")
}

/// The queue of a channel, on the heap: polled.
#[inline(never)]
fn new_disk(
    controller: Part<IdeChannel>,
    template: Info,
    cache: bool,
    name: Name,
    source: radix_hw::pci::Address,
) -> Result<Arc<Disk<IdeChannel>, Kmalloc>, &'static str> {
    let once = core::iter::once(controller);
    let disk = Disk::new(once, template, cache, false, false, name, source).ok_or("out of memory")?;
    Arc::try_new_in(disk, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")
}

/// Sets up the disk at end `target` of the channel of `disk`, named
/// `name`.
fn add_disk(
    disk: &Arc<Disk<IdeChannel>, Kmalloc>,
    target: u32,
    identity: &radix_ahci::ata::Identity,
    serial: bool,
    name: &Name,
    place: &TextBuf<40>,
    which: &str,
) -> Result<(), &'static str> {
    // The disk counts in its own sectors, the block layer in 512 bytes.
    let unit = u64::from(identity.logical_sector) / radix_block::SECTOR;
    let info = Info {
        sectors: identity.sectors.saturating_mul(unit),
        logical_block: identity.logical_sector,
        block_size: identity.physical_sector,
        read_only: false,
        max_segments: SEGMENTS,
        discard_sectors: 0,
        // A disk that does not say spins, as Linux takes it.
        rotational: identity.rotational.unwrap_or(true),
    };
    let volume = Volume::new(disk.clone(), target, info).ok_or("out of memory")?;
    let volume = Arc::try_new_in(volume, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")?;
    info!(
        "Disk: {name} is {} of {} on the IDE {place}, {which}: {}, {} bytes a block{}{}{}",
        if serial { "a SATA disk" } else { "an ATA disk" },
        Size(info.sectors * radix_block::SECTOR),
        trimmed(&identity.model),
        info.block_size,
        super::sector_note(&info),
        if identity.write_cache { ", write cache" } else { "" },
        if info.rotational { "" } else { ", solid state" },
    );
    let device: DeviceRef = volume;
    let ident = Ident {
        model: identity.model,
        serial: super::padded(&identity.serial),
        firmware: identity.firmware,
    };
    register(Entry { name: name.clone(), device: device.clone(), place: None, ident: Some(ident) });
    partitions(name, &device);
    Ok(())
}
