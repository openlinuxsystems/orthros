// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! radix-ring: submission and completion queues shared between a program
//! and the kernel, after Linux's io_uring (radix-ring(2), ADR 0034).
//!
//! The kernel allocates a ring's pages itself, keeps them mapped in its
//! own half for as long as the ring exists, and maps them read-write into
//! the program's address space. `radix_ipc::ring` does the accounting:
//! the kernel keeps its own submission head and completion tail, masks
//! every index the program wrote, and takes an entry only once room for
//! its completion is reserved, so nothing the program writes makes the
//! kernel write outside the ring or lose a completion.
//!
//! Entries are taken in batches with the ring's lock held (copying each
//! entry out once), carried out with no lock held, and completed under the
//! lock again: the completion is written first, then the tail published
//! with release ordering, so a program that sees the tail sees the entry.
//! Some operations finish later: `wait` when its notification is signalled
//! and `timeout` when its time is up, which is checked whenever a thread
//! enters the ring and while one waits in it: a thread waiting there wakes
//! when another adds a timeout, to wait for its deadline too.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

use radix_abi::errors::Kernel;
use radix_abi::ring::{Cqe, LAYOUT_VERSION, Opcode, RingHeader, RingIndexes, Sqe};
use radix_cap::{ObjectKind, Rights, SlotRef};
use radix_hal::{UserAccess, UserMode};
use radix_ipc::range::user_pages;
use radix_ipc::ring::{Layout, Queues};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::SpinLock;

use super::memory::{Backing, SpaceObject, SpaceRef};
use super::objects::{self, Object};
use super::{Result, UserThread, cap_error, error};
use crate::Arch;
use crate::caps;
use crate::domains::DomainRef;
use crate::sched::{self, wait};

/// A ring, shared.
pub type RingRef = Arc<RingObject, Kmalloc>;

/// Entries taken from the submission queue under one hold of the lock.
const BATCH: usize = 16;

/// A ring.
pub struct RingObject {
    /// Its pages, mapped in the kernel's half.
    shared: <Arch as UserMode>::Shared,
    layout: Layout,
    queues: SpinLock<Queues>,
    /// The completion tail as last published, for waiters to read
    /// without the lock.
    tail: AtomicU32,
    /// Timeouts under way: deadline and user data. Room for every
    /// reserved completion is made at setup.
    timeouts: SpinLock<Vec<(u64, u64), Kmalloc>>,
    /// How many timeouts were added, so that a thread waiting with the
    /// deadlines there were knows to look again when another thread adds
    /// one.
    timeouts_added: AtomicU32,
    /// Threads waiting in `ring_enter`.
    waiting: AtomicU32,
    /// Set when its last capability is gone.
    closed: AtomicBool,
    /// The address space it is mapped in, and where.
    space: Weak<SpaceObject, Kmalloc>,
    /// The domain its pages are charged to.
    domain: DomainRef,
}

impl RingObject {
    /// The physical address of page `page` of its shared memory.
    pub fn phys(&self, page: usize) -> u64 {
        Arch::shared_phys(&self.shared, page)
    }

    fn key(&self) -> usize {
        self as *const RingObject as usize
    }

    /// The completion head the program last wrote.
    fn cq_head(&self) -> u32 {
        Arch::shared_load(&self.shared, RingHeader::OFFSET_CQ + RingIndexes::OFFSET_HEAD)
    }

    /// Completions written and not yet read.
    fn ready(&self) -> u32 {
        let unread = self.tail.load(Ordering::Acquire).wrapping_sub(self.cq_head());
        unread.min(self.layout.completions)
    }

    /// Writes a completion at `index` and publishes the tail. With the
    /// queues locked.
    fn write(&self, queues: &Queues, index: u32, user_data: u64, result: Result<[u64; 2]>) {
        let (status, values) = match result {
            Ok(values) => (0, values),
            Err(status) => (status.0, [0; 2]),
        };
        let mut cqe = [0u8; Cqe::SIZE];
        cqe[Cqe::OFFSET_USER_DATA..][..8].copy_from_slice(&user_data.to_le_bytes());
        cqe[Cqe::OFFSET_STATUS..][..8].copy_from_slice(&status.to_le_bytes());
        cqe[Cqe::OFFSET_VALUES..][..8].copy_from_slice(&values[0].to_le_bytes());
        cqe[Cqe::OFFSET_VALUES + 8..][..8].copy_from_slice(&values[1].to_le_bytes());
        Arch::shared_write(&self.shared, self.layout.cqe(index) as usize, &cqe);
        let tail = queues.cq_tail();
        Arch::shared_store(&self.shared, RingHeader::OFFSET_CQ + RingIndexes::OFFSET_TAIL, tail);
        self.tail.store(tail, Ordering::Release);
    }

    /// Wakes the threads waiting in `ring_enter`, if there are any, after
    /// a completion was published or a timeout added.
    fn wake(&self) {
        // Pairs with the barrier a waiter makes after counting itself in
        // `waiting`: either this sees its count, or its look at the tail
        // sees the completion. Without it the load could be done before
        // this CPU's store of the tail was seen, while the waiter counts
        // itself and reads the old tail, and the wake-up would be lost.
        fence(Ordering::SeqCst);
        if self.waiting.load(Ordering::Relaxed) > 0 {
            wait::wake_all(self.key());
        }
    }

    /// Completes an entry it took earlier, with `user_data` and `result`.
    pub fn finish(&self, user_data: u64, result: Result<[u64; 2]>) {
        {
            let mut queues = self.queues.lock();
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            let Some(index) = queues.complete() else { return };
            self.write(&queues, index, user_data, result);
        }
        self.wake();
    }

    /// Posts a completion nothing in this ring asked for (msg_ring).
    fn post(&self, user_data: u64, values: [u64; 2]) -> Result<()> {
        {
            let mut queues = self.queues.lock();
            if self.closed.load(Ordering::Acquire) {
                return Err(error(Kernel::Gone));
            }
            let index = queues.post(self.cq_head()).ok_or(error(Kernel::Busy))?;
            self.write(&queues, index, user_data, Ok(values));
        }
        self.wake();
        Ok(())
    }

    /// Completes the timeouts that are due, and returns the earliest
    /// deadline still to come.
    fn expire(&self) -> Option<u64> {
        let now = sched::now_ns();
        loop {
            let (due, next) = {
                let mut timeouts = self.timeouts.lock();
                let due = timeouts.iter().position(|&(deadline, _)| deadline <= now);
                let due = due.map(|at| timeouts.swap_remove(at));
                (due, timeouts.iter().map(|&(deadline, _)| deadline).min())
            };
            match due {
                Some((_, user_data)) => self.finish(user_data, Ok([0; 2])),
                None => return next,
            }
        }
    }

    /// Its last capability is gone: it is unmapped, and what is under way
    /// is dropped.
    pub fn close(ring: &RingRef) {
        {
            let _queues = ring.queues.lock();
            ring.closed.store(true, Ordering::Release);
        }
        ring.timeouts.lock().clear();
        if let Some(space) = ring.space.upgrade() {
            space.unmap_backing(&Backing::Ring(ring.clone()));
        }
        wait::wake_all(ring.key());
    }
}

impl Drop for RingObject {
    fn drop(&mut self) {
        self.domain.uncharge(u64::from(self.layout.pages()));
    }
}

/// `ring_setup`: makes a ring with `entries` submission entries, maps it
/// at `address` in the address space in slot `space`, and puts its
/// capability in slot `to`. Returns the size mapped.
#[inline(never)]
pub fn setup(user: &UserThread, space: u32, address: u64, entries: u32, to: u32) -> Result<u64> {
    let (_, space) = objects::space(user.cspace, space, Rights::CONTROL)?;
    let layout = Layout::new(entries).ok_or(Kernel::InvalidArgument.status(2))?;
    let pages = u64::from(layout.pages());
    user_pages(address, pages).ok_or(Kernel::InvalidArgument.status(1))?;
    let ring = new(&space, layout)?;
    space.map(address, pages, &Backing::Ring(ring.clone()), 0, UserAccess::ReadWrite)?;
    let made = caps::with(|store| -> Result<()> {
        objects::reserve(store, 1).map_err(|_| error(Kernel::NoMemory))?;
        let id =
            store.create(SlotRef::new(user.cspace, to), ObjectKind::Ring).map_err(cap_error)?;
        objects::insert(store, id, Object::Ring(ring.clone()))
            .map_err(|_| error(Kernel::NoMemory))?;
        Ok(())
    })
    .map_err(cap_error)
    .and_then(|made| made);
    if let Err(err) = made {
        let _ = space.unmap(address, pages);
        return Err(err);
    }
    Ok(u64::from(layout.size))
}

/// A ring for `layout`, with its header written, for `space`.
fn new(space: &SpaceRef, layout: Layout) -> Result<RingRef> {
    let domain = sched::current().map(|me| me.domain().clone()).ok_or(error(Kernel::Gone))?;
    domain.charge(u64::from(layout.pages())).map_err(|_| error(Kernel::NoMemory))?;
    let uncharge = || domain.uncharge(u64::from(layout.pages()));
    let Some(shared) = Arch::new_shared(layout.pages() as usize) else {
        uncharge();
        return Err(error(Kernel::NoMemory));
    };
    // Made here, where it may sleep; under the spin lock it only takes
    // pushes into the room made now.
    let mut timeouts = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    if timeouts.try_reserve_exact(layout.completions as usize).is_err() {
        uncharge();
        return Err(error(Kernel::NoMemory));
    }
    let header = |offset: usize, value: u32| Arch::shared_store(&shared, offset, value);
    for (queue, entries) in
        [(RingHeader::OFFSET_SQ, layout.entries), (RingHeader::OFFSET_CQ, layout.completions)]
    {
        header(queue + RingIndexes::OFFSET_MASK, entries - 1);
        header(queue + RingIndexes::OFFSET_ENTRIES, entries);
    }
    header(RingHeader::OFFSET_SQES, layout.sqes);
    header(RingHeader::OFFSET_CQES, layout.cqes);
    header(RingHeader::OFFSET_SIZE, layout.size);
    header(RingHeader::OFFSET_VERSION, LAYOUT_VERSION);
    let ring = RingObject {
        shared,
        layout,
        queues: SpinLock::new(Queues::new(&layout)),
        tail: AtomicU32::new(0),
        timeouts: SpinLock::new(timeouts),
        timeouts_added: AtomicU32::new(0),
        waiting: AtomicU32::new(0),
        closed: AtomicBool::new(false),
        space: Arc::downgrade(space),
        domain,
    };
    // From here on the ring's drop uncharges.
    Arc::try_new_in(ring, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| error(Kernel::NoMemory))
}

/// `ring_enter`: takes up to `submit` entries and carries them out, then
/// waits for `min_complete` completions or `timeout_ns`. Returns how many
/// entries it took and how many completions wait. Never inlined into the
/// system call dispatcher, whose frame every call pays for: its batch of
/// entries is a kilobyte.
#[inline(never)]
pub fn enter(
    user: &UserThread,
    ring: u32,
    submit: u32,
    min_complete: u32,
    timeout_ns: u64,
) -> Result<(u32, u32)> {
    let (_, ring) = objects::ring(user.cspace, ring, Rights::CONTROL)?;
    if min_complete > ring.layout.completions {
        return Err(Kernel::InvalidArgument.status(2));
    }
    let deadline = (timeout_ns != 0).then(|| sched::now_ns().saturating_add(timeout_ns));
    let mut submitted = 0;
    let mut full = false;
    while submitted < submit && !full {
        let mut batch = [[0u8; Sqe::SIZE]; BATCH];
        let mut taken = 0;
        {
            let mut queues = ring.queues.lock();
            if ring.closed.load(Ordering::Acquire) {
                return Err(error(Kernel::Gone));
            }
            let tail =
                Arch::shared_load(&ring.shared, RingHeader::OFFSET_SQ + RingIndexes::OFFSET_TAIL);
            let available =
                queues.available(tail).map_err(|_| Kernel::InvalidArgument.status(1))?;
            let want = available.min(submit - submitted).min(BATCH as u32) as usize;
            if want == 0 {
                break;
            }
            let cq_head = ring.cq_head();
            while taken < want {
                let Some(index) = queues.take(cq_head) else {
                    full = true;
                    break;
                };
                Arch::shared_read(&ring.shared, ring.layout.sqe(index) as usize, &mut batch[taken]);
                taken += 1;
            }
            Arch::shared_store(
                &ring.shared,
                RingHeader::OFFSET_SQ + RingIndexes::OFFSET_HEAD,
                queues.sq_head(),
            );
        }
        for entry in &batch[..taken] {
            execute(user, &ring, entry);
        }
        submitted += taken as u32;
    }
    if full && submitted == 0 {
        return Err(error(Kernel::Busy));
    }
    loop {
        // Read before the deadlines are: a timeout added after this moves
        // it, and ends the wait below so the new deadline is seen.
        let added = ring.timeouts_added.load(Ordering::Acquire);
        let next = ring.expire();
        let ready = ring.ready();
        if ready >= min_complete {
            return Ok((submitted, ready));
        }
        if sched::interrupted() {
            return Err(error(Kernel::Interrupted));
        }
        if ring.closed.load(Ordering::Acquire) {
            return Err(error(Kernel::Gone));
        }
        let until = match (deadline, next) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if deadline.is_some_and(|deadline| sched::now_ns() >= deadline) {
            return Err(error(Kernel::TimedOut));
        }
        let done = || {
            ring.ready() >= min_complete
                || ring.closed.load(Ordering::Relaxed)
                || ring.timeouts_added.load(Ordering::Relaxed) != added
                || sched::interrupted()
        };
        ring.waiting.fetch_add(1, Ordering::Relaxed);
        // Pairs with the barrier in `wake`: either the thread finishing an
        // entry sees this count, or `done` sees what it wrote.
        fence(Ordering::SeqCst);
        match until {
            None => wait::wait_until(ring.key(), "ring", done),
            Some(until) => {
                wait::wait_until_or_timeout(ring.key(), "ring", until, done);
            }
        }
        ring.waiting.fetch_sub(1, Ordering::Relaxed);
    }
}

fn word(entry: &[u8; Sqe::SIZE], offset: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&entry[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

/// Carries out one submission entry. Its completion is reserved: written
/// now, or by whatever finishes it later.
fn execute(user: &UserThread, ring: &RingRef, entry: &[u8; Sqe::SIZE]) {
    let user_data = word(entry, Sqe::OFFSET_USER_DATA);
    let arg = |i: usize| word(entry, Sqe::OFFSET_ARGS + 8 * i);
    let cap = u32::from_le_bytes([
        entry[Sqe::OFFSET_CAP],
        entry[Sqe::OFFSET_CAP + 1],
        entry[Sqe::OFFSET_CAP + 2],
        entry[Sqe::OFFSET_CAP + 3],
    ]);
    if entry[Sqe::OFFSET_FLAGS] != 0
        || entry[Sqe::OFFSET_RESERVED] != 0
        || entry[Sqe::OFFSET_RESERVED + 1] != 0
    {
        ring.finish(user_data, Err(Kernel::InvalidArgument.status(1)));
        return;
    }
    let result = match Opcode::from_raw(entry[Sqe::OFFSET_OPCODE]) {
        None => Err(Kernel::InvalidArgument.status(0)),
        Some(Opcode::Nop) => Ok([arg(0), 0]),
        Some(Opcode::Notify) => {
            objects::notification(user.cspace, cap, Rights::SEND).map(|(cap, notification)| {
                notification.notify(cap.badge());
                [0; 2]
            })
        }
        Some(Opcode::Wait) => match objects::notification(user.cspace, cap, Rights::RECEIVE) {
            Ok((_, notification)) => {
                notification.wait_ring(ring, user_data);
                return;
            }
            Err(err) => Err(err),
        },
        Some(Opcode::MsgRing) => objects::ring(user.cspace, cap, Rights::SEND)
            .and_then(|(_, target)| target.post(arg(0), [arg(1), arg(2)]))
            .map(|()| [0; 2]),
        Some(Opcode::Random) => Ok([super::random::u64(), super::random::u64()]),
        Some(Opcode::Read) => super::files::ring_read(user, cap, [arg(0), arg(1), arg(2)]),
        Some(Opcode::Write) => super::files::ring_write(user, cap, [arg(0), arg(1), arg(2)]),
        Some(Opcode::Sync) => super::files::ring_sync(user, cap),
        Some(Opcode::Timeout) => {
            let deadline = sched::now_ns().saturating_add(arg(0));
            let mut timeouts = ring.timeouts.lock();
            // Room for every reserved completion was made at setup.
            let added = timeouts.push_within_capacity((deadline, user_data)).is_ok();
            drop(timeouts);
            if added {
                // A thread waiting in the ring with a later deadline, or
                // none, wakes to wait for this one too.
                ring.timeouts_added.fetch_add(1, Ordering::Release);
                ring.wake();
                return;
            }
            Err(error(Kernel::NoMemory))
        }
    };
    ring.finish(user_data, result);
}

/// The ring's sizes, for its `info` method.
pub fn info(ring: &RingObject) -> (u32, u32) {
    (ring.layout.entries, ring.layout.completions)
}
