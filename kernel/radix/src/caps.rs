// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel's capability store.
//!
//! One [`Capabilities`] store (`radix-cap`) holds every capability space,
//! behind one lock. Its memory comes from the kernel heap. Memory objects
//! take their pages from the frame allocator, owned by
//! [`Owner::Memory`], and give them back when the store releases them.
//!
//! The lock is a mutex, so a thread waiting for the store sleeps, and the
//! store may allocate with `MaySleep` while holding it (the out-of-memory
//! policy may then sleep too; it never needs the store). Revokes run in
//! steps of [`REVOKE_STEP`] deletions and let go of the lock between
//! steps, yielding the CPU, so other threads are not shut out for long.
//!
//! Looking a capability up takes no lock and writes nothing shared when
//! the store has not changed since this CPU last looked at that slot
//! ([`get`]): every change to the store moves a version number, and each
//! CPU keeps the capabilities it found, with the version they were found
//! at. So programs on different CPUs using their capabilities at once do
//! not contend, as long as nothing changes the store.
//!
//! Resource domains (`domains`) are objects in the store too: a domain
//! lives while a capability to it does, and goes when the last one is
//! deleted or revoked.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use radix_cap::{
    CapError, Capabilities, Capability, MAX_DEPTH, MAX_SLOTS, ObjectId, ObjectKind, Progress,
    Released, Rights, SlotRef,
};
use radix_hal::MAX_CPUS;
use radix_klog::{error, info};
use radix_mm::{FRAMES, Flags, HEAP, Kmalloc, Owner, PAGE_SHIFT};
use radix_sync::{Mutex, SpinLock};

use crate::domains::{self, DomainError, DomainRef, Settings};
use crate::percpu::{PerCpu, Slot};

/// The store's type in the kernel.
pub type Store = Capabilities<Kmalloc>;

/// The store, once [`init`] has run.
static STORE: Mutex<Option<Store>> = Mutex::new(None);

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
    let result = match store.as_mut() {
        Some(store) => Ok(f(store)),
        None => Err(CapError::NoSuchSpace),
    };
    // `f` may have changed anything, so every capability a CPU has cached
    // is stale from now on: before the lock goes, so that whoever sees the
    // change through the store sees the new version too.
    VERSION.fetch_add(1, Ordering::Release);
    result
}

/// Moves on every change to the store, under its lock.
static VERSION: AtomicU64 = AtomicU64::new(0);

/// Capabilities a CPU keeps, in a table indexed by slot.
const CACHED: usize = 16;

/// A capability this CPU found in a slot, and the store's version then.
#[derive(Clone, Copy)]
struct Cached {
    at: SlotRef,
    version: u64,
    cap: Capability,
}

/// Each CPU's capabilities. Only its own CPU takes the lock, with
/// preemption off, so it is never contended.
static CACHE: PerCpu<SpinLock<[Option<Cached>; CACHED]>> =
    PerCpu::new([const { Slot::new(SpinLock::new([None; CACHED])) }; MAX_CPUS]);

fn cache_index(at: SlotRef) -> usize {
    (at.slot as usize ^ at.space.index as usize) % CACHED
}

/// The capability in `at`, as [`Store::get`] gives it.
///
/// If this CPU found it at the version the store has now, nothing has
/// changed since, and it is still there: no lock, no shared write. A
/// change under way that has not moved the version yet has not returned
/// either, so a lookup that misses it happened before it. Otherwise the
/// store is asked under its lock, and the answer kept with the version,
/// which cannot move while the lock is held.
pub fn get(at: SlotRef) -> Result<Capability, CapError> {
    let now = VERSION.load(Ordering::Acquire);
    let index = cache_index(at);
    let hit = CACHE
        .with(|cache| cache.lock()[index].filter(|entry| entry.at == at && entry.version == now));
    if let Some(entry) = hit {
        return Ok(entry.cap);
    }
    let (found, version) = {
        let store = STORE.lock();
        let found = store.as_ref().ok_or(CapError::NoSuchSpace)?.get(at);
        (found, VERSION.load(Ordering::Relaxed))
    };
    if let Ok(cap) = found {
        CACHE.with(|cache| cache.lock()[index] = Some(Cached { at, version, cap }));
    }
    found
}

/// The capability in `at`, if it may be used on a `kind` with the rights
/// `needs`, as [`Store::lookup`] decides it, through [`get`].
pub fn lookup(at: SlotRef, kind: ObjectKind, needs: Rights) -> Result<Capability, CapError> {
    let cap = get(at)?;
    cap.check(kind, needs)?;
    Ok(cap)
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
    let registered = crate::user::objects::census();
    for kind in ObjectKind::ALL {
        let (caps, objects) = (census.caps[kind.index()], census.objects[kind.index()]);
        if caps > 0 || objects > 0 {
            match registered.map(|counts| counts[kind.index()]).filter(|&n| n > 0) {
                Some(n) => line(format_args!(
                    "  {:>13}: {caps:>6} capabilities, {objects:>6} objects, {n:>6} kernel objects",
                    kind.name()
                )),
                None => line(format_args!(
                    "  {:>13}: {caps:>6} capabilities, {objects:>6} objects",
                    kind.name()
                )),
            }
        }
    }
}

/// Gives back what the store released: kernel objects leave the registry
/// and close (`user::objects`), memory objects' pages return to the frame
/// allocator, and domains leave the domain table.
pub fn release(released: Released) {
    if released.kind == ObjectKind::ResourceDomain {
        domains::released(released.id);
    }
    // Kernel objects close first: a frame is unmapped before its memory
    // can go back.
    crate::user::objects::released(&released);
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

/// Makes a resource domain under `parent` and puts its original capability
/// in `at`.
pub fn create_domain(
    at: SlotRef,
    parent: &DomainRef,
    name: &str,
    settings: Settings,
) -> Result<DomainRef, CapError> {
    with(|store| {
        let id = store.create(at, ObjectKind::ResourceDomain)?;
        match domains::create(name, parent, settings, Some(id)) {
            Ok(domain) => Ok(domain),
            Err(err) => {
                // The capability goes again; nothing was made.
                let _ = store.delete(at, &mut |_| {});
                Err(match err {
                    DomainError::NoMemory => CapError::NoMemory,
                    DomainError::InvalidArgument | DomainError::OverLimit => {
                        CapError::InvalidArgument
                    }
                })
            }
        }
    })?
}

/// The domain the capability in `at` names, if it has `needs` on it.
pub fn domain_at(at: SlotRef, needs: Rights) -> Result<DomainRef, CapError> {
    let cap = lookup(at, ObjectKind::ResourceDomain, needs)?;
    cap.id().and_then(domains::by_object).ok_or(CapError::Empty)
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
            Progress::More => crate::sched::yield_now(),
        }
    }
}

/// Destroys a capability space and everything in it.
pub fn destroy_space(space: radix_cap::SpaceId) -> Result<(), CapError> {
    with(|store| store.destroy_space(space, &mut release))?
}
