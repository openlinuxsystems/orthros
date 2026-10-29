// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB disks: the logical units behind a USB stick's, a card reader's or
//! a USB disk's mass storage interface (Phase 11b, ADR 0161).
//!
//! [`run`] is a stick's thread (`crate::usb::storage`), which carries the
//! interface's commands over the Bulk-Only Transport
//! (`radix_usb::storage::Bot`). It asks how many logical units the
//! interface has (Get Max LUN) and finds out what each is
//! (`radix_scsi::probe`). Each that is a disk with a medium in it takes
//! the first `sd` letter free once the PCI scan has named the SATA disks
//! (and, at boot, the sticks found before it theirs, so that the names
//! are the same each boot), and appears in `/devices` and `/kernel/block`
//! with its partitions,
//! whose table the thread reads itself, since it is the one that serves
//! the disk. Then it serves the disks' requests, which come through the
//! shared queue ([`super::queue`]) one at a time, as a Bulk-Only interface
//! takes them: the queue's part of a disk ([`UsbLun`]) hands its request
//! to the thread, which sends it as READ(10) or WRITE(10) (READ(16) and
//! WRITE(16) on a disk with more blocks than 32 bits count), or as
//! SYNCHRONIZE CACHE for a flush if the disk says it has a write cache,
//! and hands back how it ended. As Linux's `sd` does for USB disks, the
//! last eight blocks go one a command (card readers fail to read the last
//! block with others); a command that a UNIT ATTENTION, an abort, a
//! hardware error or a broken transport ends goes again (the queue's
//! tries); one that finds the disk stopped starts it first. A request the
//! queue waits 30 s for has its transfer stopped, which resets the
//! interface, and goes again.
//!
//! When the stick is unplugged, every request in flight and after fails,
//! the disks and their partitions leave `/devices` and `/kernel/block`,
//! their letters are free again, and the thread ends.

use alloc::sync::Arc;
use alloc::vec::Vec;

use radix_block::{DeviceRef, Error, Info, SECTOR, partition};
use radix_klog::{TextBuf, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_scsi::{Absent, Cdb, Data, Found, Meaning, Reply, Transport, TransportError, key, probe};
use radix_sync::SpinLock;
use radix_usb::storage::Bot;

use super::queue::{self, Controller, Disk, Op, Outcome, Part, Request, Volume};
use super::{Entry, Ident, Name, Size, register};
use crate::sched::{self, ThreadRef};
use crate::usb::storage::{Link, LinkRef, StickPipes};

/// The most buffers a request to a USB 2 disk has: 120 KiB in pages,
/// the most Linux's usb-storage sends, since some sticks fail more.
const USB2_SEGMENTS: usize = 30;
/// The most a request to a SuperSpeed disk has: 512 KiB in pages, which
/// one transfer of the controller holds.
const SUPER_SEGMENTS: usize = radix_xhci::MAX_TD_BUFFERS;
/// How long a read, write or flush may take: Linux's `sd`'s.
const TIMEOUT_MS: u32 = 30_000;
/// The blocks at a disk's end that go one a command (Linux's
/// SD_LAST_BUGGY_SECTORS).
const LAST_BLOCKS: u64 = 8;
/// How long the thread sleeps with no request, at most.
const IDLE_NS: u64 = 1_000_000_000;
/// How long a disk that says it is becoming ready is given before its
/// request goes again.
const READY_NS: u64 = 1_000_000_000;
/// How many times the partition table's reads go.
const TRIES: u32 = 3;

/// Where a logical unit's request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// It has none: room for one.
    Idle,
    /// Handed to the thread, as the queue's slot.
    Waiting(u8),
    /// The thread runs it.
    Running(u8),
    /// It ended so.
    Done(u8, Outcome),
}

/// A logical unit's request, between the queue and the stick's thread.
struct Command {
    stage: Stage,
    op: Op,
    /// Its first sector, of 512 bytes.
    sector: u64,
    /// Its buffers (physical address, length).
    frames: Vec<(u64, u32), Kmalloc>,
    /// Why the last one failed or went again, for the log.
    why: Option<&'static str>,
    /// The stick is gone: requests fail at once.
    gone: bool,
}

/// What a logical unit's disk shares with its stick's thread.
struct Shared {
    command: SpinLock<Command>,
    /// The stick's thread, which a request wakes.
    thread: ThreadRef,
    /// The stick's link, which an abort goes through.
    link: LinkRef,
}

/// A logical unit as the queue drives it: one request at a time.
pub struct UsbLun {
    shared: Arc<Shared, Kmalloc>,
}

impl Controller for UsbLun {
    fn room(&self, _op: Op, _segments: usize) -> bool {
        // A gone disk takes every request, to fail it.
        let command = self.shared.command.lock_irq();
        command.gone || command.stage == Stage::Idle
    }

    fn submit(&mut self, slot: u8, request: &Request<'_>, _more: bool) -> Result<(), Error> {
        {
            let mut command = self.shared.command.lock_irq();
            if command.gone {
                return Err(Error::Io);
            }
            if command.stage != Stage::Idle || request.data.len() > command.frames.capacity() {
                return Err(Error::OutOfRange);
            }
            command.frames.clear();
            for &frame in request.data {
                let _ = command.frames.push_within_capacity(frame);
            }
            command.op = request.op;
            command.sector = request.sector;
            command.stage = Stage::Waiting(slot);
        }
        sched::wake(&self.shared.thread);
        Ok(())
    }

    fn complete(&mut self) -> Option<(u8, Outcome)> {
        let mut command = self.shared.command.lock_irq();
        let Stage::Done(slot, outcome) = command.stage else { return None };
        command.stage = Stage::Idle;
        Some((slot, outcome))
    }

    fn why(&self, _slot: u8) -> Option<&'static str> {
        self.shared.command.lock_irq().why
    }

    fn reset(lanes: &mut [Part<UsbLun>]) -> bool {
        // The thread stops the transfer it waits for, which resets the
        // interface; the request goes again.
        for lane in lanes.iter() {
            lane.shared.link.abort();
        }
        true
    }
}

/// A logical unit with a disk, as its stick's thread keeps it.
struct Lun {
    number: u8,
    name: Name,
    /// Its `sd` letter.
    letter: u32,
    found: Found,
    shared: Arc<Shared, Kmalloc>,
    disk: Arc<Disk<UsbLun>, Kmalloc>,
}

impl Lun {
    /// Takes the request waiting, if one is: its slot, what it does and
    /// its first sector, its buffers into `frames`.
    fn take(&self, frames: &mut Vec<(u64, u32), Kmalloc>) -> Option<(u8, Op, u64)> {
        let mut command = self.shared.command.lock_irq();
        let Stage::Waiting(slot) = command.stage else { return None };
        command.stage = Stage::Running(slot);
        frames.clear();
        for &frame in &command.frames {
            // As much room as the command's.
            let _ = frames.push_within_capacity(frame);
        }
        Some((slot, command.op, command.sector))
    }

    /// Hands back the request in `slot`, which ended with `outcome`.
    fn finish(&self, slot: u8, outcome: Outcome, why: Option<&'static str>) {
        {
            let mut command = self.shared.command.lock_irq();
            command.stage = Stage::Done(slot, outcome);
            command.why = why;
        }
        self.disk.collect(0);
    }

    /// The stick is gone: fails the request it has, and every one after.
    fn gone(&self) {
        {
            let mut command = self.shared.command.lock_irq();
            command.gone = true;
            if let Stage::Waiting(slot) | Stage::Running(slot) = command.stage {
                command.stage = Stage::Done(slot, Outcome::Failed);
                command.why = Some("its USB device was unplugged");
            }
        }
        self.disk.collect(0);
    }
}

/// What the thread works with: a request's buffers, and a part of them.
struct Work {
    frames: Vec<(u64, u32), Kmalloc>,
    part: Vec<(u64, u32), Kmalloc>,
}

/// The thread of the stick `link`: finds its disks, serves them until it
/// is unplugged, and takes them away then.
pub fn run(link: &LinkRef) {
    let stick = Stick::new(link);
    crate::usb::storage::probed(link);
    let Some(mut stick) = stick else { return };
    stick.serve();
    for lun in &stick.luns {
        lun.gone();
        super::unregister(lun.name.as_str());
        super::give_back_sd_letter(lun.letter);
        info!("Disk: {} is gone with its USB device", lun.name);
    }
}

/// A stick's transport and disks, as its thread runs them.
struct Stick<'a> {
    link: &'a LinkRef,
    bot: Bot<StickPipes<'a>>,
    luns: Vec<Lun, Kmalloc>,
    work: Work,
}

impl<'a> Stick<'a> {
    /// Finds the disks behind the stick `link`; `None` if it has none, or
    /// memory runs out. Kept out of [`run`]'s frame, which the probe's
    /// buffers would not fit in.
    #[inline(never)]
    fn new(link: &'a LinkRef) -> Option<Stick<'a>> {
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let pipes = StickPipes::new(link);
        let mut frames = Vec::new_in(heap);
        let mut part = Vec::new_in(heap);
        let mut luns = Vec::new_in(heap);
        let reserved = frames.try_reserve_exact(SUPER_SEGMENTS).is_ok()
            && part.try_reserve_exact(SUPER_SEGMENTS).is_ok();
        let (Some(pipes), true) = (pipes, reserved) else {
            warn!("Disk: USB {}: out of memory for its disks", link.place);
            return None;
        };
        let mut bot = Bot::new(pipes, link.interface);
        // A stick unplugged already.
        let max_lun = bot.max_lun().ok()?;
        let mut wait = |ms: u32| sched::sleep_ns(u64::from(ms) * 1_000_000);
        for number in 0..=max_lun {
            let mut lun = TextBuf::<16>::new();
            if max_lun > 0 {
                let _ = core::fmt::Write::write_fmt(&mut lun, format_args!(" LUN {number}"));
            }
            let found = match probe(&mut bot, number, &mut wait) {
                Ok(found) => found,
                Err(Absent::Gone) => break,
                Err(why) => {
                    info!("Disk: USB {}{lun}: {why}; not used", link.place);
                    continue;
                }
            };
            if luns.try_reserve(1).is_err() {
                warn!("Disk: USB {}{lun}: out of memory", link.place);
                break;
            }
            match add(link, &mut bot, number, found, &lun) {
                Ok(added) => {
                    let _ = luns.push_within_capacity(added);
                }
                Err(why) => warn!("Disk: USB {}{lun}: {why}", link.place),
            }
        }
        if luns.is_empty() {
            return None;
        }
        Some(Stick { link, bot, luns, work: Work { frames, part } })
    }

    /// Serves the disks' requests until the stick is unplugged.
    fn serve(&mut self) {
        while !self.link.is_gone() {
            let mut served = false;
            for at in 0..self.luns.len() {
                let Some((slot, op, sector)) = self.luns[at].take(&mut self.work.frames) else {
                    continue;
                };
                served = true;
                self.link.forget_abort();
                let (outcome, why) = self.execute(at, op, sector);
                self.luns[at].finish(slot, outcome, why);
            }
            if !served {
                sched::block_until(sched::now_ns().saturating_add(IDLE_NS), "usb-storage");
            }
        }
    }

    /// Carries out `op` from `sector` with the buffers in the work's, on
    /// the disk at `at`; returns how it ended, and why if it did not work.
    fn execute(&mut self, at: usize, op: Op, sector: u64) -> (Outcome, Option<&'static str>) {
        let Lun { number, found, .. } = self.luns[at];
        let into_host = match op {
            Op::Read => true,
            Op::Write | Op::WriteThrough => false,
            Op::Flush => {
                let reply =
                    self.bot.command(number, &Cdb::synchronize_cache(), Data::None, TIMEOUT_MS);
                return self.judge(number, reply, true);
            }
            Op::Discard(_) => return (Outcome::Failed, Some("it does not discard")),
            Op::Smart(_) => return (Outcome::Failed, Some("it keeps no SMART record")),
        };
        let block = u64::from(found.capacity.block_length);
        let unit = block / SECTOR;
        let total: u64 = self.work.frames.iter().map(|&(_, len)| u64::from(len)).sum();
        let (first, blocks) = (sector / unit, total / block);
        let last = found.capacity.blocks.saturating_sub(LAST_BLOCKS);
        let mut done = 0;
        while done < blocks {
            let start = first + done;
            let mut count = blocks - done;
            if start + count > last {
                count = if start < last { last - start } else { 1 };
            }
            slice(&self.work.frames, done * block, count * block, &mut self.work.part);
            let cdb = if into_host {
                Cdb::read(start, count as u32, found.sixteen)
            } else {
                Cdb::write(start, count as u32, found.sixteen)
            };
            let Some(cdb) = cdb else {
                return (Outcome::Failed, Some("its blocks are past what a command reaches"));
            };
            let data = Data::Frames { into_host, frames: &self.work.part };
            let reply = self.bot.command(number, &cdb, data, TIMEOUT_MS);
            let (outcome, why) = self.judge(number, reply, false);
            if outcome != Outcome::Done {
                return (outcome, why);
            }
            done += count;
        }
        (Outcome::Done, None)
    }

    /// How a command to logical unit `number` that ended with `reply`
    /// ends its request, as Linux's SCSI layer has it for a USB disk: a
    /// `flush` the disk does not take did not need doing.
    fn judge(
        &mut self,
        number: u8,
        reply: Result<Reply, TransportError>,
        flush: bool,
    ) -> (Outcome, Option<&'static str>) {
        let sense = match reply {
            Ok(Reply::Good { residue: 0 }) => return (Outcome::Done, None),
            Ok(Reply::Good { .. }) => return (Outcome::Retry, Some("it moved less than asked")),
            Ok(Reply::Failed(Some(sense))) => sense,
            Ok(Reply::Failed(None)) => {
                return (Outcome::Retry, Some("it failed the command, and did not say why"));
            }
            Err(TransportError::Gone) => {
                return (Outcome::Failed, Some("its USB device was unplugged"));
            }
            Err(TransportError::Failed(why)) => return (Outcome::Retry, Some(why)),
        };
        match sense.meaning() {
            Meaning::Fine => (Outcome::Done, None),
            Meaning::Unsupported if flush => (Outcome::Done, None),
            Meaning::Attention | Meaning::Aborted => (Outcome::Retry, Some(sense.key_name())),
            Meaning::BecomingReady => {
                sched::sleep_ns(READY_NS);
                (Outcome::Retry, Some("it is becoming ready"))
            }
            Meaning::NeedsStart => {
                let start = Cdb::start_stop_unit(true, false);
                let _ = self.bot.command(number, &start, Data::None, TIMEOUT_MS);
                (Outcome::Retry, Some("it had stopped"))
            }
            // USB bridges say so of errors they recover from (Linux's
            // retry_hwerror).
            Meaning::Error if sense.key == key::HARDWARE_ERROR => {
                (Outcome::Retry, Some(sense.key_name()))
            }
            Meaning::NoMedium => (Outcome::Failed, Some("its medium was taken out")),
            Meaning::WriteProtected => (Outcome::Failed, Some("its medium is write-protected")),
            _ => (Outcome::Failed, Some(sense.key_name())),
        }
    }
}

/// Puts the buffers of `frames` that hold bytes `from` to `from + len`
/// (counting from the first buffer's) in `out`, cutting the first and
/// last.
fn slice(frames: &[(u64, u32)], from: u64, len: u64, out: &mut Vec<(u64, u32), Kmalloc>) {
    out.clear();
    let (end, mut at) = (from + len, 0u64);
    for &(phys, size) in frames {
        let (start, stop) = (at, at + u64::from(size));
        at = stop;
        if stop <= from || start >= end {
            continue;
        }
        let (lo, hi) = (start.max(from), stop.min(end));
        // No more parts than the buffers.
        let _ = out.push_within_capacity((phys + (lo - start), (hi - lo) as u32));
    }
}

/// Gives the disk at logical unit `number`, as `found` says it is, a name
/// and its queue, and registers it and its partitions.
fn add(
    link: &LinkRef,
    bot: &mut Bot<StickPipes<'_>>,
    number: u8,
    found: Found,
    lun: &TextBuf<16>,
) -> Result<Lun, &'static str> {
    // The SATA disks found at boot are named first, then the sticks'
    // in the order they were found.
    super::wait_for_scan();
    crate::usb::storage::wait_for_turn(link);
    let (letter, name) = super::take_sd_name().ok_or("too many SATA and USB disks")?;
    let (added, device) = add_named(link, number, found, name, letter, lun)
        .inspect_err(|_| super::give_back_sd_letter(letter))?;
    read_partitions(bot, &added, &device);
    Ok(added)
}

/// [`add`]'s part with the disk named `name`, after `letter`.
fn add_named(
    link: &LinkRef,
    number: u8,
    found: Found,
    name: Name,
    letter: u32,
    lun: &TextBuf<16>,
) -> Result<(Lun, DeviceRef), &'static str> {
    let heap = HEAP.with(Flags::MAY_SLEEP);
    let capacity = found.capacity;
    let unit = u64::from(capacity.block_length) / SECTOR;
    let info = Info {
        sectors: capacity.blocks.saturating_mul(unit),
        logical_block: capacity.block_length,
        block_size: capacity.physical_block(),
        read_only: found.read_only,
        max_segments: if link.speed.is_super() { SUPER_SEGMENTS } else { USB2_SEGMENTS },
        discard_sectors: 0,
        // A disk that does not say spins, as Linux takes it.
        rotational: true,
    };
    let mut frames = Vec::new_in(heap);
    frames.try_reserve_exact(info.max_segments).map_err(|_| "out of memory")?;
    let command =
        Command { stage: Stage::Idle, op: Op::Read, sector: 0, frames, why: None, gone: false };
    let thread = sched::current().ok_or("it has no thread")?;
    let shared = Shared { command: SpinLock::new(command), thread, link: link.clone() };
    let shared = Arc::try_new_in(shared, heap).map_err(|_| "out of memory")?;
    let part = queue::part(UsbLun { shared: shared.clone() }).ok_or("out of memory")?;
    let disk =
        Disk::new(core::iter::once(part), info, found.write_cache, false, true, name.clone())
            .ok_or("out of memory")?;
    let disk = Arc::try_new_in(disk, heap).map_err(|_| "out of memory")?;
    let inquiry = found.inquiry;
    info!(
        "Disk: {name} is a USB disk of {} on {}{lun}: {} {} {}, {} bytes a block{}{}{}{}",
        Size(info.sectors * SECTOR),
        link.place,
        inquiry.vendor(),
        inquiry.product(),
        inquiry.revision(),
        info.block_size,
        super::sector_note(&info),
        if inquiry.removable { ", removable" } else { "" },
        if found.read_only { ", write-protected" } else { "" },
        if found.write_cache { ", write cache" } else { "" },
    );
    let volume = Volume::new(disk.clone(), 0, info).ok_or("out of memory")?;
    let device: DeviceRef = Arc::try_new_in(volume, heap).map_err(|_| "out of memory")?;
    let ident = Some(ident(link, &found));
    register(Entry { name: name.clone(), device: device.clone(), place: None, ident });
    Ok((Lun { number, name, letter, found, shared, disk }, device))
}

/// What a USB disk says it is, as `/kernel/block` shows it: its vendor
/// and product as the model, its revision as the firmware's, and its
/// device's serial number, as Linux's udev gives it (`ID_SERIAL_SHORT`).
fn ident(link: &Link, found: &Found) -> Ident {
    let mut model = [b' '; 40];
    let (vendor, product) = (found.inquiry.vendor(), found.inquiry.product());
    let words = [vendor.as_bytes(), b" ", product.as_bytes()];
    let mut at = 0;
    for byte in words.iter().flat_map(|word| word.iter()) {
        if at < model.len() && !(at == 0 && *byte == b' ') {
            model[at] = *byte;
            at += 1;
        }
    }
    let mut firmware = [b' '; 8];
    let revision = found.inquiry.revision().as_bytes();
    let n = revision.len().min(firmware.len());
    firmware[..n].copy_from_slice(&revision[..n]);
    Ident { model, serial: super::padded(link.serial.as_str().as_bytes()), firmware }
}

/// Reads the partition table of the disk `lun`, whose block device is
/// `device`, and registers its partitions: through `bot` itself, since
/// the thread that would serve the disk's queue is this one.
fn read_partitions(bot: &mut Bot<StickPipes<'_>>, lun: &Lun, device: &DeviceRef) {
    let heap = HEAP.with(Flags::MAY_SLEEP);
    let block = lun.found.capacity.block_length as usize;
    let mut buffer = Vec::new_in(heap);
    if buffer.try_reserve_exact(block).is_err() {
        warn!("Disk: {}: out of memory for its partition table", lun.name);
        return;
    }
    while buffer.len() < block && buffer.push_within_capacity(0u8).is_ok() {}
    let unit = (block / SECTOR as usize) as u64;
    let mut read = |sector: u64, out: &mut [u8; SECTOR as usize]| {
        let cdb = Cdb::read(sector / unit, 1, lun.found.sixteen).ok_or(partition::Error::Io)?;
        for _ in 0..TRIES {
            match bot.command(lun.number, &cdb, Data::In(&mut buffer), TIMEOUT_MS) {
                Ok(Reply::Good { residue: 0 }) => {
                    let at = ((sector % unit) * SECTOR) as usize;
                    out.copy_from_slice(&buffer[at..at + SECTOR as usize]);
                    return Ok(());
                }
                Err(TransportError::Gone) => break,
                _ => {}
            }
        }
        Err(partition::Error::Io)
    };
    super::partitions_read(&lun.name, device, &mut read);
}
