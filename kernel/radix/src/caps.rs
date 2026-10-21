// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel's capability store.
//!
//! One [`Capabilities`] store (`radix-cap`) holds every capability space,
//! behind one lock. Its memory comes from the kernel heap. Memory objects
//! take their pages from the frame allocator, owned by
//! [`Owner::Memory`], and give them back when the store releases them.
//!
//! Until the scheduler arrives (Phase 4) the lock is a spin lock, and the
//! store allocates with `MaySleep` while holding it: nothing sleeps yet,
//! and the heap's reclaim takes only its own locks. Phase 4 turns it into
//! a lock that sleeps. Revokes run in steps of [`REVOKE_STEP`] deletions
//! and let go of the lock between steps, so other CPUs are not shut out
//! for long.

use core::fmt;

use radix_cap::{
    CapError, Capabilities, MAX_DEPTH, MAX_SLOTS, ObjectId, ObjectKind, Progress, Released, Rights,
    SlotRef,
};
use radix_klog::{error, info};
use radix_mm::{FRAMES, Flags, HEAP, Kmalloc, Owner, PAGE_SHIFT};
use radix_sync::SpinLock;

/// The store's type in the kernel.
pub type Store = Capabilities<Kmalloc>;

/// The store, once [`init`] has run.
static STORE: SpinLock<Option<Store>> = SpinLock::new(None);

/// Capabilities a revoke deletes before it lets go of the lock.
pub const REVOKE_STEP: usize = 64;

/// Sets up the store and reports it.
pub fn init() {
    *STORE.lock() = Some(Capabilities::new(HEAP.with(Flags::MAY_SLEEP)));
    info!(
        "Capabilities: ready; {} object kinds, {} rights, spaces of up to {MAX_SLOTS} slots, \
         derivations up to {MAX_DEPTH} deep",
        ObjectKind::ALL.len(),
        Rights::ALL.len()
    );
}

/// Runs `f` on the store with its lock held, if the store is set up.
pub fn with<R>(f: impl FnOnce(&mut Store) -> R) -> Result<R, CapError> {
    let mut store = STORE.lock();
    match store.as_mut() {
        Some(store) => Ok(f(store)),
        None => Err(CapError::NoSuchSpace),
    }
}

/// Like [`with`], but gives up instead of waiting for the lock: for `kdb`,
/// which may have stopped the CPU that holds it.
pub fn try_with<R>(f: impl FnOnce(&Store) -> R) -> Option<R> {
    let store = STORE.try_lock()?;
    store.as_ref().map(f)
}

/// Writes what the store holds, by kind, for `kdb`. Gives up if another
/// CPU holds the store's lock.
pub fn write_census(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
    let Some(census) = try_with(Store::census) else {
        line(format_args!("Capabilities: the store is not set up, or another CPU holds its lock"));
        return;
    };
    line(format_args!(
        "Capabilities: {} spaces with {} slots, {} capabilities to {} objects, {} pages in memory \
         objects; deepest derivation {}, {} revokes under way",
        census.spaces,
        census.slots,
        census.total_caps(),
        census.total_objects(),
        census.memory_pages,
        census.deepest,
        census.revoking
    ));
    for kind in ObjectKind::ALL {
        let (caps, objects) = (census.caps[kind.index()], census.objects[kind.index()]);
        if caps > 0 || objects > 0 {
            line(format_args!(
                "  {:>13}: {caps:>6} capabilities, {objects:>6} objects",
                kind.name()
            ));
        }
    }
}

/// Gives back what the store released: memory objects' pages return to
/// the frame allocator.
pub fn release(released: Released) {
    let Some(memory) = released.memory else { return };
    let order = memory.pages.trailing_zeros();
    if let Err(err) = FRAMES.free(memory.base << PAGE_SHIFT, order, Owner::Memory) {
        error!("Capabilities: could not free {} pages of {}: {err}", memory.pages, released.id);
    }
}

/// Makes a memory object of 2^`order` pages from the frame allocator and
/// puts its original capability in `at`.
pub fn create_memory(at: SlotRef, order: u32) -> Result<ObjectId, CapError> {
    let phys =
        FRAMES.alloc(order, Flags::MAY_SLEEP, Owner::Memory).map_err(|_| CapError::NoMemory)?;
    let made =
        with(|store| store.create_memory(at, phys >> PAGE_SHIFT, 1 << order)).and_then(|r| r);
    if made.is_err() {
        let _ = FRAMES.free(phys, order, Owner::Memory);
    }
    made
}

/// Deletes the capability in `at`.
pub fn delete(at: SlotRef) -> Result<(), CapError> {
    with(|store| store.delete(at, &mut release))?
}

/// Revokes everything derived from the capability in `at`, in steps of
/// [`REVOKE_STEP`] with the lock let go in between. Returns how many
/// steps it took.
pub fn revoke(at: SlotRef) -> Result<u32, CapError> {
    let mut steps = 0;
    loop {
        steps += 1;
        match with(|store| store.revoke(at, REVOKE_STEP, &mut release))?? {
            Progress::Done => return Ok(steps),
            Progress::More => core::hint::spin_loop(),
        }
    }
}

/// Destroys a capability space and everything in it.
pub fn destroy_space(space: radix_cap::SpaceId) -> Result<(), CapError> {
    with(|store| store.destroy_space(space, &mut release))?
}
