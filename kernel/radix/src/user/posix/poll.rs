// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `poll` (Phase 8, radix-process(2)): waits until one of many
//! capabilities is ready, as POSIX's `poll` and `ppoll`.
//!
//! The entries are copied in (onto the kernel stack, for up to
//! [`ON_STACK`] of them), then each slot is looked at: a pipe's end
//! or a terminal's side says what it is ready for
//! ([`crate::fs::pipe::Pipe::events`], [`crate::fs::tty::Handle::events`]),
//! a notification is readable while bits are set, other files and
//! directories are always ready, and an empty slot is invalid. If none is
//! ready, and the poll may wait, the thread holds the objects its entries
//! designate, as Linux's `poll` holds its files (a slot closed meanwhile
//! does not change what it waits for), counts itself among the pollers of
//! each ([`Polled`]), and waits for a change to any of them: whatever a
//! poll looks at calls [`Polled::changed`] after it changes, which, while
//! anyone polls it, moves [`GENERATION`] and wakes the pollers. A woken
//! poller does not know which of its objects changed, so it looks at them
//! all again, and every poller wakes for a change to anything polled
//! (docs/design/posix.md lists that limit); a change to what nobody polls,
//! such as a pipe that is only read and written, wakes nobody.
//!
//! No wake-up is lost between a poller's look and its wait: a poller
//! counts itself in each object's [`Polled`] before it reads the
//! generation and looks, and [`Polled::changed`] reads the count after the
//! change, each with a full barrier between, so either the poller sees the
//! change or the change sees the poller (and moves the generation it waits
//! on). A notification, whose word changes by a read-modify-write that is
//! already such a barrier, skips the extra one
//! ([`Polled::changed_by_rmw`]). The TLA+ model `kernel/radix/tla/Poll.tla`
//! checks this.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};

use radix_abi::errors::Kernel;
use radix_abi::process::{PollArgs, PollEntry, PollEvents, PollFlags};
use radix_cap::SpaceId;
use radix_hal::UserMode;
use radix_mm::Kmalloc;

use super::kmalloc;
use super::signal::{self, Restart, interrupted};
use crate::Arch;
use crate::sched::{self, wait};
use crate::user::objects::{self, Object};
use crate::user::{Result, UserRef, error};

/// There is data to read, or the end of a stream.
pub const READABLE: u16 = PollEvents::READABLE.0 as u16;
/// A write would not wait.
pub const WRITABLE: u16 = PollEvents::WRITABLE.0 as u16;
/// An error: a pipe no end reads, a terminal that hung up.
pub const ERROR: u16 = PollEvents::ERROR.0 as u16;
/// The other end has gone.
pub const HANG_UP: u16 = PollEvents::HANG_UP.0 as u16;
/// The slot holds no capability.
pub const INVALID: u16 = PollEvents::INVALID.0 as u16;

/// The events reported whether asked for or not.
const ALWAYS: u16 = ERROR | HANG_UP | INVALID;

/// The most entries one call takes: as many as Linux's default limit of
/// open files allows.
pub const ENTRIES_MAX: u32 = 4096;

/// Moves whenever something polled changes.
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn key() -> usize {
    &raw const GENERATION as usize
}

/// What a poll may look at keeps one of these: how many threads poll it,
/// which its changes wake.
#[derive(Default)]
pub struct Polled {
    pollers: AtomicUsize,
}

impl Polled {
    /// Nobody polls it yet.
    pub const fn new() -> Polled {
        Polled { pollers: AtomicUsize::new(0) }
    }

    /// It has changed: the threads polling it look again. Costs a barrier
    /// and a load while nobody polls it.
    pub fn changed(&self) {
        fence(Ordering::SeqCst);
        self.changed_by_rmw();
    }

    /// As [`Polled::changed`], for a change made by a sequentially
    /// consistent read-modify-write that pollers read with sequentially
    /// consistent loads (a notification's word), which orders as the
    /// barrier would: only a load while nobody polls it.
    pub fn changed_by_rmw(&self) {
        if self.pollers.load(Ordering::SeqCst) > 0 {
            GENERATION.fetch_add(1, Ordering::SeqCst);
            wait::wake_all(key());
        }
    }
}

/// Calls `each` with what `object` keeps for its pollers: a pipe end's
/// pipe, both pipes of a connected socket or a listening one's listener,
/// a terminal, the kernel log or the keyboard, or a notification.
fn polled_by(object: &Object, each: &mut dyn FnMut(&Polled)) {
    match object {
        Object::File(file) => {
            if let Some(end) = file.pipe_end() {
                each(end.pipe().polled());
            } else if let Some(socket) = file.socket() {
                socket.polled(each);
            } else if let Some(handle) = file.tty_handle() {
                each(handle.polled());
            } else if file.is_kmsg() {
                each(&crate::fs::kmsg::POLLED);
            } else if file.is_keyboard() {
                each(&crate::fs::keyboard::POLLED);
            } else if file.is_mouse() {
                each(&crate::fs::mouse::POLLED);
            }
        }
        Object::Notification(notification) => each(notification.polled()),
        // Always ready, or never.
        _ => {}
    }
}

/// Counts the running thread among the pollers of `objects` while it
/// lives.
struct Polling<'a> {
    objects: &'a [Option<Object>],
}

impl<'a> Polling<'a> {
    fn new(objects: &'a [Option<Object>]) -> Polling<'a> {
        for object in objects.iter().flatten() {
            polled_by(object, &mut |polled| {
                polled.pollers.fetch_add(1, Ordering::SeqCst);
            });
        }
        Polling { objects }
    }
}

impl Drop for Polling<'_> {
    fn drop(&mut self) {
        // The poll has looked for the last time, so no change needs to
        // reach it: a count left high a while longer costs a wake-up.
        for object in self.objects.iter().flatten() {
            polled_by(object, &mut |polled| {
                polled.pollers.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }
}

/// An entry's descriptor, and the events it asks for.
fn entry_of(entry: [u8; PollEntry::SIZE]) -> (i32, u16) {
    let fd = i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
    let events =
        u16::from_le_bytes([entry[PollEntry::OFFSET_EVENTS], entry[PollEntry::OFFSET_EVENTS + 1]]);
    (fd, events)
}

/// The events an entry held found when it was copied in.
fn found_of(entry: [u8; PollEntry::SIZE]) -> u16 {
    u16::from_le_bytes([entry[PollEntry::OFFSET_FOUND], entry[PollEntry::OFFSET_FOUND + 1]])
}

fn entries_of(entries: &[u8]) -> core::slice::Iter<'_, [u8; PollEntry::SIZE]> {
    entries.as_chunks::<{ PollEntry::SIZE }>().0.iter()
}

/// What slot `slot` of `cspace` is ready for, among `events` and those
/// always reported.
fn slot_events(cspace: SpaceId, slot: u32, events: u16) -> u16 {
    let found = match objects::find_object(cspace, slot) {
        Some(object) => events_of(&object),
        // An empty slot, nothing in the registry (a memory capability),
        // or released since the capability was found.
        None => INVALID,
    };
    found & (events | ALWAYS)
}

/// What `object` is ready for.
fn events_of(object: &Object) -> u16 {
    match object {
        Object::File(file) => {
            if let Some(end) = file.pipe_end() {
                end.events()
            } else if let Some(socket) = file.socket() {
                socket.events()
            } else if let Some(handle) = file.tty_handle() {
                handle.events()
            } else if file.is_kmsg() {
                crate::fs::kmsg::events(file) | WRITABLE
            } else if file.is_keyboard() {
                crate::fs::keyboard::events(file)
            } else if file.is_mouse() {
                crate::fs::mouse::events(file)
            } else {
                READABLE | WRITABLE
            }
        }
        Object::Directory(_) => READABLE | WRITABLE,
        Object::Notification(notification) => {
            let (set, closed) = notification.peek();
            (if set { READABLE } else { 0 }) | (if closed { HANG_UP } else { 0 })
        }
        _ => 0,
    }
}

/// Looks at every entry's slot, writing the events found in `found`;
/// returns how many found any.
fn look(cspace: SpaceId, base: u32, entries: &[u8], found: &mut [u16]) -> u32 {
    let mut ready = 0;
    for (entry, found) in entries_of(entries).zip(found.iter_mut()) {
        let (fd, events) = entry_of(*entry);
        *found = if fd < 0 {
            0
        } else {
            match base.checked_add(fd as u32) {
                Some(slot) => slot_events(cspace, slot, events),
                None => INVALID,
            }
        };
        ready += u32::from(*found != 0);
    }
    ready
}

/// As [`look`], at the objects `held` for the entries (none for one left
/// out).
fn look_held(held: &[Option<Object>], entries: &[u8], found: &mut [u16]) -> u32 {
    let mut ready = 0;
    for ((entry, object), found) in entries_of(entries).zip(held).zip(found.iter_mut()) {
        let (_, events) = entry_of(*entry);
        *found = object.as_ref().map_or(0, |object| events_of(object) & (events | ALWAYS));
        ready += u32::from(*found != 0);
    }
    ready
}

/// The most entries a call keeps on its kernel stack; a call with more
/// copies them into the heap.
const ON_STACK: usize = 8;

/// `poll`, for `user`'s thread.
#[inline(never)]
pub fn poll(user: &UserRef, a: PollArgs) -> Result<u32> {
    if a.count > ENTRIES_MAX {
        return Err(Kernel::InvalidArgument.status(1));
    }
    let count = a.count as usize;
    let len = count * PollEntry::SIZE;
    if !radix_ipc::range::user_bytes(a.entries, len as u64) {
        return Err(error(Kernel::BadAddress));
    }
    let mut entries_here = [0u8; ON_STACK * PollEntry::SIZE];
    let mut found_here = [0u16; ON_STACK];
    let mut entries_heap: Vec<u8, Kmalloc>;
    let mut found_heap: Vec<u16, Kmalloc> = Vec::new_in(kmalloc());
    let (entries, found): (&mut [u8], &mut [u16]) = if count <= ON_STACK {
        (&mut entries_here[..len], &mut found_here[..count])
    } else {
        let no_memory = |_| error(Kernel::NoMemory);
        entries_heap = radix_mm::bytes::zeroed(len, kmalloc()).map_err(no_memory)?;
        found_heap.try_reserve_exact(count).map_err(no_memory)?;
        while found_heap.len() < count {
            // Room was reserved above.
            let _ = found_heap.push_within_capacity(0);
        }
        (&mut entries_heap, &mut found_heap)
    };
    if !Arch::copy_from_user(entries, a.entries) {
        return Err(error(Kernel::BadAddress));
    }
    let cspace = user.process.cspace();
    // A first look, without counting as a poller: what is ready, or a
    // poll that does not wait, returns at once. One with a signal mask
    // goes on to take it, so that a signal the mask lets through
    // interrupts it, as on Linux.
    let mut ready = look(cspace, a.base, entries, found);
    let waits = a.flags.contains(PollFlags::FOREVER) || a.timeout_ns > 0;
    if ready == 0 && (waits || a.flags.contains(PollFlags::MASK)) {
        ready = wait_ready(user, &a, cspace, entries, found)?;
    }
    // An array polled again mostly holds what it found last time, as an
    // entry left as ready or as idle as it was: only events that differ
    // from what an entry held are written, and nothing at all, not even
    // to a page shared until written, when none do.
    for (i, (entry, found)) in entries_of(entries).zip(found.iter()).enumerate() {
        if found_of(*entry) == *found {
            continue;
        }
        let at = a.entries + (i * PollEntry::SIZE + PollEntry::OFFSET_FOUND) as u64;
        if !Arch::copy_to_user(at, &found.to_le_bytes()) {
            return Err(error(Kernel::BadAddress));
        }
    }
    Ok(ready)
}

/// Waits, as `a` says, until one of `entries` is ready, writing what
/// each one is ready for in `found` (all 0, from a first look); returns
/// how many are, 0 if the time ran out.
fn wait_ready(
    user: &UserRef,
    a: &PollArgs,
    cspace: SpaceId,
    entries: &[u8],
    found: &mut [u16],
) -> Result<u32> {
    let mut held_here: [Option<Object>; ON_STACK] = [const { None }; ON_STACK];
    let mut held_heap: Vec<Option<Object>, Kmalloc> = Vec::new_in(kmalloc());
    let held: &mut [Option<Object>] = if found.len() <= ON_STACK {
        &mut held_here[..found.len()]
    } else {
        held_heap.try_reserve_exact(found.len()).map_err(|_| error(Kernel::NoMemory))?;
        while held_heap.len() < found.len() {
            // Room was reserved above.
            let _ = held_heap.push_within_capacity(None);
        }
        &mut held_heap
    };
    let mut ready = 0;
    for ((entry, held), found) in entries_of(entries).zip(held.iter_mut()).zip(found.iter_mut()) {
        let (fd, _) = entry_of(*entry);
        if fd < 0 {
            continue;
        }
        let object =
            a.base.checked_add(fd as u32).and_then(|slot| objects::find_object(cspace, slot));
        if object.is_none() {
            // Emptied since the first look.
            *found = INVALID;
            ready += 1;
        }
        *held = object;
    }
    if ready > 0 {
        return Ok(ready);
    }
    let masked = a.flags.contains(PollFlags::MASK);
    if masked {
        signal::begin_masked_wait(user, a.mask);
    }
    let deadline = (!a.flags.contains(PollFlags::FOREVER))
        .then(|| sched::now_ns().saturating_add(a.timeout_ns));
    let polling = Polling::new(held);
    let ready = loop {
        let seen = GENERATION.load(Ordering::SeqCst);
        let ready = look_held(held, entries, found);
        if ready > 0 {
            break ready;
        }
        if sched::interrupted() {
            // The mask stays until the handler has run.
            return Err(interrupted(Restart::NoHandler));
        }
        let changed = || GENERATION.load(Ordering::Acquire) != seen || sched::interrupted();
        match deadline {
            None => wait::wait_until(key(), "poll", changed),
            Some(deadline) => {
                if !wait::wait_until_or_timeout(key(), "poll", deadline, changed) {
                    break look_held(held, entries, found);
                }
            }
        }
    };
    drop(polling);
    if masked {
        signal::end_masked_wait(user);
    }
    Ok(ready)
}
