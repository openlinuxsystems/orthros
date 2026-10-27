// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The queue a disk's requests go through, whatever drives the disk.
//!
//! A request blocks its thread until the device is done; many threads have
//! requests in flight at once, up to what the device holds. A disk has one
//! *lane* or several, each a part of its driver with its own lock: an
//! NVMe namespace one for each CPU (a submission and completion queue
//! pair, whose interrupt comes to that CPU), an AHCI port or a virtio disk
//! one. A thread's requests go to its CPU's lane, so that threads on
//! different CPUs neither wait for each other's lock nor are woken from
//! another CPU. A request takes one of the lane's [`SLOTS`] slots, whose
//! state is set when the driver hands the request back ([`Disk::collect`],
//! in the lane's interrupt handler, or in the waiting threads of a device
//! with no interrupt); the thread then frees the slot. A thread waits on
//! its slot alone, so a completion wakes only the thread whose request it
//! ends. A [`Controller`] is
//! the driver's part in a lane: it numbers what it has in flight its own
//! way (descriptor chains, command slots) and says which slot each
//! finished request was.
//!
//! Requests take room by their processes' I/O class and level
//! (radix-process(2)'s `io_priority`, ionice(1)): one takes room only
//! while no more urgent one waits for it, in any lane, the real-time
//! class before best effort, and level 0 before 7 in each; the idle class
//! takes room only while the disk has nothing else to do. A request the
//! device takes only with nothing else in flight (an AHCI disk's flush or
//! TRIM) holds back those of its rank and below that come after it, so
//! that the device empties for it. Once in flight, every request is the
//! device's alike.
//!
//! A raw transfer ([`BlockDevice::read_frames`], into memory a program
//! lends) goes in requests of as many segments as the device takes, not
//! the [`MAX_SEGMENTS`] of the page cache's: a virtio disk's take 254
//! pages, an AHCI disk's 32. A transfer of more goes as several requests,
//! all in flight at once, and the device hears of them
//! together: a controller may hold back the doorbell of a request that
//! has another right behind it ([`Controller::submit`]'s `more`), which
//! lets QEMU's virtio disk merge them into one read or write of its file.
//! Its thread waits for the last request first: requests tend to end in
//! order, so the earlier ones' completions wake no one.
//!
//! A request the device fails ends in an I/O error, which the log tells
//! of, with what the controller said of it, for at most
//! [`FAILURES_LOGGED`] requests of a disk in each 5 s. One it hands back to
//! go again (another request's error stopped it, or the link was reset)
//! goes again, up to [`TRIES`] times in all. A controller an error stopped
//! is restarted by one of the threads waiting for it: that thread takes
//! every lane's part out of its lock, so that it can sleep while the
//! controller recovers, and puts them back; the interrupt handlers leave
//! the disk alone meanwhile. A request unanswered for [`TIMEOUT_NS`] has
//! the controller reset the same way, if it can be (an AHCI port resets
//! its link, an NVMe controller is disabled and enabled again); a virtio
//! device cannot, and its requests wait on.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use radix_abi::process::{IO_LEVELS, IoClass};
use radix_block::{BlockDevice, Error, Info, Segment, SegmentMut};
use radix_hal::{CpuLocal, Timer};
use radix_klog::{TextBuf, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::SpinLock;

use super::{Name, READ_SECTORS, READS, WRITES, WRITTEN_SECTORS};
use crate::Arch;
use crate::sched::{self, wait};
use crate::user::posix::priority;

/// Requests in flight or waiting to be collected, per lane.
pub const SLOTS: usize = 64;
/// The most requests one raw transfer has in flight at once.
pub const PARTS: usize = 16;
/// The most segments one request of the page cache or a filesystem
/// takes: theirs take one page each, so this is plenty. A raw transfer's
/// requests take as many as the device does.
pub const MAX_SEGMENTS: usize = 16;
/// How many times a request goes to the device before it fails, when the
/// device keeps handing it back to go again.
pub const TRIES: u32 = 5;
/// How long a request may go unanswered before the controller is reset:
/// the time Linux gives SCSI and ATA disks.
pub const TIMEOUT_NS: u64 = 30_000_000_000;
/// How many of a disk's failed requests the log tells of in each
/// [`FAILURE_WINDOW_NS`]: Linux's limit on its own messages, so that a
/// dying disk does not drown the log.
pub const FAILURES_LOGGED: u32 = 10;
const FAILURE_WINDOW_NS: u64 = 5_000_000_000;

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

/// A request: what it does, to which of the disk's volumes, and the
/// buffers (physical address, length) it reads into or writes from.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// What it does.
    pub op: Op,
    /// Which volume: an NVMe namespace's ID; 0 on a disk with one.
    pub target: u32,
    /// Its first sector.
    pub sector: u64,
    /// Its buffers.
    pub data: &'a [(u64, u32)],
    /// The sectors in one of the volume's logical blocks: the request
    /// starts and ends on a block's boundary, and every buffer is whole
    /// blocks, which is how a driver whose disk counts in blocks takes it.
    pub unit: u64,
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

/// What the queue needs of a disk's driver, in each of its lanes. Every
/// method but [`Controller::recover`] and [`Controller::reset`] runs
/// with the lane's lock held and interrupts off, and must not wait.
pub trait Controller: Send + Sized {
    /// Whether `op` with `segments` buffers finds room now.
    fn room(&self, op: Op, segments: usize) -> bool;

    /// Whether the device takes `op` only with nothing else in flight.
    fn alone(&self, _op: Op) -> bool {
        false
    }

    /// Starts `request` as the lane's `slot`. With `more`, another
    /// request follows at once: the controller may wait to tell the
    /// device until one comes without it, or until [`Controller::kick`].
    fn submit(&mut self, slot: u8, request: &Request<'_>, more: bool) -> Result<(), Error>;

    /// Tells the device of the requests [`Controller::submit`] held back.
    fn kick(&mut self) {}

    /// The next finished request: its slot and how it ended.
    fn complete(&mut self) -> Option<(u8, Outcome)>;

    /// What the device said of the request in `slot`, which failed or
    /// was handed back, for the log: an NVMe status.
    fn why(&self, _slot: u8) -> Option<&'static str> {
        None
    }

    /// Whether an error stopped the controller, which then takes nothing
    /// until [`Controller::recover`] has run.
    fn stopped(&self) -> bool {
        false
    }

    /// Restarts a stopped controller, given every lane's part in order;
    /// what was in flight comes out of [`Controller::complete`]
    /// afterwards. It may sleep: it runs without the lanes' locks.
    fn recover(_lanes: &mut [Part<Self>]) {}

    /// Resets the controller after a request went unanswered too long,
    /// given every lane's part in order; what was in flight comes out of
    /// [`Controller::complete`] afterwards. False if it cannot be reset.
    /// It may sleep.
    fn reset(_lanes: &mut [Part<Self>]) -> bool {
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

/// A lane's part of a driver, on the heap: it is moved out of its lane
/// while the controller recovers, and some are large.
pub type Part<C> = Box<C, Kmalloc>;

/// Puts `controller` on the heap as a lane's part; `None` when memory
/// runs out.
pub fn part<C>(controller: C) -> Option<Part<C>> {
    Box::try_new_in(controller, HEAP.with(Flags::MAY_SLEEP)).ok()
}

/// One lane of a disk: a part of its driver with its own lock, request
/// slots and waiting threads.
struct Lane<C> {
    /// The driver's part: `None` while a thread recovers or resets it.
    queue: SpinLock<Option<Part<C>>>,
    /// Each request slot's state.
    requests: [AtomicU8; SLOTS],
    /// Moves on every completion and every slot freed: what threads
    /// waiting for room wait on (the lane's key). A thread waiting for its
    /// request waits on its slot's key instead ([`Disk::slot_key`]).
    events: AtomicU64,
}

/// A disk: the device's driver, in one lane or one for each CPU, and the
/// requests waiting for it.
pub struct Disk<C> {
    lanes: Vec<Lane<C>, Kmalloc>,
    /// Room for every lane's part while a thread recovers or resets the
    /// controller, with no lock held: taken out, then put back.
    spare: SpinLock<Vec<Part<C>, Kmalloc>>,
    /// How many requests wait for room, by rank, in every lane.
    waiting: [AtomicU32; RANKS],
    /// How many of them the device takes only alone, and wait for it to
    /// empty, by rank.
    lone: [AtomicU32; RANKS],
    /// An error stopped the controller: a waiting thread is to recover it.
    attention: AtomicBool,
    /// A thread has the controller out to recover or reset it.
    recovering: AtomicBool,
    /// When the log's present window of failures began, and how many it
    /// told of since ([`FAILURES_LOGGED`]).
    failures: SpinLock<(u64, u32)>,
    info: Info,
    /// The most segments a request of a raw transfer takes: the
    /// controller's own limit, not [`MAX_SEGMENTS`].
    raw_segments: usize,
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
    /// The disk `name` that `lanes` drive, one lane each (on the heap,
    /// [`part`]): what it is,
    /// whether it has a write cache and writes through it, and whether
    /// its completions raise an interrupt that calls [`Disk::collect`].
    /// `None` when memory runs out.
    pub fn new(
        lanes: impl ExactSizeIterator<Item = Part<C>>,
        info: Info,
        write_cache: bool,
        fua: bool,
        interrupt: bool,
        name: Name,
    ) -> Option<Disk<C>> {
        let count = lanes.len().max(1);
        let mut built = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
        built.try_reserve_exact(count).ok()?;
        for controller in lanes {
            let lane = Lane {
                queue: SpinLock::new(Some(controller)),
                requests: [const { AtomicU8::new(FREE) }; SLOTS],
                events: AtomicU64::new(0),
            };
            built.push_within_capacity(lane).ok()?;
        }
        if built.is_empty() {
            return None;
        }
        let mut spare = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
        spare.try_reserve_exact(count).ok()?;
        Some(Disk {
            lanes: built,
            spare: SpinLock::new(spare),
            waiting: [const { AtomicU32::new(0) }; RANKS],
            lone: [const { AtomicU32::new(0) }; RANKS],
            attention: AtomicBool::new(false),
            recovering: AtomicBool::new(false),
            failures: SpinLock::new((0, 0)),
            info: Info { max_segments: info.max_segments.min(MAX_SEGMENTS), ..info },
            raw_segments: info.max_segments.max(1),
            write_cache,
            fua,
            interrupt,
            name,
        })
    }

    /// The lane of the CPU this runs on.
    fn here(&self) -> usize {
        <Arch as CpuLocal>::current_cpu() % self.lanes.len()
    }

    fn key(&self, lane: usize) -> usize {
        core::ptr::from_ref::<Lane<C>>(&self.lanes[lane]) as usize
    }

    /// What the thread waiting for the request in `slot` of `lane` waits
    /// on: the slot's state, which no other key shares.
    fn slot_key(&self, lane: usize, slot: usize) -> usize {
        core::ptr::from_ref::<AtomicU8>(&self.lanes[lane].requests[slot]) as usize
    }

    /// Wakes every thread waiting for room in `lane`.
    fn wake(&self, lane: usize) {
        self.lanes[lane].events.fetch_add(1, Ordering::Release);
        wait::wake_all(self.key(lane));
    }

    /// Wakes the threads waiting in every lane, for room or for their
    /// requests.
    fn wake_every_lane(&self) {
        for (lane, this) in self.lanes.iter().enumerate() {
            self.wake(lane);
            for (slot, request) in this.requests.iter().enumerate() {
                if request.load(Ordering::Acquire) != FREE {
                    wait::wake_all(self.slot_key(lane, slot));
                }
            }
        }
    }

    /// Takes the requests the driver has handed back in `lane`, notes an
    /// error that stopped it, and wakes the threads waiting there. Runs
    /// in the lane's interrupt handler, and in waiting threads when the
    /// device has no interrupt; leaves a controller being recovered alone.
    pub fn collect(&self, lane: usize) {
        let Some(this) = self.lanes.get(lane) else { return };
        let mut ended = 0u64;
        let mut stopped = false;
        {
            let mut queue = this.queue.lock_irq();
            let Some(controller) = queue.as_mut() else { return };
            while let Some((slot, outcome)) = controller.complete() {
                let state = match outcome {
                    Outcome::Done => DONE,
                    Outcome::Failed => FAILED,
                    Outcome::Retry => RETRY,
                };
                if let Some(request) = this.requests.get(usize::from(slot)) {
                    request.store(state, Ordering::Release);
                    ended |= 1 << slot;
                }
            }
            if controller.stopped() && !self.attention.load(Ordering::Relaxed) {
                self.attention.store(true, Ordering::Release);
                stopped = true;
            }
        }
        if stopped {
            // A thread in any lane may be the one to recover it.
            self.wake_every_lane();
        } else if ended != 0 {
            let mut left = ended;
            while left != 0 {
                let slot = left.trailing_zeros() as usize;
                left &= left - 1;
                wait::wake_all(self.slot_key(lane, slot));
            }
            // The device has room again.
            self.wake(lane);
        }
    }

    /// Recovers a controller an error stopped or, with `reset`, resets
    /// one that left a request unanswered: with every lane's part out of
    /// its lock, so that it may sleep. Another thread at it already does
    /// it for both.
    fn attend(&self, reset: bool) {
        if self.recovering.swap(true, Ordering::AcqRel) {
            return;
        }
        self.attention.store(false, Ordering::Release);
        let mut taken = core::mem::replace(
            &mut *self.spare.lock_irq(),
            Vec::new_in(HEAP.with(Flags::MAY_SLEEP)),
        );
        for lane in &self.lanes {
            if let Some(controller) = lane.queue.lock_irq().take() {
                // Room for every lane was reserved when the disk was made.
                let _ = taken.push_within_capacity(controller);
            }
        }
        if taken.len() == self.lanes.len() {
            if taken.iter().any(|part| part.stopped()) {
                C::recover(&mut taken);
            } else if reset && !C::reset(&mut taken) {
                warn!("Disk: {}: a request has gone unanswered for 30 s", self.name);
            }
        }
        // Back where they were taken from, the last first: each lane's own
        // part.
        for lane in self.lanes.iter().rev() {
            let mut queue = lane.queue.lock_irq();
            if queue.is_none() {
                *queue = taken.pop();
            }
        }
        *self.spare.lock_irq() = taken;
        self.recovering.store(false, Ordering::Release);
        for lane in 0..self.lanes.len() {
            self.collect(lane);
        }
        // Those waiting for room find it again, whatever was collected.
        self.wake_every_lane();
    }

    /// Waits in `lane` until `condition` holds, an error needs a thread
    /// to recover the controller, or the clock passes `deadline_ns`:
    /// asleep on `key` until an interrupt, or polling the device without
    /// one. Returns whether `condition` holds.
    fn wait(
        &self,
        lane: usize,
        key: usize,
        deadline_ns: u64,
        condition: impl Fn() -> bool,
    ) -> bool {
        // Counted in the load averages while it waits, as on Linux.
        sched::DISK_WAITERS.fetch_add(1, Ordering::Relaxed);
        let _counted = Counted;
        let woken = || condition() || self.attention.load(Ordering::Acquire);
        if self.interrupt {
            wait::wait_until_or_timeout(key, "disk", deadline_ns, woken);
        } else {
            while !woken() && sched::now_ns() < deadline_ns {
                self.collect(lane);
                if !woken() {
                    sched::yield_now();
                }
            }
        }
        condition()
    }

    /// Whether a request of rank `rank` may take room now: no more urgent
    /// one waits for it, and for the idle class, no other request waits or
    /// is in flight in any lane, as Linux's idle class is served only when
    /// the disk is otherwise idle.
    fn may_go(&self, rank: usize) -> bool {
        let others = |range: core::ops::Range<usize>| {
            self.waiting[range].iter().any(|count| count.load(Ordering::Acquire) > 0)
        };
        if rank < IDLE {
            return !others(0..rank);
        }
        !others(0..IDLE)
            && self
                .lanes
                .iter()
                .all(|lane| lane.requests.iter().all(|r| r.load(Ordering::Acquire) == FREE))
    }

    /// Whether a request that goes out alone, of rank `rank` or more
    /// urgent, waits for the device to empty: then those of rank `rank`
    /// wait behind it.
    fn held_back(&self, rank: usize) -> bool {
        self.lone[..=rank].iter().any(|count| count.load(Ordering::Acquire) > 0)
    }

    /// Takes room for `request` in `lane` and starts it, in slot `held`
    /// (a request that goes again) or a free one; returns the slot. With
    /// `more`, another request follows at once ([`Controller::submit`]).
    fn issue(
        &self,
        lane: usize,
        rank: usize,
        held: Option<usize>,
        request: &Request<'_>,
        more: bool,
    ) -> Result<usize, Error> {
        let this = &self.lanes[lane];
        self.waiting[rank].fetch_add(1, Ordering::AcqRel);
        let waiting = Waiting(&self.waiting[rank]);
        let mut lone = None;
        let slot = loop {
            let seen = this.events.load(Ordering::Acquire);
            if self.attention.load(Ordering::Acquire) {
                self.attend(false);
                continue;
            }
            if self.may_go(rank) && (lone.is_some() || !self.held_back(rank)) {
                let mut queue = this.queue.lock_irq();
                if let Some(controller) = queue.as_mut()
                    && !controller.stopped()
                {
                    let free = held.or_else(|| {
                        this.requests.iter().position(|r| r.load(Ordering::Acquire) == FREE)
                    });
                    if let Some(slot) = free {
                        if controller.room(request.op, request.data.len()) {
                            controller.submit(slot as u8, request, more)?;
                            this.requests[slot].store(PENDING, Ordering::Release);
                            break slot;
                        }
                        if lone.is_none() && controller.alone(request.op) {
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
            self.kick(lane);
            let room = || this.events.load(Ordering::Acquire) != seen;
            self.wait(lane, self.key(lane), u64::MAX, room);
        };
        drop(lone);
        drop(waiting);
        if self.waiting.iter().any(|count| count.load(Ordering::Acquire) > 0) {
            // Less urgent requests waited for this one, in any lane:
            // there may be room for them too.
            self.wake_every_lane();
        }
        Ok(slot)
    }

    /// Waits for the request in `slot` of `lane` to be handed back:
    /// recovers the controller if an error stops it meanwhile, and resets
    /// it if the request goes unanswered too long.
    fn finish(&self, lane: usize, slot: usize) {
        let request = &self.lanes[lane].requests[slot];
        let key = self.slot_key(lane, slot);
        let mut deadline = sched::now_ns().saturating_add(TIMEOUT_NS);
        loop {
            if self.wait(lane, key, deadline, || request.load(Ordering::Acquire) != PENDING) {
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

    /// Tells the device of the requests held back for it in `lane`.
    fn kick(&self, lane: usize) {
        if let Some(controller) = self.lanes[lane].queue.lock_irq().as_mut() {
            controller.kick();
        }
    }

    /// Frees `slot` of `lane`, and wakes those waiting there for one.
    fn free(&self, lane: usize, slot: usize) {
        self.lanes[lane].requests[slot].store(FREE, Ordering::Release);
        self.wake(lane);
    }

    /// Carries out `request` in this CPU's lane and waits for it, again
    /// as long as the device hands it back to go again, up to [`TRIES`]
    /// times.
    fn request(&self, request: &Request<'_>) -> Result<(), Error> {
        let (lane, rank) = (self.here(), running_rank());
        let slot = self.issue(lane, rank, None, request, false)?;
        self.settle(lane, rank, slot, request)
    }

    /// Carries out a transfer of any number of buffers as requests of as
    /// many as the device takes, up to [`PARTS`] of them in flight at
    /// once in this CPU's lane, and waits for every one: each goes again
    /// as [`Disk::request`] has it. Returns only once the device is done
    /// with every buffer, whatever failed.
    fn request_all(&self, request: &Request<'_>) -> Result<(), Error> {
        let (lane, rank) = (self.here(), running_rank());
        let most = self.raw_segments;
        let mut result = Ok(());
        let mut at = request.sector;
        for window in request.data.chunks(most * PARTS) {
            // Each part's slot, buffers and first sector.
            let mut parts = [(0usize, 0usize, 0u64); PARTS];
            let mut issued = 0;
            for (i, part) in window.chunks(most).enumerate() {
                let more = (i + 1) * most < window.len();
                let part = Request { sector: at, data: part, ..*request };
                match self.issue(lane, rank, None, &part, more) {
                    Ok(slot) => parts[i] = (slot, i * most, at),
                    Err(err) => {
                        result = Err(err);
                        break;
                    }
                }
                issued += 1;
                at += sectors(part.data);
            }
            self.kick(lane);
            // The last first: the others have likely ended by then.
            for &(slot, first, start) in parts[..issued].iter().rev() {
                let data = &window[first..(first + most).min(window.len())];
                let part = Request { sector: start, data, ..*request };
                let settled = self.settle(lane, rank, slot, &part);
                if result.is_ok() {
                    result = settled;
                }
            }
            result?;
        }
        Ok(())
    }

    /// Waits for `request`, in `slot` of `lane`, to end, sending it again
    /// while the device hands it back to go again, up to [`TRIES`] times
    /// in all; frees the slot.
    fn settle(
        &self,
        lane: usize,
        rank: usize,
        mut slot: usize,
        request: &Request<'_>,
    ) -> Result<(), Error> {
        let requests = &self.lanes[lane].requests;
        let mut tries = 1;
        loop {
            self.finish(lane, slot);
            match requests[slot].load(Ordering::Acquire) {
                DONE => break,
                RETRY if tries < TRIES => {
                    tries += 1;
                    requests[slot].store(HELD, Ordering::Release);
                    slot = match self.issue(lane, rank, Some(slot), request, false) {
                        Ok(slot) => slot,
                        Err(err) => {
                            self.free(lane, slot);
                            return Err(err);
                        }
                    };
                }
                _ => {
                    self.failed(lane, slot, request, tries);
                    self.free(lane, slot);
                    return Err(Error::Io);
                }
            }
        }
        self.free(lane, slot);
        let sectors = sectors(request.data);
        let (count, moved) = match request.op {
            Op::Read => (&READS, &READ_SECTORS),
            Op::Write | Op::WriteThrough => (&WRITES, &WRITTEN_SECTORS),
            _ => return Ok(()),
        };
        count.fetch_add(1, Ordering::Relaxed);
        moved.fetch_add(sectors, Ordering::Relaxed);
        Ok(())
    }

    /// Tells the log of `request`, in `slot` of `lane`, which failed
    /// after `tries` tries, unless it has told of [`FAILURES_LOGGED`]
    /// already in this window.
    fn failed(&self, lane: usize, slot: usize, request: &Request<'_>, tries: u32) {
        let now = sched::now_ns();
        {
            let mut window = self.failures.lock();
            if now.saturating_sub(window.0) >= FAILURE_WINDOW_NS {
                *window = (now, 0);
            }
            if window.1 >= FAILURES_LOGGED {
                return;
            }
            window.1 += 1;
        }
        let why = self.lanes[lane]
            .queue
            .lock_irq()
            .as_ref()
            .and_then(|controller| controller.why(slot as u8));
        let mut volume = Name::new();
        let _ = write!(volume, "{}", self.name);
        if request.target != 0 {
            let _ = write!(volume, "n{}", request.target);
        }
        let mut what = TextBuf::<64>::new();
        let _ = match request.op {
            Op::Read | Op::Write | Op::WriteThrough | Op::Discard(_) => {
                let (op, count) = match request.op {
                    Op::Read => ("read", sectors(request.data)),
                    Op::Discard(count) => ("discard", count),
                    _ => ("write", sectors(request.data)),
                };
                write!(what, "a {op} of {count} sectors from sector {}", request.sector)
            }
            Op::Flush => what.write_str("a flush"),
        };
        let mut after = TextBuf::<16>::new();
        if tries > 1 {
            let _ = write!(after, " after {tries} tries");
        }
        warn!("Disk: {volume}: {what} failed{after}: {}", why.unwrap_or("an I/O error"));
    }
}

/// What a disk serves as a block device: the whole of it, or one of its
/// namespaces, all through the disk's lanes.
pub struct Volume<C> {
    disk: Arc<Disk<C>, Kmalloc>,
    /// Its ID in the requests.
    target: u32,
    info: Info,
}

impl<C: Controller> Volume<C> {
    /// The block device for volume `target` of `disk`: its whole for a
    /// disk with one (target 0), or one of an NVMe controller's
    /// namespaces, which `info` says what it is. Its requests take as many
    /// segments as the disk's do.
    pub fn new(disk: Arc<Disk<C>, Kmalloc>, target: u32, info: Info) -> Self {
        // A discard of the most sectors is whole blocks too.
        let unit = info.unit();
        let discard_sectors = info.discard_sectors / unit * unit;
        let info = Info { max_segments: disk.info.max_segments, discard_sectors, ..info };
        Volume { disk, target, info }
    }

    /// Carries out `op` from `sector` with `data`, as [`Disk::request`].
    fn request(&self, op: Op, sector: u64, data: &[(u64, u32)]) -> Result<(), Error> {
        let unit = self.info.unit();
        self.disk.request(&Request { op, target: self.target, sector, data, unit })
    }

    /// Writes `segments` from `sector`, as `op`.
    fn write_as(&self, op: Op, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        if self.info.read_only {
            return Err(Error::ReadOnly);
        }
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        let Info { sectors, max_segments, .. } = self.info;
        radix_block::check(sectors, self.info.unit(), max_segments, sector, lengths)?;
        let mut data = [(0u64, 0u32); MAX_SEGMENTS];
        for (i, segment) in segments.iter().enumerate() {
            data[i] = (segment.page.phys() + segment.offset as u64, segment.len as u32);
        }
        radix_hw::device_barrier();
        self.request(op, sector, &data[..segments.len()])
    }
}

impl<C: Controller> BlockDevice for Volume<C> {
    fn info(&self) -> Info {
        self.info
    }

    fn read(&self, sector: u64, segments: &mut [SegmentMut<'_>]) -> Result<(), Error> {
        let lengths = segments.iter().map(|s| (s.offset, s.len));
        let Info { sectors, max_segments, .. } = self.info;
        radix_block::check(sectors, self.info.unit(), max_segments, sector, lengths)?;
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
        if self.disk.fua {
            return self.write_as(Op::WriteThrough, sector, segments);
        }
        self.write_as(Op::Write, sector, segments)?;
        self.flush()
    }

    fn read_frames(&self, sector: u64, frames: &[(u64, u32)]) -> Result<(), Error> {
        let unit = self.info.unit();
        radix_block::check_frames(self.info.sectors, unit, sector, frames)?;
        let request = Request { op: Op::Read, target: self.target, sector, data: frames, unit };
        self.disk.request_all(&request)?;
        radix_hw::device_barrier();
        Ok(())
    }

    fn write_frames(&self, sector: u64, frames: &[(u64, u32)]) -> Result<(), Error> {
        if self.info.read_only {
            return Err(Error::ReadOnly);
        }
        let unit = self.info.unit();
        radix_block::check_frames(self.info.sectors, unit, sector, frames)?;
        radix_hw::device_barrier();
        let request = Request { op: Op::Write, target: self.target, sector, data: frames, unit };
        self.disk.request_all(&request)
    }

    fn flush(&self) -> Result<(), Error> {
        if !self.disk.write_cache {
            // Without a write cache, a write that returned is durable.
            return Ok(());
        }
        self.request(Op::Flush, 0, &[])
    }

    fn discard(&self, sector: u64, count: u64) -> Result<(), Error> {
        radix_block::check_discard(self.info.sectors, self.info.unit(), sector, count)?;
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
