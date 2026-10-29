// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB disks: the logical units behind a USB stick's, a card reader's or
//! a USB disk's mass storage interface (Phase 11b, ADRs 0161 and 0162).
//!
//! [`run`] is a stick's thread (`crate::usb::storage`), which carries the
//! interface's commands over the Bulk-Only Transport
//! (`radix_usb::storage::Bot`) or USB Attached SCSI
//! (`radix_usb::storage::uas`). It finds out how many logical units the
//! interface has (Get Max LUN over Bulk-Only, REPORT LUNS over UAS, as
//! Linux's SCSI scan asks) and what each is (`radix_scsi::probe`). Each
//! that is a disk with a medium in it takes the first `sd` letter free
//! once the PCI scan has named the SATA disks (and, at boot, the sticks
//! found before it theirs, so that the names are the same each boot), and
//! appears in `/devices` and `/kernel/block` with its partitions, whose
//! table the thread reads itself, since it is the one that serves the
//! disk.
//!
//! Then it serves the disks' requests, which come through the shared
//! queue ([`super::queue`]): one at a time over Bulk-Only, as the
//! transport takes them; over UAS as many at once as the device has tags
//! for, shared among its disks, each request's commands under a tag of
//! its own. The queue's part of a disk ([`UsbLun`]) hands its requests to
//! the thread, which sends each as READ(10) or WRITE(10) (READ(16) and
//! WRITE(16) on a disk with more blocks than 32 bits count), or as
//! SYNCHRONIZE CACHE for a flush if the disk says it has a write cache,
//! and hands back how it ended. As Linux's `sd` does for USB disks, the
//! last eight blocks go one a command (card readers fail to read the last
//! block with others); a command that a UNIT ATTENTION, an abort, a
//! hardware error or a broken transport ends goes again (the queue's
//! tries); one that finds the disk stopped starts it first, and one that
//! finds it becoming ready waits a second. A request the queue waits 30 s
//! for has its transfer stopped, which resets the Bulk-Only interface, or
//! has the UAS device reset; either way it goes again.
//!
//! When the stick is unplugged, every request in flight and after fails,
//! the disks and their partitions leave `/devices` and `/kernel/block`,
//! their letters are free again, and the thread ends.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use radix_block::{DeviceRef, Error, Info, SECTOR, partition};
use radix_klog::{TextBuf, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_scsi::{
    Absent, Cdb, Data, Found, Meaning, Reply, Transport, TransportError, key, probe, reported_luns,
};
use radix_sync::SpinLock;
use radix_usb::storage::uas::{MAX_TAGS, OneAtATime, STATUS_LENGTH, Uas};
use radix_usb::storage::{Bot, MAX_LUNS};

use super::queue::{self, Controller, Disk, Op, Outcome, Part, Request, SLOTS, Volume};
use super::{Entry, Ident, Name, Size, register};
use crate::sched::{self, ThreadRef};
use crate::usb::storage::{Link, LinkRef, Protocol, StickPipes, UasPipes};

/// The most buffers a request to a USB 2 disk has over Bulk-Only: 120
/// KiB in pages, the most Linux's usb-storage sends, since some sticks
/// fail more.
const USB2_SEGMENTS: usize = 30;
/// The most a request to a SuperSpeed disk over Bulk-Only has: 512 KiB in
/// pages, which one transfer of the controller holds.
const SUPER_SEGMENTS: usize = radix_xhci::MAX_TD_BUFFERS;
/// The most a request over UAS has: what one transfer on a stream holds,
/// at any speed.
const UAS_SEGMENTS: usize = radix_xhci::MAX_STREAM_TD_BUFFERS;
/// How long a read, write or flush may take: Linux's `sd`'s.
const TIMEOUT_MS: u32 = 30_000;
/// The blocks at a disk's end that go one a command (Linux's
/// SD_LAST_BUGGY_SECTORS).
const LAST_BLOCKS: u64 = 8;
/// How long the thread sleeps with nothing to do, at most.
const IDLE_NS: u64 = 1_000_000_000;
/// How long a disk that says it is becoming ready is given before its
/// request goes again.
const READY_NS: u64 = 1_000_000_000;
/// How many times the partition table's reads go.
const TRIES: u32 = 3;
/// How much of a REPORT LUNS answer is read: its header and 63 units.
const REPORT_LENGTH: usize = 512;

/// Where a request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// None: room for one.
    Idle,
    /// Handed to the thread, as the queue's slot.
    Waiting(u8),
    /// The thread runs it.
    Running(u8),
    /// It ended so.
    Done(u8, Outcome),
}

/// A request, between the queue and the stick's thread.
struct Job {
    stage: Stage,
    op: Op,
    /// Its first sector, of 512 bytes.
    sector: u64,
    /// Its buffers (physical address, length).
    frames: Vec<(u64, u32), Kmalloc>,
}

/// A logical unit's requests, as many as it may have in flight.
struct Jobs {
    jobs: Vec<Job, Kmalloc>,
    /// Why the last request in each of the queue's slots failed or went
    /// again, for the log.
    why: Vec<Option<&'static str>, Kmalloc>,
    /// The stick is gone: requests fail at once.
    gone: bool,
}

/// What a logical unit's disk shares with its stick's thread.
struct Shared {
    jobs: SpinLock<Jobs>,
    /// The stick's thread, which a request wakes.
    thread: ThreadRef,
    /// The stick's link, which an abort goes through.
    link: LinkRef,
}

/// A logical unit as the queue drives it.
pub struct UsbLun {
    shared: Arc<Shared, Kmalloc>,
}

impl Controller for UsbLun {
    fn room(&self, _op: Op, _segments: usize) -> bool {
        // A gone disk takes every request, to fail it.
        let jobs = self.shared.jobs.lock_irq();
        jobs.gone || jobs.jobs.iter().any(|job| job.stage == Stage::Idle)
    }

    fn submit(&mut self, slot: u8, request: &Request<'_>, _more: bool) -> Result<(), Error> {
        {
            let mut jobs = self.shared.jobs.lock_irq();
            if jobs.gone {
                return Err(Error::Io);
            }
            let Some(job) = jobs.jobs.iter_mut().find(|job| job.stage == Stage::Idle) else {
                return Err(Error::OutOfRange);
            };
            if request.data.len() > job.frames.capacity() {
                return Err(Error::OutOfRange);
            }
            job.frames.clear();
            for &frame in request.data {
                let _ = job.frames.push_within_capacity(frame);
            }
            job.op = request.op;
            job.sector = request.sector;
            job.stage = Stage::Waiting(slot);
        }
        sched::wake(&self.shared.thread);
        Ok(())
    }

    fn complete(&mut self) -> Option<(u8, Outcome)> {
        let mut jobs = self.shared.jobs.lock_irq();
        let job = jobs.jobs.iter_mut().find(|job| matches!(job.stage, Stage::Done(..)))?;
        let Stage::Done(slot, outcome) = job.stage else { return None };
        job.stage = Stage::Idle;
        Some((slot, outcome))
    }

    fn why(&self, slot: u8) -> Option<&'static str> {
        self.shared.jobs.lock_irq().why.get(usize::from(slot)).copied().flatten()
    }

    fn reset(lanes: &mut [Part<UsbLun>]) -> bool {
        // The thread stops the transfer it waits for, which resets a
        // Bulk-Only interface, or resets a UAS device; the requests go
        // again.
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
    /// Over UAS: it said it had stopped, and a START STOP UNIT is to go
    /// to it before its requests do.
    start: bool,
    /// The START STOP UNIT is in flight.
    starting: bool,
    /// It said it is becoming ready: its requests wait until then.
    not_before: u64,
    /// A request of its ended since its queue last heard.
    ended: bool,
}

impl Lun {
    /// Takes the first request waiting, if one is: where it is, its slot,
    /// what it does, its first sector and its length in bytes, its
    /// buffers into `frames` if given.
    fn take(
        &self,
        frames: Option<&mut Vec<(u64, u32), Kmalloc>>,
    ) -> Option<(usize, u8, Op, u64, u64)> {
        let mut jobs = self.shared.jobs.lock_irq();
        let at = jobs.jobs.iter().position(|job| matches!(job.stage, Stage::Waiting(_)))?;
        let job = &mut jobs.jobs[at];
        let Stage::Waiting(slot) = job.stage else { return None };
        job.stage = Stage::Running(slot);
        if let Some(frames) = frames {
            frames.clear();
            for &frame in &job.frames {
                // As much room as the request's.
                let _ = frames.push_within_capacity(frame);
            }
        }
        let bytes = job.frames.iter().map(|&(_, len)| u64::from(len)).sum();
        Some((at, slot, job.op, job.sector, bytes))
    }

    /// The buffers of request `at` that hold its bytes `from` to `from +
    /// len`, into `out`.
    fn part(&self, at: usize, from: u64, len: u64, out: &mut Vec<(u64, u32), Kmalloc>) {
        let jobs = self.shared.jobs.lock_irq();
        match jobs.jobs.get(at) {
            Some(job) => slice(&job.frames, from, len, out),
            None => out.clear(),
        }
    }

    /// Hands back request `at`, which ended with `outcome`; its queue
    /// hears of it at [`Lun::collect`].
    fn finish(&mut self, at: usize, outcome: Outcome, why: Option<&'static str>) {
        {
            let mut jobs = self.shared.jobs.lock_irq();
            let Jobs { jobs, why: whys, .. } = &mut *jobs;
            let Some(job) = jobs.get_mut(at) else { return };
            let Stage::Running(slot) = job.stage else { return };
            job.stage = Stage::Done(slot, outcome);
            if let Some(entry) = whys.get_mut(usize::from(slot)) {
                *entry = why;
            }
        }
        self.ended = true;
    }

    /// Tells its queue of the requests that ended.
    fn collect(&mut self) {
        if core::mem::take(&mut self.ended) {
            self.disk.collect(0);
        }
    }

    /// The stick is gone: fails the requests it has, and every one after.
    fn gone(&self) {
        {
            let mut jobs = self.shared.jobs.lock_irq();
            let Jobs { jobs, why, gone } = &mut *jobs;
            *gone = true;
            for job in jobs.iter_mut() {
                if let Stage::Waiting(slot) | Stage::Running(slot) = job.stage {
                    job.stage = Stage::Done(slot, Outcome::Failed);
                    if let Some(entry) = why.get_mut(usize::from(slot)) {
                        *entry = Some("its USB device was unplugged");
                    }
                }
            }
        }
        self.disk.collect(0);
    }
}

/// What a request waits for after a command of it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    Nothing,
    /// The disk is becoming ready: a second.
    Ready,
    /// It had stopped: a START STOP UNIT.
    Start,
}

/// How a command that ended with `reply` ends its request, as Linux's SCSI
/// layer has it for a USB disk, and what its next try waits for: a
/// `flush` the disk does not take did not need doing.
fn judge(
    reply: Result<Reply, TransportError>,
    flush: bool,
) -> (Outcome, Option<&'static str>, Then) {
    let sense = match reply {
        Ok(Reply::Good { residue: 0 }) => return (Outcome::Done, None, Then::Nothing),
        Ok(Reply::Good { .. }) => {
            return (Outcome::Retry, Some("it moved less than asked"), Then::Nothing);
        }
        Ok(Reply::Failed(Some(sense))) => sense,
        Ok(Reply::Failed(None)) => {
            let why = "it failed the command, and did not say why";
            return (Outcome::Retry, Some(why), Then::Nothing);
        }
        Err(TransportError::Gone) => {
            return (Outcome::Failed, Some("its USB device was unplugged"), Then::Nothing);
        }
        Err(TransportError::Failed(why)) => return (Outcome::Retry, Some(why), Then::Nothing),
    };
    match sense.meaning() {
        Meaning::Fine => (Outcome::Done, None, Then::Nothing),
        Meaning::Unsupported if flush => (Outcome::Done, None, Then::Nothing),
        Meaning::Attention | Meaning::Aborted => {
            (Outcome::Retry, Some(sense.key_name()), Then::Nothing)
        }
        Meaning::BecomingReady => (Outcome::Retry, Some("it is becoming ready"), Then::Ready),
        Meaning::NeedsStart => (Outcome::Retry, Some("it had stopped"), Then::Start),
        // USB bridges say so of errors they recover from (Linux's
        // retry_hwerror).
        Meaning::Error if sense.key == key::HARDWARE_ERROR => {
            (Outcome::Retry, Some(sense.key_name()), Then::Nothing)
        }
        Meaning::NoMedium => (Outcome::Failed, Some("its medium was taken out"), Then::Nothing),
        Meaning::WriteProtected => {
            (Outcome::Failed, Some("its medium is write-protected"), Then::Nothing)
        }
        _ => (Outcome::Failed, Some(sense.key_name()), Then::Nothing),
    }
}

/// What the thread works with: a request's buffers, and a part of them.
struct Work {
    frames: Vec<(u64, u32), Kmalloc>,
    part: Vec<(u64, u32), Kmalloc>,
}

/// What a UAS command carries.
#[derive(Clone, Copy, Debug)]
enum Carried {
    /// A part of request `job` of the disk at `lun`, which reads (or
    /// writes) `blocks` blocks from block `first`: its blocks `done` to
    /// `done + count`.
    Part { lun: usize, job: usize, into_host: bool, first: u64, blocks: u64, done: u64, count: u64 },
    /// Request `job` of the disk at `lun`, a flush.
    Flush { lun: usize, job: usize },
    /// A START STOP UNIT to the disk at `lun`, which had stopped.
    Start { lun: usize },
}

/// A UAS stick's commands, as its thread runs them.
struct Queued<'a> {
    uas: Box<Uas, Kmalloc>,
    pipes: UasPipes<'a>,
    /// What each tag carries, by tag less one.
    carried: Vec<Option<Carried>, Kmalloc>,
    /// Where a status lands.
    status: [u8; STATUS_LENGTH],
    /// The buffers of the part of a request that a command carries.
    part: Vec<(u64, u32), Kmalloc>,
}

impl<'a> Queued<'a> {
    /// The commands of the UAS stick `link`, whose status and data pipes
    /// have `streams` streams; `None` when memory runs out. Kept out of
    /// its callers' frames, which the engine would not fit in with them.
    #[inline(never)]
    fn new(link: &'a Link, streams: Option<usize>) -> Option<Box<Queued<'a>, Kmalloc>> {
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let uas = Box::try_new_in(Uas::new(streams), heap).ok()?;
        let pipes = UasPipes::new(link)?;
        let (mut carried, mut part) = (Vec::new_in(heap), Vec::new_in(heap));
        carried.try_reserve_exact(MAX_TAGS).ok()?;
        part.try_reserve_exact(UAS_SEGMENTS).ok()?;
        while carried.push_within_capacity(None).is_ok() {}
        let queued = Queued { uas, pipes, carried, status: [0; STATUS_LENGTH], part };
        Box::try_new_in(queued, heap).ok()
    }
}

/// How a stick's commands go.
enum Wire<'a> {
    Bot(Bot<StickPipes<'a>>),
    Uas(Box<Queued<'a>, Kmalloc>),
}

impl Wire<'_> {
    /// Runs `f` with the stick's commands one at a time: over UAS, while
    /// no other is in flight.
    fn one<R>(&mut self, f: impl FnOnce(&mut dyn Transport) -> R) -> R {
        match self {
            Wire::Bot(bot) => f(bot),
            Wire::Uas(queued) => {
                let Queued { uas, pipes, .. } = &mut **queued;
                f(&mut OneAtATime { uas, pipes })
            }
        }
    }
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
    wire: Wire<'a>,
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
        let mut frames = Vec::new_in(heap);
        let mut part = Vec::new_in(heap);
        let mut luns = Vec::new_in(heap);
        let reserved = frames.try_reserve_exact(SUPER_SEGMENTS).is_ok()
            && part.try_reserve_exact(SUPER_SEGMENTS).is_ok();
        let wire = match link.protocol {
            Protocol::Bot { .. } => {
                StickPipes::new(link).map(|p| Wire::Bot(Bot::new(p, link.interface)))
            }
            Protocol::Uas { streams, .. } => Queued::new(link, streams).map(Wire::Uas),
        };
        let (Some(mut wire), true) = (wire, reserved) else {
            warn!("Disk: USB {}: out of memory for its disks", link.place);
            return None;
        };
        // A stick unplugged already finds none.
        let mut numbers = [0u8; MAX_LUNS];
        let count = logical_units(&mut wire, &mut numbers)?;
        // Over UAS the tags are shared among the units, each its part.
        let jobs = match &wire {
            Wire::Bot(_) => 1,
            Wire::Uas(queued) => (queued.uas.depth() / count).max(1),
        };
        let mut wait = |ms: u32| sched::sleep_ns(u64::from(ms) * 1_000_000);
        for &number in &numbers[..count] {
            let mut lun = TextBuf::<16>::new();
            if count > 1 || number > 0 {
                let _ = core::fmt::Write::write_fmt(&mut lun, format_args!(" LUN {number}"));
            }
            let found = match wire.one(|transport| probe(transport, number, &mut wait)) {
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
            match add(link, &mut wire, (number, jobs), found, &lun) {
                Ok(added) => {
                    let _ = luns.push_within_capacity(added);
                }
                Err(why) => warn!("Disk: USB {}{lun}: {why}", link.place),
            }
        }
        if luns.is_empty() {
            return None;
        }
        Some(Stick { link, wire, luns, work: Work { frames, part } })
    }

    /// Serves the disks' requests until the stick is unplugged.
    fn serve(&mut self) {
        match self.wire {
            Wire::Bot(_) => self.serve_one_at_a_time(),
            Wire::Uas(_) => self.serve_queued(),
        }
    }

    /// Serves the requests one at a time, over Bulk-Only.
    fn serve_one_at_a_time(&mut self) {
        while !self.link.is_gone() {
            let mut served = false;
            for at in 0..self.luns.len() {
                let Some((job, _, op, sector, _)) = self.luns[at].take(Some(&mut self.work.frames))
                else {
                    continue;
                };
                served = true;
                self.link.forget_abort();
                let (outcome, why) = self.execute(at, op, sector);
                self.luns[at].finish(job, outcome, why);
                self.luns[at].collect();
            }
            if !served {
                sched::block_until(sched::now_ns().saturating_add(IDLE_NS), "usb-storage");
            }
        }
    }

    /// Carries out `op` from `sector` with the buffers in the work's, on
    /// the disk at `at`, one command at a time; returns how it ended, and
    /// why if it did not work.
    fn execute(&mut self, at: usize, op: Op, sector: u64) -> (Outcome, Option<&'static str>) {
        let Lun { number, found, .. } = self.luns[at];
        let into_host = match op {
            Op::Read => true,
            Op::Write | Op::WriteThrough => false,
            Op::Flush => {
                let cdb = Cdb::synchronize_cache();
                let reply = self.wire.one(|t| t.command(number, &cdb, Data::None, TIMEOUT_MS));
                return self.after(number, judge(reply, true));
            }
            Op::Discard(_) => return (Outcome::Failed, Some("it does not discard")),
        };
        let block = u64::from(found.capacity.block_length);
        let unit = block / SECTOR;
        let total: u64 = self.work.frames.iter().map(|&(_, len)| u64::from(len)).sum();
        let (first, blocks) = (sector / unit, total / block);
        let mut done = 0;
        while done < blocks {
            let (start, count) = part_of(&found, first + done, blocks - done);
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
            let reply = self.wire.one(|t| t.command(number, &cdb, data, TIMEOUT_MS));
            let (outcome, why) = self.after(number, judge(reply, false));
            if outcome != Outcome::Done {
                return (outcome, why);
            }
            done += count;
        }
        (Outcome::Done, None)
    }

    /// Does what a request of logical unit `number` waits for before it
    /// goes again, one command at a time: a second for a disk becoming
    /// ready, a START STOP UNIT for one that stopped.
    fn after(
        &mut self,
        number: u8,
        (outcome, why, then): (Outcome, Option<&'static str>, Then),
    ) -> (Outcome, Option<&'static str>) {
        match then {
            Then::Nothing => {}
            Then::Ready => sched::sleep_ns(READY_NS),
            Then::Start => {
                let start = Cdb::start_stop_unit(true, false);
                let _ = self.wire.one(|t| t.command(number, &start, Data::None, TIMEOUT_MS));
            }
        }
        (outcome, why)
    }

    /// Serves the requests many at once, over UAS.
    fn serve_queued(&mut self) {
        let Stick { link, wire, luns, .. } = self;
        let Wire::Uas(queued) = wire else { return };
        let queued = &mut **queued;
        loop {
            if link.is_gone() {
                queued.uas.gone();
                queued.finish_all(luns);
                return;
            }
            let mut busy = false;
            if link.take_abort() {
                // A request went unanswered 30 s: the device is reset.
                queued.uas.timed_out();
            }
            loop {
                if queued.uas.is_gone() {
                    break;
                }
                let Some((pipe, n, result)) = queued.pipes.take_ended(&mut queued.status) else {
                    break;
                };
                queued.uas.ended(pipe, n, result, &queued.status);
                busy = true;
            }
            if let Some(why) = queued.uas.broken() {
                warn!("Disk: USB {}: {why}; it is reset", link.place);
                let result = queued.pipes.reset();
                queued.uas.reset(result);
                busy = true;
            }
            busy |= queued.finish_all(luns);
            busy |= queued.begin(luns);
            busy |= queued.pipes.act(&mut queued.uas);
            for lun in luns.iter_mut() {
                lun.collect();
            }
            if !busy {
                sched::block_until(sched::now_ns().saturating_add(IDLE_NS), "usb-storage");
            }
        }
    }
}

impl Queued<'_> {
    /// Hands back the commands that finished; says whether any did.
    fn finish_all(&mut self, luns: &mut [Lun]) -> bool {
        let mut any = false;
        while let Some((tag, reply)) = self.uas.next_done() {
            any = true;
            let Some(carried) = self.carried[usize::from(tag) - 1].take() else { continue };
            self.finished(luns, carried, reply);
        }
        any
    }

    /// A command that carried `carried` ended with `reply`.
    fn finished(
        &mut self,
        luns: &mut [Lun],
        carried: Carried,
        reply: Result<Reply, TransportError>,
    ) {
        let (at, job, flush) = match carried {
            Carried::Start { lun } => {
                luns[lun].starting = false;
                return;
            }
            Carried::Flush { lun, job } => (lun, job, true),
            Carried::Part { lun, job, .. } => (lun, job, false),
        };
        let (outcome, why, then) = judge(reply, flush);
        if let (Outcome::Done, Carried::Part { into_host, first, blocks, done, count, .. }) =
            (outcome, carried)
            && done + count < blocks
        {
            // The next part goes under the tag this one freed.
            self.part(luns, (at, job), (into_host, first, blocks), done + count);
            return;
        }
        let lun = &mut luns[at];
        match then {
            Then::Nothing => {}
            Then::Ready => lun.not_before = sched::now_ns().saturating_add(READY_NS),
            Then::Start => lun.start = true,
        }
        lun.finish(job, outcome, why);
    }

    /// Starts the requests waiting, as the free tags let them, and the
    /// START STOP UNITs disks wait for; says whether any started.
    fn begin(&mut self, luns: &mut [Lun]) -> bool {
        let now = sched::now_ns();
        let mut any = false;
        for at in 0..luns.len() {
            if luns[at].start
                && !luns[at].starting
                && let Some(tag) = self.uas.free_tag()
            {
                let start = Cdb::start_stop_unit(true, false);
                if self.command(tag, luns[at].number, &start, None, Carried::Start { lun: at }) {
                    (luns[at].start, luns[at].starting) = (false, true);
                    any = true;
                }
            }
            if luns[at].start || luns[at].starting || now < luns[at].not_before {
                continue;
            }
            while let Some(tag) = self.uas.free_tag() {
                let Some((job, _, op, sector, bytes)) = luns[at].take(None) else { break };
                any = true;
                let found = luns[at].found;
                let block = u64::from(found.capacity.block_length);
                let first = sector / (block / SECTOR);
                let into_host = match op {
                    Op::Read => true,
                    Op::Write | Op::WriteThrough => false,
                    Op::Flush => {
                        let cdb = Cdb::synchronize_cache();
                        let carried = Carried::Flush { lun: at, job };
                        if !self.command(tag, luns[at].number, &cdb, None, carried) {
                            luns[at].finish(job, Outcome::Retry, Some("its device is being reset"));
                        }
                        continue;
                    }
                    Op::Discard(_) => {
                        luns[at].finish(job, Outcome::Failed, Some("it does not discard"));
                        continue;
                    }
                };
                let blocks = bytes / block;
                if blocks == 0 {
                    luns[at].finish(job, Outcome::Done, None);
                    continue;
                }
                self.part(luns, (at, job), (into_host, first, blocks), 0);
            }
        }
        any
    }

    /// Starts the part of request `job` of the disk at `lun` from its
    /// block `done`, of a request that reads (or writes) `blocks` blocks
    /// from block `first`.
    fn part(
        &mut self,
        luns: &mut [Lun],
        (at, job): (usize, usize),
        (into_host, first, blocks): (bool, u64, u64),
        done: u64,
    ) {
        let found = luns[at].found;
        let block = u64::from(found.capacity.block_length);
        let (start, count) = part_of(&found, first + done, blocks - done);
        let cdb = if into_host {
            Cdb::read(start, count as u32, found.sixteen)
        } else {
            Cdb::write(start, count as u32, found.sixteen)
        };
        let Some(cdb) = cdb else {
            let why = "its blocks are past what a command reaches";
            luns[at].finish(job, Outcome::Failed, Some(why));
            return;
        };
        let Some(tag) = self.uas.free_tag() else {
            luns[at].finish(job, Outcome::Retry, Some("its device is being reset"));
            return;
        };
        luns[at].part(job, done * block, count * block, &mut self.part);
        self.pipes.set_data(tag, &self.part);
        let data = Some((into_host, (count * block) as u32));
        let carried = Carried::Part { lun: at, job, into_host, first, blocks, done, count };
        if !self.command(tag, luns[at].number, &cdb, data, carried) {
            luns[at].finish(job, Outcome::Retry, Some("its device is being reset"));
        }
    }

    /// Starts `cdb` to logical unit `number` under `tag`, with data of
    /// `data`'s direction and length in the buffers given the tag, for
    /// `carried`. False if it could not start: the device needs a reset.
    fn command(
        &mut self,
        tag: u16,
        number: u8,
        cdb: &Cdb,
        data: Option<(bool, u32)>,
        carried: Carried,
    ) -> bool {
        let Some(iu) = self.uas.start(tag, number, cdb, data) else { return false };
        self.pipes.place_iu(tag, &iu);
        self.carried[usize::from(tag) - 1] = Some(carried);
        true
    }
}

/// The blocks of a read or write of `count` blocks from block `start` of
/// the disk `found` that go in its next command: all of them, but for the
/// last eight blocks, which go one a command.
fn part_of(found: &Found, start: u64, count: u64) -> (u64, u64) {
    let last = found.capacity.blocks.saturating_sub(LAST_BLOCKS);
    if start + count <= last {
        (start, count)
    } else if start < last {
        (start, last - start)
    } else {
        (start, 1)
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

/// The logical units behind the stick, into `numbers`: Get Max LUN's over
/// Bulk-Only, REPORT LUNS's over UAS (logical unit 0 alone if it fails,
/// as Linux's SCSI scan then takes it). Returns how many; `None` if the
/// stick is gone.
#[inline(never)]
fn logical_units(wire: &mut Wire<'_>, numbers: &mut [u8; MAX_LUNS]) -> Option<usize> {
    let max_lun = match wire {
        Wire::Bot(bot) => bot.max_lun().ok()?,
        Wire::Uas(_) => {
            let mut bytes = [0u8; REPORT_LENGTH];
            let cdb = Cdb::report_luns(REPORT_LENGTH as u32);
            let reply = wire.one(|t| t.command(0, &cdb, Data::In(&mut bytes), TIMEOUT_MS));
            let mut count = 0;
            match reply {
                Err(TransportError::Gone) => return None,
                Ok(Reply::Good { residue }) => {
                    let came = REPORT_LENGTH.saturating_sub(residue as usize);
                    for lun in reported_luns(&bytes[..came]) {
                        if count < MAX_LUNS && !numbers[..count].contains(&lun) {
                            numbers[count] = lun;
                            count += 1;
                        }
                    }
                }
                _ => {}
            }
            if count == 0 {
                numbers[0] = 0;
                count = 1;
            }
            return Some(count);
        }
    };
    for (number, slot) in numbers.iter_mut().enumerate().take(usize::from(max_lun) + 1) {
        *slot = number as u8;
    }
    Some(usize::from(max_lun) + 1)
}

/// Gives the disk at logical unit `number`, as `found` says it is, a name
/// and its queue of `jobs` requests in flight, and registers it and its
/// partitions.
#[inline(never)]
fn add(
    link: &LinkRef,
    wire: &mut Wire<'_>,
    (number, jobs): (u8, usize),
    found: Found,
    lun: &TextBuf<16>,
) -> Result<Lun, &'static str> {
    // The SATA disks found at boot are named first, then the sticks'
    // in the order they were found.
    super::wait_for_scan();
    crate::usb::storage::wait_for_turn(link);
    let (letter, name) = super::take_sd_name().ok_or("too many SATA and USB disks")?;
    let (added, device) = add_named(link, (number, jobs), found, (name, letter), lun)
        .inspect_err(|_| super::give_back_sd_letter(letter))?;
    read_partitions(wire, &added, &device);
    Ok(added)
}

/// The requests of a logical unit, `count` of them in flight of up to
/// `segments` buffers each.
fn jobs(count: usize, segments: usize) -> Option<Jobs> {
    let heap = HEAP.with(Flags::MAY_SLEEP);
    let mut jobs = Vec::new_in(heap);
    jobs.try_reserve_exact(count).ok()?;
    for _ in 0..count {
        let mut frames = Vec::new_in(heap);
        frames.try_reserve_exact(segments).ok()?;
        let job = Job { stage: Stage::Idle, op: Op::Read, sector: 0, frames };
        let _ = jobs.push_within_capacity(job);
    }
    let mut why = Vec::new_in(heap);
    why.try_reserve_exact(SLOTS).ok()?;
    while why.push_within_capacity(None).is_ok() {}
    Some(Jobs { jobs, why, gone: false })
}

/// [`add`]'s part with the disk named `name`, after `letter`.
fn add_named(
    link: &LinkRef,
    (number, count): (u8, usize),
    found: Found,
    (name, letter): (Name, u32),
    lun: &TextBuf<16>,
) -> Result<(Lun, DeviceRef), &'static str> {
    let heap = HEAP.with(Flags::MAY_SLEEP);
    let capacity = found.capacity;
    let unit = u64::from(capacity.block_length) / SECTOR;
    let max_segments = match link.protocol {
        Protocol::Uas { .. } => UAS_SEGMENTS,
        Protocol::Bot { .. } if link.speed.is_super() => SUPER_SEGMENTS,
        Protocol::Bot { .. } => USB2_SEGMENTS,
    };
    let info = Info {
        sectors: capacity.blocks.saturating_mul(unit),
        logical_block: capacity.block_length,
        block_size: capacity.physical_block(),
        read_only: found.read_only,
        max_segments,
        discard_sectors: 0,
        // A disk that does not say spins, as Linux takes it.
        rotational: true,
    };
    let jobs = jobs(count, max_segments).ok_or("out of memory")?;
    let thread = sched::current().ok_or("it has no thread")?;
    let shared = Shared { jobs: SpinLock::new(jobs), thread, link: link.clone() };
    let shared = Arc::try_new_in(shared, heap).map_err(|_| "out of memory")?;
    let part = queue::part(UsbLun { shared: shared.clone() }).ok_or("out of memory")?;
    let disk =
        Disk::new(core::iter::once(part), info, found.write_cache, false, true, name.clone())
            .ok_or("out of memory")?;
    let disk = Arc::try_new_in(disk, heap).map_err(|_| "out of memory")?;
    let inquiry = found.inquiry;
    let how = match link.protocol {
        Protocol::Uas { .. } => ", USB Attached SCSI",
        Protocol::Bot { .. } => "",
    };
    info!(
        "Disk: {name} is a USB disk of {} on {}{lun}: {} {} {}, {} bytes a block{}{}{}{}{how}",
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
    let volume = Volume::new(disk.clone(), 0, info);
    let device: DeviceRef = Arc::try_new_in(volume, heap).map_err(|_| "out of memory")?;
    let ident = Some(ident(link, &found));
    register(Entry { name: name.clone(), device: device.clone(), place: None, ident });
    let lun = Lun {
        number,
        name,
        letter,
        found,
        shared,
        disk,
        start: false,
        starting: false,
        not_before: 0,
        ended: false,
    };
    Ok((lun, device))
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
/// `device`, and registers its partitions: through `wire` itself, since
/// the thread that would serve the disk's queue is this one.
fn read_partitions(wire: &mut Wire<'_>, lun: &Lun, device: &DeviceRef) {
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
            let data = Data::In(&mut buffer);
            match wire.one(|t| t.command(lun.number, &cdb, data, TIMEOUT_MS)) {
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
