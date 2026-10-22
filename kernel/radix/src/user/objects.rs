// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The registry of kernel objects: what each object name the capability
//! store hands out stands for.
//!
//! The store keeps capabilities and the derivation tree, and knows each
//! object only by its kind and an [`ObjectId`]; the objects themselves
//! (an endpoint's queues, a frame's mappings) live here, in a table
//! indexed like the store's object table and checked against its
//! generation. An object enters the registry when the kernel makes it,
//! right after the store gives it a name, and leaves when the store
//! releases the name (its last capability is gone), which also closes
//! it: an endpoint fails its waiting threads, a frame is unmapped from
//! every address space. Holders of a reference (a thread blocked on an
//! endpoint, a region of an address space) keep the memory alive until
//! they let go.
//!
//! The registry changes only with the store's lock held, so the store's
//! version (`caps::version`) moves on every change. Finding an object
//! takes no lock when this CPU found it before at the version the store
//! has now ([`get`]), as finding a capability does (`caps::get`): system
//! calls on different CPUs share no lock to reach their objects.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use radix_abi::errors::Kernel;
use radix_cap::{Capability, ObjectId, ObjectKind, Released, Rights, SlotRef, SpaceId};
use radix_hal::MAX_CPUS;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::Mutex;

use super::account::Charge;
use super::ipc::{Endpoint, EndpointRef};
use super::memory::{FrameObject, FrameRef, SpaceObject, SpaceRef};
use super::notify::{Notification, NotificationRef};
use super::process::{ProcessObject, ProcessRef};
use super::ring::{RingObject, RingRef};
use super::{Result, cap_error, error};
use crate::caps::{self, Store};
use crate::fs::{FileObject, FileRef};
use crate::percpu::{Own, PerCpu, Slot};
use crate::sched::{Thread, ThreadRef};

/// A kernel object.
#[derive(Clone)]
pub enum Object {
    /// An IPC endpoint.
    Endpoint(EndpointRef),
    /// A notification.
    Notification(NotificationRef),
    /// A thread.
    Thread(ThreadRef),
    /// An address space.
    Space(SpaceRef),
    /// A frame.
    Frame(FrameRef),
    /// A ring.
    Ring(RingRef),
    /// A process.
    Process(ProcessRef),
    /// The kernel's random number generator.
    Random,
    /// The kernel log.
    KernelLog,
    /// A file (radix-file(2)).
    File(FileRef),
    /// A directory (radix-directory(2)).
    Directory(FileRef),
}

impl Object {
    /// The kind of object it is.
    pub fn kind(&self) -> ObjectKind {
        match self {
            Object::Endpoint(_) => ObjectKind::Endpoint,
            Object::Notification(_) => ObjectKind::Notification,
            Object::Thread(_) => ObjectKind::Thread,
            Object::Space(_) => ObjectKind::AddressSpace,
            Object::Frame(_) => ObjectKind::Frame,
            Object::Ring(_) => ObjectKind::Ring,
            Object::Process(_) => ObjectKind::Process,
            Object::Random => ObjectKind::Random,
            Object::KernelLog => ObjectKind::KernelLog,
            Object::File(_) => ObjectKind::File,
            Object::Directory(_) => ObjectKind::Directory,
        }
    }
}

struct Entry {
    generation: u32,
    object: Object,
    /// What the object costs, charged to the account of the process that
    /// made it (`process.create`), and given back when it leaves.
    charge: Option<Charge>,
}

static REGISTRY: Mutex<Option<Vec<Option<Entry>, Kmalloc>>> = Mutex::new(None);

/// Sets up the registry.
pub fn init() {
    *REGISTRY.lock() = Some(Vec::new_in(HEAP.with(Flags::MAY_SLEEP)));
}

/// The registry is out of memory.
#[derive(Clone, Copy, Debug)]
pub struct NoMemory;

/// Makes room for the next `count` objects `store` makes, whatever their
/// indexes (`Store::object_records`), so that inserting them cannot fail.
/// Called with the store locked, which every insertion also holds, so
/// nothing else takes the room first.
pub fn reserve(store: &Store, count: usize) -> core::result::Result<(), NoMemory> {
    let mut registry = REGISTRY.lock();
    let Some(table) = registry.as_mut() else { return Err(NoMemory) };
    let needed = store.object_records().saturating_add(count);
    table.try_reserve(needed.saturating_sub(table.len())).map_err(|_| NoMemory)
}

/// Records `object` under `id`. The store must be locked (it hands over
/// `store` to show it), so that the store's version moves after the
/// change.
pub fn insert(store: &Store, id: ObjectId, object: Object) -> core::result::Result<(), NoMemory> {
    insert_charged(store, id, object, None)
}

/// Records `object` under `id`, with what it was charged, which is given
/// back when it leaves the registry.
pub fn insert_charged(
    _store: &Store,
    id: ObjectId,
    object: Object,
    charge: Option<Charge>,
) -> core::result::Result<(), NoMemory> {
    let mut registry = REGISTRY.lock();
    let Some(table) = registry.as_mut() else { return Err(NoMemory) };
    let index = id.index as usize;
    if index >= table.len() {
        table.try_reserve(index + 1 - table.len()).map_err(|_| NoMemory)?;
        while table.len() <= index {
            // Room was reserved above.
            let _ = table.push_within_capacity(None);
        }
    }
    table[index] = Some(Entry { generation: id.generation, object, charge });
    Ok(())
}

/// The object named `id`, if it exists.
///
/// If this CPU found it at the version the store has now, the registry
/// has not changed since, and the object is still there: no lock, and
/// nothing shared written but the object's reference count. A change
/// under way that has not moved the version yet has not returned either,
/// so a lookup that misses it happened before it. Otherwise the registry
/// is asked under its lock, and the answer kept with the version.
pub fn get(id: ObjectId) -> Option<Object> {
    let now = caps::version();
    let index = cache_index(id);
    let hit = CACHE.with_own(|cache| match &cache[index] {
        Some(entry) if entry.id == id && entry.version == now => entry.object.upgrade(),
        _ => None,
    });
    match hit {
        Some(Some(object)) => Some(object),
        _ => get_from_registry(id, index),
    }
}

/// [`get`] when this CPU's cache cannot answer: kept apart, so that a hit
/// needs neither its registers nor its stack.
#[cold]
#[inline(never)]
fn get_from_registry(id: ObjectId, index: usize) -> Option<Object> {
    let (object, version) = {
        let registry = REGISTRY.lock();
        let entry = registry.as_ref()?.get(id.index as usize)?.as_ref()?;
        if entry.generation != id.generation {
            return None;
        }
        // Read under the registry's lock: a change to the entry after it
        // lets go moves the version past this one.
        (entry.object.clone(), caps::version())
    };
    let kept = Cached { id, version, object: Kept::of(&object) };
    // What the entry held before goes once preemption is back on: it may
    // be the last reference to an object's memory.
    let before = CACHE.with_own(|cache| cache[index].replace(kept));
    drop(before);
    Some(object)
}

/// Objects a CPU keeps, in a table indexed by object.
const CACHED: usize = 16;

/// An object as a CPU's cache keeps it: a weak reference, which keeps its
/// memory but not the object, so a cache never holds on to an object the
/// registry has let go of. A hit takes a strong reference again, which
/// fails once the object is gone.
enum Kept {
    Endpoint(Weak<Endpoint, Kmalloc>),
    Notification(Weak<Notification, Kmalloc>),
    Thread(Weak<Thread, Kmalloc>),
    Space(Weak<SpaceObject, Kmalloc>),
    Frame(Weak<FrameObject, Kmalloc>),
    Ring(Weak<RingObject, Kmalloc>),
    Process(Weak<ProcessObject, Kmalloc>),
    Random,
    KernelLog,
    File(Weak<FileObject, Kmalloc>),
    Directory(Weak<FileObject, Kmalloc>),
}

impl Kept {
    fn of(object: &Object) -> Kept {
        match object {
            Object::Endpoint(endpoint) => Kept::Endpoint(Arc::downgrade(endpoint)),
            Object::Notification(notification) => Kept::Notification(Arc::downgrade(notification)),
            Object::Thread(thread) => Kept::Thread(Arc::downgrade(thread)),
            Object::Space(space) => Kept::Space(Arc::downgrade(space)),
            Object::Frame(frame) => Kept::Frame(Arc::downgrade(frame)),
            Object::Ring(ring) => Kept::Ring(Arc::downgrade(ring)),
            Object::Process(process) => Kept::Process(Arc::downgrade(process)),
            Object::Random => Kept::Random,
            Object::KernelLog => Kept::KernelLog,
            Object::File(file) => Kept::File(Arc::downgrade(file)),
            Object::Directory(dir) => Kept::Directory(Arc::downgrade(dir)),
        }
    }

    fn upgrade(&self) -> Option<Object> {
        Some(match self {
            Kept::Endpoint(endpoint) => Object::Endpoint(endpoint.upgrade()?),
            Kept::Notification(notification) => Object::Notification(notification.upgrade()?),
            Kept::Thread(thread) => Object::Thread(thread.upgrade()?),
            Kept::Space(space) => Object::Space(space.upgrade()?),
            Kept::Frame(frame) => Object::Frame(frame.upgrade()?),
            Kept::Ring(ring) => Object::Ring(ring.upgrade()?),
            Kept::Process(process) => Object::Process(process.upgrade()?),
            Kept::Random => Object::Random,
            Kept::KernelLog => Object::KernelLog,
            Kept::File(file) => Object::File(file.upgrade()?),
            Kept::Directory(dir) => Object::Directory(dir.upgrade()?),
        })
    }
}

/// An object this CPU found, and the store's version then.
struct Cached {
    id: ObjectId,
    version: u64,
    object: Kept,
}

/// Each CPU's objects, which only its own CPU uses.
static CACHE: PerCpu<Own<[Option<Cached>; CACHED]>> =
    PerCpu::new([const { Slot::new(Own::new([const { None }; CACHED])) }; MAX_CPUS]);

fn cache_index(id: ObjectId) -> usize {
    (id.index ^ id.generation) as usize % CACHED
}

/// The capability in slot `slot` of `cspace`, if it may be used on a
/// `kind` with the rights `needs`, and the object it designates.
pub fn lookup(
    cspace: SpaceId,
    slot: u32,
    kind: ObjectKind,
    needs: Rights,
) -> Result<(Capability, Object)> {
    let cap = caps::lookup(SlotRef::new(cspace, slot), kind, needs).map_err(cap_error)?;
    let object = object_of(&cap)?;
    Ok((cap, object))
}

/// The capability in slot `slot` of `cspace`, whatever it designates.
pub fn capability(cspace: SpaceId, slot: u32) -> Result<Capability> {
    caps::get(SlotRef::new(cspace, slot)).map_err(cap_error)
}

/// The object `cap`, looked up a moment ago, designates.
pub fn object_of(cap: &Capability) -> Result<Object> {
    // Released since the lookup: as good as gone.
    cap.id().and_then(get).ok_or(error(Kernel::Gone))
}

macro_rules! typed_lookup {
    ($(#[$doc:meta])* $name:ident, $kind:ident, $variant:ident, $type:ty) => {
        $(#[$doc])*
        pub fn $name(cspace: SpaceId, slot: u32, needs: Rights) -> Result<(Capability, $type)> {
            match lookup(cspace, slot, ObjectKind::$kind, needs)? {
                (cap, Object::$variant(object)) => Ok((cap, object)),
                _ => Err(error(Kernel::Gone)),
            }
        }
    };
}

typed_lookup!(
    /// The endpoint a capability designates.
    endpoint,
    Endpoint,
    Endpoint,
    EndpointRef
);
typed_lookup!(
    /// The notification a capability designates.
    notification,
    Notification,
    Notification,
    NotificationRef
);
typed_lookup!(
    /// The address space a capability designates.
    space,
    AddressSpace,
    Space,
    SpaceRef
);
typed_lookup!(
    /// The frame a capability designates.
    frame,
    Frame,
    Frame,
    FrameRef
);
typed_lookup!(
    /// The ring a capability designates.
    ring,
    Ring,
    Ring,
    RingRef
);
typed_lookup!(
    /// The thread a capability designates.
    thread,
    Thread,
    Thread,
    ThreadRef
);
typed_lookup!(
    /// The process a capability designates.
    process,
    Process,
    Process,
    ProcessRef
);
typed_lookup!(
    /// The file a capability designates.
    file,
    File,
    File,
    FileRef
);
typed_lookup!(
    /// The directory a capability designates.
    directory,
    Directory,
    Directory,
    FileRef
);

/// Takes the object named `id` out of the registry, with its charge.
fn remove(id: ObjectId) -> Option<Entry> {
    let mut registry = REGISTRY.lock();
    let slot = registry.as_mut()?.get_mut(id.index as usize)?;
    if slot.as_ref().is_some_and(|entry| entry.generation == id.generation) {
        slot.take()
    } else {
        None
    }
}

/// The store released an object: it leaves the registry and is closed.
/// Called with the store locked, so the store's version moves after the
/// change.
pub fn released(released: &Released) {
    let Some(Entry { object, charge, .. }) = remove(released.id) else { return };
    match object {
        Object::Endpoint(endpoint) => endpoint.close(),
        Object::Notification(notification) => notification.close(),
        Object::Frame(frame) => super::memory::FrameObject::close(&frame),
        Object::Ring(ring) => super::ring::RingObject::close(&ring),
        // With its last capability gone, nobody can control the thread:
        // it ends at its next return to user mode.
        Object::Thread(thread) => {
            thread.request_stop();
            crate::sched::wake(&thread);
        }
        // It ends; with no thread left to finish it, the store does once
        // its change is done (`process::finish_pending`).
        Object::Process(process) => super::process::released(process),
        // It may be the last reference to a file removed while open,
        // whose space is freed on the disk when it goes: that waits until
        // the store's lock has gone (`fs::drop_deferred`).
        Object::File(file) | Object::Directory(file) => crate::fs::defer_drop(file),
        Object::Space(_) | Object::Random | Object::KernelLog => {}
    }
    // What it was charged is given back once it has closed.
    drop(charge);
}

/// The numbers of the processes the registry holds, for `/proc`.
pub fn process_numbers() -> Vec<u64, Kmalloc> {
    let mut numbers = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    let registry = REGISTRY.lock();
    let Some(table) = registry.as_ref() else { return numbers };
    let count = table.iter().flatten().filter(|e| matches!(e.object, Object::Process(_))).count();
    if numbers.try_reserve_exact(count).is_err() {
        return numbers;
    }
    for entry in table.iter().flatten() {
        if let Object::Process(process) = &entry.object {
            let _ = numbers.push_within_capacity(process.number());
        }
    }
    numbers
}

/// The process numbered `number`, if the registry holds it.
pub fn process_by_number(number: u64) -> Option<ProcessRef> {
    let registry = REGISTRY.lock();
    registry.as_ref()?.iter().flatten().find_map(|entry| match &entry.object {
        Object::Process(process) if process.number() == number => Some(process.clone()),
        _ => None,
    })
}

/// How many objects of each kind the registry holds, for `kdb`. Gives up
/// if the registry is locked.
pub fn census() -> Option<[u32; radix_cap::KINDS]> {
    let registry = REGISTRY.try_lock()?;
    let mut counts = [0; radix_cap::KINDS];
    for entry in registry.as_ref()?.iter().flatten() {
        counts[entry.object.kind().index()] += 1;
    }
    Some(counts)
}
