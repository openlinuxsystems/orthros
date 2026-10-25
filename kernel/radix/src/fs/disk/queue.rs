// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The queue a disk's requests go through, whatever drives the disk.
//!
//! A request blocks its thread until the device is done; many threads have
//! requests in flight at once, up to what the device holds. A request
//! takes one of [`SLOTS`] slots, whose state is set when the driver hands
//! the request back ([`Disk::collect`], in the interrupt handler, or in the
//! waiting threads of a device with no interrupt); the thread then frees
//! the slot. A [`Controller`] is the driver underneath: it numbers what it
//! has in flight its own way (descriptor chains, command slots) and says
//! which slot each finished request was.
//!
//! Requests take room by their processes' I/O class and level
//! (radix-process(2)'s `io_priority`, ionice(1)): one takes room only
//! while no more urgent one waits for it, the real-time class before
//! best effort, and level 0 before 7 in each; the idle class takes room
//! only while the disk has nothing else to do. A request the device takes
//! only with nothing else in flight (an AHCI disk's flush or TRIM) holds
//! back those of its rank and below that come after it, so that the
//! device empties for it. Once in flight, every request is the device's
//! alike.
//!
//! A transfer of more segments than the device takes in one
//! ([`BlockDevice::read_frames`], into memory a program lends) goes as
//! several requests, all in flight at once, and the device hears of them
//! together: a controller may hold back the doorbell of a request that
//! has another right behind it ([`Controller::submit`]'s `more`), which
//! lets QEMU's virtio disk merge them into one read or write of its file.
//!
//! A request the device fails ends in an I/O error. One it hands back to
//! go again (another request's error stopped it, or the link was reset)
//! goes again, up to [`TRIES`] times in all. A controller an error stopped
//! is restarted by one of the threads waiting for it: that thread takes
//! the controller out of the queue's lock, so that it can sleep while the
//! controller recovers, and puts it back; the interrupt handler leaves the
//! disk alone meanwhile. A request unanswered for [`TIMEOUT_NS`] has the
//! controller reset the same way, if it can be (an AHCI port resets its
//! link); a virtio device cannot, and its requests wait on.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use radix_abi::process::{IO_LEVELS, IoClass};
use radix_block::{BlockDevice, Error, Info, Segment, SegmentMut};
use radix_hal::Timer;
use radix_klog::warn;
use radix_sync::SpinLock;

use super::{Name, READ_SECTORS, READS, WRITES, WRITTEN_SECTORS};
use crate::Arch;
use crate::sched::{self, wait};
use crate::user::posix::priority;

/// Requests in flight or waiting to be collected, per disk.
pub const SLOTS: usize = 64;
/// The most requests one transfer has in flight at once: a mebibyte of
/// pages, in requests of [`MAX_SEGMENTS`].
pub const PARTS: usize = 16;
/// The most segments one request takes: a page cache's requests take one
/// page each, so this is plenty.
pub const MAX_SEGMENTS: usize = 16;
/// How many times a request goes to the device before it fails, when the
/// device keeps handing it back to go again.
pub const TRIES: u32 = 5;
/// How long a request may go unanswered before the controller is reset:
/// the time Linux gives SCSI and ATA disks.
pub const TIMEOUT_NS: u64 = 30_000_000_000;

const FREE: u8 = 0;
/// Taken by a request that is not in flight: between its tries.
const HELD: u8 = 1;
const PENDING: u8 = 2;
const DONE: u8 = 3;
const FAILED: u8 = 4;
const RETRY: u8 = 5;

/// How many ranks of urgency requests have (`posix::priority::io_rank`):
/// the real-time class's levels, the best-effort class's, and idle.
const RANKS: usize = 2 * IO_LEVELS as usize + 1;
/// The idle class's rank, the least urgent.
const IDLE: usize = RANKS - 1;

/// What a request does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Reads sectors into the buffers.
    Read,
    /// Writes the buffers to sectors.
    Write,
    /// Writes the buffers to sectors through the device's cache: durable
    /// when done. Only for a controller that says it does
    /// ([`Disk::new`]'s `fua`).
    WriteThrough,
    /// Makes every completed write durable.
    Flush,
    /// Discards this many sectors, at most [`Info::discard_sectors`].
    Discard(u64),
}

/// How a request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It worked.
    Done,
    /// The device failed it.
    Failed,
    /// It goes again: an error that was not its own stopped it.
    Retry,
}

/// What the queue needs of a disk's driver. Every method but
/// [`Controller::recover`] and [`Controller::reset`] runs with the
/// queue's lock held and interrupts off, and must not wait.
pub trait Controller: Send {
    /// Whether `op` with `segments` buffers finds room now.
    fn room(&self, op: Op, segments: usize) -> bool;

    /// Whether the device takes `op` only with nothing else in flight.
    fn alone(&self, _op: Op) -> bool {
        false
    }

    /// Starts a request, `op` on the sectors from `sector` with the
    /// `data` buffers (physical address, length), as the queue's `slot`.
    /// With `more`, another request follows at once: the controller may
    /// wait to tell the device until one comes without it, or until
    /// [`Controller::kick`].
    fn submit(
        &mut self,
        slot: u8,
        op: Op,
        sector: u64,
        data: &[(u64, u32)],
        more: bool,
    ) -> Result<(), Error>;

    /// Tells the device of the requests [`Controller::submit`] held back.
    fn kick(&mut self) {}

    /// The next finished request: its slot and how it ended.
    fn complete(&mut self) -> Option<(u8, Outcome)>;

    /// Whether an error stopped the controller, which then takes nothing
    /// until [`Controller::recover`] has run.
    fn stopped(&self) -> bool {
        false
    }

    /// Restarts a stopped controller; what was in flight comes out of
    /// [`Controller::complete`] afterwards. It may sleep: it runs without
    /// the queue's lock.
    fn recover(&mut self) {}

    /// Resets the controller after a request went unanswered too long;
    /// what was in flight comes out of [`Controller::complete`]
    /// afterwards. False if it cannot be reset. It may sleep.
    fn reset(&mut self) -> bool {
        false
    }
}

/// Takes a waiter off [`sched::DISK_WAITERS`] when it goes.
struct Counted;

impl Drop for Counted {
    fn drop(&mut self) {
        sched::DISK_WAITERS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Takes a request off a count of those waiting when it goes.
struct Waiting<'a>(&'a AtomicU32);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How many sectors `data` (physical address, length) covers.
fn sectors(data: &[(u64, u32)]) -> u64 {
    data.iter().map(|&(_, len)| u64::from(len)).sum::<u64>() / radix_block::SECTOR
}

/// How urgently the running thread's requests are served: by its
/// process's I/O class and level (radix-process(2)'s `io_priority`), or
/// for the kernel's own threads, the middle of best effort.
fn running_rank() -> usize {
    let class = sched::running()
        .and_then(|me| me.user().map(|user| user.process.io_priority()))
        .unwrap_or((IoClass::None.raw(), 0));
    (priority::io_rank(class.0, class.1) as usize).min(IDLE)
}

/// Waits `us` microseconds, asleep: for a controller that needs time
/// while it recovers or starts. A thread asked to stop wakes early from
/// its sleeps, so it waits out the rest without sleeping.
pub fn delay(us: u32) {
    let deadline = sched::now_ns() + u64::from(us) * 1_000;
    sched::sleep_until(deadline);
    while sched::now_ns() < deadline {
        Arch::delay_us(10);
    }
}

/// A disk: the device's driver, and the requests waiting for it.
pub struct Disk<C> {
    /// The driver: `None` while a thread recovers or resets it.
    queue: SpinLock<Option<C>>,
    /// Each request slot's state.
    requests: [AtomicU8; SLOTS],
    /// Moves on every completion and every slot freed: what threads
    /// waiting for room wait on.
    events: AtomicU64,
    /// How many requests wait for room, by rank.
    waiting: [AtomicU32; RANKS],
    /// How many of them the device takes only alone, and wait for it to
    /// empty, by rank.
    lone: [AtomicU32; RANKS],
    /// An error stopped the controller: a waiting thread is to recover it.
    attention: AtomicBool,
    /// A thread has the controller out to recover or reset it.
    recovering: AtomicBool,
    info: Info,
    /// Whether it has a write cache for flushes to empty.
    write_cache: bool,
    /// Whether it writes through its cache itself ([`Op::WriteThrough`]).
    fua: bool,
    /// Whether its completions raise an interrupt; otherwise waiters poll.
    interrupt: bool,
    /// Its name, for the log.
    name: Name,
}

impl<C: Controller> Disk<C> {
    /// The disk `name` that `controller` drives: what it is, whether it
    /// has a write cache and writes through it, and whether its
    /// completions raise an interrupt that calls [`Disk::collect`].
    pub fn new(
        controller: C,
        info: Info,
        write_cache: bool,
        fua: bool,
        interrupt: bool,
        name: Name,
    ) -> Disk<C> {
        Disk {
            queue: SpinLock::new(Some(controller)),
            requests: [const { AtomicU8::new(FREE) }; SLOTS],
            events: AtomicU64::new(0),
            waiting: [const { AtomicU32::new(0) }; RANKS],
            lone: [const { AtomicU32::new(0) }; RANKS],
            attention: AtomicBool::new(false),
            recovering: AtomicBool::new(false),
            info: Info { max_segments: info.max_segments.min(MAX_SEGMENTS), ..info },
            write_cache,
            fua,
            interrupt,
            name,
        }
    }

    fn key(&self) -> usize {
        core::ptr::from_ref::<Disk<C>>(self) as usize
    }

    /// Wakes every thread waiting for room or for its request.
    fn wake(&self) {
        self.events.fetch_add(1, Ordering::Release);
        wait::wake_all(self.key());
    }

    /// Takes the requests the driver has handed back, notes an error that
    /// stopped it, and wakes the threads waiting. Runs in the interrupt
    /// handler, and in waiting threads when the device has no interrupt;
    /// leaves a controller being recovered alone.
    pub fn collect(&self) {
        let mut any = false;
        {
            let mut queue = self.queue.lock_irq();
            let Some(controller) = queue.as_mut() else { return };
            while let Some((slot, outcome)) = controller.complete() {
                let state = match outcome {
                    Outcome::Done => DONE,
                    Outcome::Failed => FAILED,
                    Outcome::Retry => RETRY,
                };
                if let Some(request) = self.requests.get(usize::from(slot)) {
                    request.store(state, Ordering::Release);
                }
                any = true;
            }
            if controller.stopped() && !self.attention.load(Ordering::Relaxed) {
                self.attention.store(true, Ordering::Release);
                any = true;
            }
        }
        if any {
            self.wake();
        }
    }

    /// Recovers a controller an error stopped or, with `reset`, resets
    /// one that left a request unanswered: with the controller out of the
    /// lock, so that it may sleep. Another thread at it already does it
    /// for both.
    fn attend(&self, reset: bool) {
        if self.recovering.swap(true, Ordering::AcqRel) {
            return;
        }
        self.attention.store(false, Ordering::Release);
        let taken = self.queue.lock_irq().take();
        if let Some(mut controller) = taken {
            if controller.stopped() {
                controller.recover();
            } else if reset && !controller.reset() {
                warn!("Disk: {}: a request has gone unanswered for 30 s", self.name);
            }
            *self.queue.lock_irq() = Some(controller);
        }
        self.recovering.store(false, Ordering::Release);
        self.collect();
        // Those waiting for room find it again, whatever was collected.
        self.wake();
    }

    /// Waits until `condition` holds, an error needs a thread to recover
    /// the controller, or the clock passes `deadline_ns`: asleep until an
    /// interrupt, or polling the device without one. Returns whether
    /// `condition` holds.
    fn wait(&self, deadline_ns: u64, condition: impl Fn() -> bool) -> bool {
        // Counted in the load averages while it waits, as on Linux.
        sched::DISK_WAITERS.fetch_add(1, Ordering::Relaxed);
        let _counted = Counted;
        let woken = || condition() || self.attention.load(Ordering::Acquire);
        if self.interrupt {
            wait::wait_until_or_timeout(self.key(), "disk", deadline_ns, woken);
        } else {
            while !woken() && sched::now_ns() < deadline_ns {
                self.collect();
                if !woken() {
                    sched::yield_now();
                }
            }
        }
        condition()
    }

    /// Whether a request of rank `rank` may take room now: no more urgent
    /// one waits for it, and for the idle class, no other request waits or
    /// is in flight, as Linux's idle class is served only when the disk
    /// is otherwise idle.
    fn may_go(&self, rank: usize) -> bool {
        let others = |range: core::ops::Range<usize>| {
            self.waiting[range].iter().any(|count| count.load(Ordering::Acquire) > 0)
        };
        if rank < IDLE {
            return !others(0..rank);
        }
        !others(0..IDLE) && self.requests.iter().all(|r| r.load(Ordering::Acquire) == FREE)
    }

    /// Whether a request that goes out alone, of rank `rank` or more
    /// urgent, waits for the device to empty: then those of rank `rank`
    /// wait behind it.
    fn held_back(&self, rank: usize) -> bool {
        self.lone[..=rank].iter().any(|count| count.load(Ordering::Acquire) > 0)
    }

    /// Takes room for a request and starts it, in slot `held` (a request
    /// that goes again) or a free one; returns the slot. With `more`,
    /// another request follows at once ([`Controller::submit`]).
    fn issue(
        &self,
        rank: usize,
        held: Option<usize>,
        op: Op,
        sector: u64,
        data: &[(u64, u32)],
        more: bool,
    ) -> Result<usize, Error> {
        self.waiting[rank].fetch_add(1, Ordering::AcqRel);
        let waiting = Waiting(&self.waiting[rank]);
        let mut lone = None;
        let slot = loop {
            let seen = self.events.load(Ordering::Acquire);
            if self.attention.load(Ordering::Acquire) {
                self.attend(false);
                continue;
            }
            if self.may_go(rank) && (lone.is_some() || !self.held_back(rank)) {
                let mut queue = self.queue.lock_irq();
                if let Some(controller) = queue.as_mut()
                    && !controller.stopped()
                {
                    let free = held.or_else(|| {
                        self.requests.iter().position(|r| r.load(Ordering::Acquire) == FREE)
                    });
                    if let Some(slot) = free {
                        if controller.room(op, data.len()) {
                            controller.submit(slot as u8, op, sector, data, more)?;
                            self.requests[slot].store(PENDING, Ordering::Release);
                            break slot;
                        }
                        if lone.is_none() && controller.alone(op) {
                            self.lone[rank].fetch_add(1, Ordering::AcqRel);
                            lone = Some(Waiting(&self.lone[rank]));
                        }
                    }
                }
            }
            // No room, or a more urgent request comes first, or the
            // controller is being recovered: wait for a completion, or a
            // slot freed, once the device knows of what was held back
            // for it.
            self.kick();
            self.wait(u64::MAX, || self.events.load(Ordering::Acquire) != seen);
        };
        drop(lone);
        drop(waiting);
        if self.waiting.iter().any(|count| count.load(Ordering::Acquire) > 0) {
            // Less urgent requests waited for this one: there may be room
            // for them too.
            self.wake();
        }
        Ok(slot)
    }

    /// Waits for the request in `slot` to be handed back: recovers the
    /// controller if an error stops it meanwhile, and resets it if the
    /// request goes unanswered too long.
    fn finish(&self, slot: usize) {
        let mut deadline = sched::now_ns().saturating_add(TIMEOUT_NS);
        loop {
            if self.wait(deadline, || self.requests[slot].load(Ordering::Acquire) != PENDING) {
                return;
            }
            if self.attention.load(Ordering::Acquire) {
                self.attend(false);
            } else if sched::now_ns() >= deadline {
                self.attend(true);
                deadline = sched::now_ns().saturating_add(TIMEOUT_NS);
            }
        }
    }

    /// Tells the device of the requests held back for it.
    fn kick(&self) {
        if let Some(controller) = self.queue.lock_irq().as_mut() {
            controller.kick();
        }
    }

    /// Frees `slot`, and wakes those waiting for one.
    fn free(&self, slot: usize) {
        self.requests[slot].store(FREE, Ordering::Release);
        self.wake();
    }

    /// Carries out one request and waits for it, again as long as the
    /// device hands it back to go again, up to [`TRIES`] times.
    fn request(&self, op: Op, sector: u64, data: &[(u64, u32)]) -> Result<(), Error> {
        let rank = running_rank();
        let slot = self.issue(rank, None, op, sector, data, false)?;
        self.settle(rank, slot, op, sector, data)
    }

    /// Carries out a transfer of any number of buffers as requests of as
    /// many as the device takes, up to [`PARTS`] of them in flight at
    /// once, and waits for every one: each goes again as [`Disk::request`]
    /// has it. Returns only once the device is done with every buffer,
    /// whatever failed.
    fn request_all(&self, op: Op, sector: u64, data: &[(u64, u32)]) -> Result<(), Error> {
        let rank = running_rank();
        let most = self.info.max_segments.max(1);
        let mut result = Ok(());
        let mut at = sector;
        for window in data.chunks(most * PARTS) {
            // Each part's slot, buffers and first sector.
            let mut parts = [(0usize, 0usize, 0u64); PARTS];
            let mut issued = 0;
            for (i, part) in window.chunks(most).enumerate() {
                let more = (i + 1) * most < window.len();
                match self.issue(rank, None, op, at, part, more) {
                    Ok(slot) => parts[i] = (slot, i * most, at),
                    Err(err) => {
                        result = Err(err);
                        break;
                    }
                }
                issued += 1;
                at += sectors(part);
            }
            self.kick();
            for &(slot, first, start) in &parts[..issued] {
                let part = &window[first..(first + most).min(window.len())];
                let settled = self.settle(rank, slot, op, start, part);
                if result.is_ok() {
                    result = settled;
                }
            }
            result?;
        }
        Ok(())
    }

    /// Waits for the request in `slot` to end, sending it again while the
    /// device hands it back to go again, up to [`TRIES`] times in all;
    /// frees the slot.
    fn settle(
        &self,
        rank: usize,
        mut slot: usize,
        op: Op,
        sector: u64,
        data: &[(u64, u32)],
    ) -> Result<(), Error> {
        let mut tries = 1;
        loop {
            self.finish(slot);
            match self.requests[slot].load(Ordering::Acquire) {
                DONE => break,
                RETRY if tries < TRIES => {
                    tries += 1;
                    self.requests[slot].store(HELD, Ordering::Release);
                    slot = match self.issue(rank, Some(slot), op, sector, data, false) {
                        Ok(slot) => slot,
                        Err(err) => {
                            self.free(slot);
                            return Err(err);
                        }
                    };
                }
                _ => {
                    self.free(slot);
                    return Err(Error::Io);
                }
            }
        }
        self.free(slot);
        let sectors = sectors(data);
        let (count, moved) = match op {
            Op::Read => (&READS, &READ_SECTORS),
            Op::Write | Op::WriteThrough => (&WRITES, &WRITTEN_SECTORS),
            _ => return Ok(()),
        };
        count.fetch_add(1, Ordering::Relaxed);
        moved.fetch_add(sectors, Ordering::Relaxed);
        Ok(())
    }

    /// Writes `segments` from `sector`, as `op`.
    fn write_as(&self, op: Op, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        if self.info.read_only {
            return Err(Error::ReadOnly);
        }
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        radix_block::check(self.info.sectors, self.info.max_segments, sector, lengths)?;
        let mut data = [(0u64, 0u32); MAX_SEGMENTS];
        for (i, segment) in segments.iter().enumerate() {
            data[i] = (segment.page.phys() + segment.offset as u64, segment.len as u32);
        }
        radix_hw::device_barrier();
        self.request(op, sector, &data[..segments.len()])
    }
}

impl<C: Controller> BlockDevice for Disk<C> {
    fn info(&self) -> Info {
        self.info
    }

    fn read(&self, sector: u64, segments: &mut [SegmentMut<'_>]) -> Result<(), Error> {
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        radix_block::check(self.info.sectors, self.info.max_segments, sector, lengths)?;
        let mut data = [(0u64, 0u32); MAX_SEGMENTS];
        for (i, segment) in segments.iter().enumerate() {
            data[i] = (segment.page.phys() + segment.offset as u64, segment.len as u32);
        }
        self.request(Op::Read, sector, &data[..segments.len()])?;
        for segment in segments.iter_mut() {
            let (offset, len) = (segment.offset, segment.len);
            radix_hw::device_wrote(&mut segment.page.bytes_mut()[offset..offset + len]);
        }
        Ok(())
    }

    fn write(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        self.write_as(Op::Write, sector, segments)
    }

    fn write_through(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        if self.fua {
            return self.write_as(Op::WriteThrough, sector, segments);
        }
        self.write_as(Op::Write, sector, segments)?;
        self.flush()
    }

    fn read_frames(&self, sector: u64, frames: &[(u64, u32)]) -> Result<(), Error> {
        radix_block::check_frames(self.info.sectors, sector, frames)?;
        self.request_all(Op::Read, sector, frames)?;
        radix_hw::device_barrier();
        Ok(())
    }

    fn write_frames(&self, sector: u64, frames: &[(u64, u32)]) -> Result<(), Error> {
        if self.info.read_only {
            return Err(Error::ReadOnly);
        }
        radix_block::check_frames(self.info.sectors, sector, frames)?;
        radix_hw::device_barrier();
        self.request_all(Op::Write, sector, frames)
    }

    fn flush(&self) -> Result<(), Error> {
        if !self.write_cache {
            // Without a write cache, a write that returned is durable.
            return Ok(());
        }
        self.request(Op::Flush, 0, &[])
    }

    fn discard(&self, sector: u64, count: u64) -> Result<(), Error> {
        radix_block::check_discard(self.info.sectors, sector, count)?;
        if self.info.read_only {
            return Err(Error::ReadOnly);
        }
        if self.info.discard_sectors == 0 {
            return Err(Error::Unsupported);
        }
        let (mut at, end) = (sector, sector + count);
        while at < end {
            let n = (end - at).min(self.info.discard_sectors);
            self.request(Op::Discard(n), at, &[])?;
            at += n;
        }
        Ok(())
    }
}
