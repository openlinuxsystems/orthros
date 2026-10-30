// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! uACPI's deferred work: two queues, which the kernel's threads empty.
//!
//! uACPI defers what it may not do in its interrupt handler, a
//! general-purpose event's method and the handlers of a `Notify`, as work
//! it schedules with a work item it made beforehand, so that scheduling
//! never allocates. A work item is on a queue at most once at a time, so
//! each queue keeps room for as many entries as uACPI holds work items:
//! making one makes room first (outside the lock, so that nothing is
//! allocated with interrupts masked), and a queued entry is pushed into
//! room that is already there. The queues never shrink.
//!
//! uACPI also waits for all of its work to be done (before it lets an
//! event's handler go, for instance): [`wait_idle`] waits for the
//! interrupt handlers running, then for every entry queued or running.

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use radix_sync::SpinLock;

use crate::{Host, Queue};

/// The C function a piece of work calls, with its context.
pub type Handler = unsafe extern "C" fn(*mut c_void);

type Heap = &'static (dyn Allocator + Sync);

/// A piece of work: `handler(ctx)`.
#[derive(Clone, Copy)]
pub struct Work {
    pub handler: Handler,
    /// The context, an address uACPI gave (its provenance exposed).
    pub ctx: usize,
}

struct Queues {
    /// The work items uACPI holds.
    items: usize,
    events: Vec<Work, Heap>,
    notify: Vec<Work, Heap>,
}

impl Queues {
    fn queue(&mut self, queue: Queue) -> &mut Vec<Work, Heap> {
        match queue {
            Queue::Events => &mut self.events,
            Queue::Notify => &mut self.notify,
        }
    }
}

static QUEUES: SpinLock<Option<Queues>> = SpinLock::new(None);

/// Entries queued or running.
static OUTSTANDING: AtomicUsize = AtomicUsize::new(0);
/// uACPI's interrupt handlers running.
static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

fn outstanding_key() -> usize {
    ptr::from_ref(&OUTSTANDING).addr()
}

fn interrupts_key() -> usize {
    ptr::from_ref(&INTERRUPTS).addr()
}

/// Makes room on both queues for one more work item. Returns false, and
/// counts no item, when memory runs out.
pub fn add_item(host: &'static dyn Host) -> bool {
    loop {
        let need = {
            let mut queues = QUEUES.lock_irq();
            let heap = host.heap();
            let queues = queues.get_or_insert_with(|| Queues {
                items: 0,
                events: Vec::new_in(heap),
                notify: Vec::new_in(heap),
            });
            let need = queues.items + 1;
            if queues.events.capacity() >= need && queues.notify.capacity() >= need {
                queues.items = need;
                return true;
            }
            need
        };
        // Bigger queues, made with interrupts on, then swapped in.
        let room = need.next_power_of_two().max(16);
        let (Ok(events), Ok(notify)) = (bigger(host, room), bigger(host, room)) else {
            return false;
        };
        let mut spare = [events, notify];
        {
            let mut queues = QUEUES.lock_irq();
            let Some(queues) = queues.as_mut() else { return false };
            for (queue, new) in [Queue::Events, Queue::Notify].into_iter().zip(spare.iter_mut()) {
                let old = queues.queue(queue);
                if new.capacity() > old.capacity() {
                    for work in old.drain(..) {
                        // `new` has room for more than the old held.
                        let _ = new.push_within_capacity(work);
                    }
                    core::mem::swap(old, new);
                }
            }
        }
        // The old queues, or the unused new ones, are freed here, with
        // interrupts on. Another item may have taken the room meanwhile:
        // the loop checks again.
        drop(spare);
    }
}

fn bigger(host: &'static dyn Host, room: usize) -> Result<Vec<Work, Heap>, ()> {
    Vec::try_with_capacity_in(room, host.heap()).map_err(|_| ())
}

/// One work item fewer.
pub fn remove_item() {
    if let Some(queues) = QUEUES.lock_irq().as_mut() {
        queues.items = queues.items.saturating_sub(1);
    }
}

/// Queues `work` on `queue`, and tells the host. Returns false if there
/// was no room, which uACPI's promises rule out.
pub fn push(host: &dyn Host, queue: Queue, work: Work) -> bool {
    OUTSTANDING.fetch_add(1, Ordering::AcqRel);
    let pushed = QUEUES
        .lock_irq()
        .as_mut()
        .is_some_and(|queues| queues.queue(queue).push_within_capacity(work).is_ok());
    if !pushed {
        done(host);
        return false;
    }
    host.work_queued(queue);
    true
}

/// The first entry of `queue`, taken off it.
pub fn pop(queue: Queue) -> Option<Work> {
    let mut queues = QUEUES.lock_irq();
    let queue = queues.as_mut()?.queue(queue);
    (!queue.is_empty()).then(|| queue.remove(0))
}

/// An entry from [`pop`] has run.
pub fn done(host: &dyn Host) {
    if OUTSTANDING.fetch_sub(1, Ordering::AcqRel) == 1 {
        host.wake_all(outstanding_key());
    }
}

/// uACPI's interrupt handler starts.
pub fn interrupt_starts() {
    INTERRUPTS.fetch_add(1, Ordering::AcqRel);
}

/// uACPI's interrupt handler is done.
pub fn interrupt_ends(host: &dyn Host) {
    if INTERRUPTS.fetch_sub(1, Ordering::AcqRel) == 1 {
        host.wake_all(interrupts_key());
    }
}

/// Waits for the interrupt handlers running, then for every entry queued
/// or running, including any they queue.
pub fn wait_idle(host: &dyn Host) {
    host.wait(interrupts_key(), None, &|| INTERRUPTS.load(Ordering::Acquire) == 0);
    host.wait(outstanding_key(), None, &|| OUTSTANDING.load(Ordering::Acquire) == 0);
}
