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
//! The registry is guarded by the store's lock (`caps::Locked`): it
//! changes only with the store, so it has no lock of its own, and making
//! or releasing an object takes one lock, not two. A system call
//! finds a capability and the object it designates with one look at its
//! CPU's cache, which takes no lock when this CPU found that slot before
//! at the version the store has now ([`find`]): system calls on
//! different CPUs share no lock to reach their objects. A capability a
//! CPU found leaving its slot, or an object leaving the registry, moves
//! the version (`caps`); one nobody looked up, such as a file opened and
//! closed again, does not.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use radix_abi::errors::Kernel;
use radix_cap::{Capability, ObjectId, ObjectKind, Released, Rights, SlotRef, SpaceId};
use radix_hal::MAX_CPUS;
use radix_mm::{Flags, HEAP, Kmalloc};

use super::account::Charge;
use super::ipc::{Endpoint, EndpointRef};
use super::memory::{FrameObject, FrameRef, SpaceObject, SpaceRef};
use super::notify::{Notification, NotificationRef};
use super::process::{ProcessObject, ProcessRef};
use super::ring::{RingObject, RingRef};
use super::{Result, cap_error, error};
use crate::caps::{self, Locked};
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
    /// The machine's power (radix-power(2)).
    Power,
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
            Object::Power => ObjectKind::Power,
        }
    }
}

struct Entry {
    generation: u32,
    /// A CPU's cache may hold it ([`get_to_cache`]): taking it out moves
    /// the store's version.
    cached: bool,
    object: Object,
    /// What the object costs, charged to the account of the process that
    /// made it (`process.create`), and given back when it leaves.
    charge: Option<Charge>,
}

/// The registry: an entry for each object the store names, at the index of
/// its name. It lives with the store, under the store's lock.
pub struct Registry {
    table: Vec<Option<Entry>, Kmalloc>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Registry {
        Registry { table: Vec::new_in(HEAP.with(Flags::MAY_SLEEP)) }
    }
}

/// The registry is out of memory.
#[derive(Clone, Copy, Debug)]
pub struct NoMemory;

/// Makes room for the next `count` objects the store makes, whatever their
/// indexes (`Store::object_records`), so that inserting them cannot fail:
/// nothing else takes the room first while the store stays locked.
pub fn reserve(locked: &mut Locked, count: usize) -> core::result::Result<(), NoMemory> {
    let needed = locked.store.object_records().saturating_add(count);
    let table = &mut locked.registry.table;
    table.try_reserve(needed.saturating_sub(table.len())).map_err(|_| NoMemory)
}

/// Records `object` under `id`, with the store locked, so that the store's
/// version moves after the change.
pub fn insert(
    locked: &mut Locked,
    id: ObjectId,
    object: Object,
) -> core::result::Result<(), NoMemory> {
    insert_charged(locked, id, object, None)
}

/// Records `object` under `id`, with what it was charged, which is given
/// back when it leaves the registry.
pub fn insert_charged(
    locked: &mut Locked,
    id: ObjectId,
    object: Object,
    charge: Option<Charge>,
) -> core::result::Result<(), NoMemory> {
    let table = &mut locked.registry.table;
    let index = id.index as usize;
    if index >= table.len() {
        table.try_reserve(index + 1 - table.len()).map_err(|_| NoMemory)?;
        while table.len() <= index {
            // Room was reserved above.
            let _ = table.push_within_capacity(None);
        }
    }
    replace(table, index, Entry { generation: id.generation, cached: false, object, charge });
    Ok(())
}

/// Puts `entry` in the registry's slot `index`, which is normally empty;
/// if a CPU may have cached what it held, the version moves.
fn replace(table: &mut [Option<Entry>], index: usize, entry: Entry) {
    if table[index].replace(entry).is_some_and(|old| old.cached) {
        caps::mark_stale();
    }
}

/// Makes an object of `kind` in the store, with its original capability,
/// holding only `rights`, in `at`, and records `object` under its name
/// with what it was charged: what [`reserve`], `Store::create_with_rights`
/// and [`insert_charged`] do apart. `object` is taken only once nothing
/// can fail; on a failure it is left to the caller, to drop once the
/// store's lock has gone, since it may hold the last reference to
/// something whose drop takes locks (a removed file frees its space on
/// the disk).
pub fn create_with_rights(
    locked: &mut Locked,
    at: SlotRef,
    kind: ObjectKind,
    rights: Rights,
    object: &mut Option<Object>,
    charge: Option<Charge>,
) -> Result<()> {
    let Locked { store, registry } = locked;
    let table = &mut registry.table;
    let needed = store.object_records().saturating_add(1);
    table.try_reserve(needed.saturating_sub(table.len())).map_err(|_| error(Kernel::NoMemory))?;
    // Taken before the capability is made, which then always has its
    // object in the registry.
    let Some(taken) = object.take() else { return Err(error(Kernel::Gone)) };
    let id = match store.create_with_rights(at, kind, rights) {
        Ok(id) => id,
        Err(err) => {
            *object = Some(taken);
            return Err(cap_error(err));
        }
    };
    let index = id.index as usize;
    while table.len() <= index {
        // Room was reserved above, for every index the store can give.
        let _ = table.push_within_capacity(None);
    }
    let object = taken;
    replace(table, index, Entry { generation: id.generation, cached: false, object, charge });
    Ok(())
}

/// The object named `id`, if it is in the registry, looked up under the
/// store's lock: for lookups too rare to be worth a place in a CPU's
/// cache. The system calls' lookups go through [`find`].
pub fn get(id: ObjectId) -> Option<Object> {
    caps::with_registry(|registry| {
        let entry = registry.table.get(id.index as usize)?.as_ref()?;
        (entry.generation == id.generation).then(|| entry.object.clone())
    })?
}

/// The object named `id`, for a CPU's cache to keep: from now on the
/// version moves once it leaves the registry.
fn get_to_cache(registry: &mut Registry, id: ObjectId) -> Option<Object> {
    let entry = registry.table.get_mut(id.index as usize)?.as_mut()?;
    if entry.generation != id.generation {
        return None;
    }
    entry.cached = true;
    Some(entry.object.clone())
}

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
    Power,
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
            Object::Power => Kept::Power,
        }
    }

    #[inline(always)]
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
            Kept::Power => Object::Power,
        })
    }
}

/// Capabilities a CPU keeps, with their objects, in a table indexed by
/// slot.
const CACHED: usize = 16;

/// A capability this CPU found in a slot, the object it designates, and
/// the store's version then.
struct Cached {
    at: SlotRef,
    version: u64,
    cap: Capability,
    /// `None` if the capability designates nothing in the registry, as a
    /// memory capability does.
    object: Option<Kept>,
}

/// Each CPU's capabilities, which only its own CPU uses.
static CACHE: PerCpu<Own<[Option<Cached>; CACHED]>> =
    PerCpu::new([const { Slot::new(Own::new([const { None }; CACHED])) }; MAX_CPUS]);

fn cache_index(at: SlotRef) -> usize {
    (at.slot as usize ^ at.space.index as usize) % CACHED
}

/// A capability, and the object it designates, as [`find`] finds them.
pub struct Found {
    /// The capability.
    pub cap: Capability,
    /// The object it designates, if the caller wanted it and the registry
    /// holds it, unless `held`.
    pub object: Option<Object>,
    /// Whether the capability designates the process the caller said it
    /// holds ([`find_holding`]), which `object` then leaves out.
    pub held: bool,
}

/// The capability in slot `slot` of `cspace`, and the object it
/// designates if `wanted` says its kind's is needed, found in `place`.
///
/// If this CPU found the capability at the version the store has now, it
/// has not left the slot since, nor its object the registry, and nothing
/// else changes either: no lock, and nothing shared written but the
/// object's reference count. A change under way that has not moved the
/// version yet has not returned either, so a lookup that misses it
/// happened before it. Otherwise the store and the registry are asked
/// under their locks (`caps::get_to_cache`, [`get_to_cache`], so that the
/// version moves once either goes), and the answer is kept with the
/// version, which cannot move while the store's lock is held.
///
/// What is found is written in the caller's `place`, not returned: a
/// capability and an object are 88 bytes, which a move would copy with a
/// string move, whose start-up costs more than the lookup.
#[inline(always)]
pub fn find(
    cspace: SpaceId,
    slot: u32,
    wanted: impl Fn(ObjectKind) -> bool,
    place: &mut Option<Found>,
) -> Result<&mut Found> {
    find_holding(cspace, slot, wanted, None, place)
}

/// As [`find`], for a caller that holds a reference to `process`: if this
/// CPU's cache finds the capability designating it, the lookup takes no
/// second reference (two atomic read-modify-writes on a line every
/// thread of the process shares) but says it is the one held, in
/// [`Found::held`]. A POSIX program's system calls are methods of its own
/// process.
///
/// That is sound because the reference keeps the process's memory where it
/// is: the cache's weak reference to the same address is to the same
/// object, which the capability designated at the version the store has
/// now.
#[inline(always)]
pub fn find_holding<'a>(
    cspace: SpaceId,
    slot: u32,
    wanted: impl Fn(ObjectKind) -> bool,
    process: Option<&ProcessRef>,
    place: &'a mut Option<Found>,
) -> Result<&'a mut Found> {
    let at = SlotRef::new(cspace, slot);
    let now = caps::version();
    let index = cache_index(at);
    'cached: {
        let Some(cache) = CACHE.own() else { break 'cached };
        let Some(entry) = &cache[index] else { break 'cached };
        if entry.at != at || entry.version != now {
            break 'cached;
        }
        let object = match &entry.object {
            Some(Kept::Process(kept))
                if process.is_some_and(|held| core::ptr::eq(kept.as_ptr(), Arc::as_ptr(held)))
                    && wanted(entry.cap.kind()) =>
            {
                return Ok(place.insert(Found { cap: entry.cap, object: None, held: true }));
            }
            Some(kept) if wanted(entry.cap.kind()) => match kept.upgrade() {
                Some(object) => Some(object),
                // Gone, so the version is about to move: ask the store.
                None => break 'cached,
            },
            _ => None,
        };
        return Ok(place.insert(Found { cap: entry.cap, object, held: false }));
    }
    let found = place.insert(fill(at, index)?);
    if !wanted(found.cap.kind()) {
        found.object = None;
    }
    Ok(found)
}

/// The object the capability in slot `slot` of `cspace` designates, if
/// the registry holds one: [`find`] for a caller that wants only the
/// object, which leaves the capability where it is rather than copy it.
#[inline(always)]
pub fn find_object(cspace: SpaceId, slot: u32) -> Option<Object> {
    let at = SlotRef::new(cspace, slot);
    let now = caps::version();
    let index = cache_index(at);
    'cached: {
        let Some(cache) = CACHE.own() else { break 'cached };
        let Some(entry) = &cache[index] else { break 'cached };
        if entry.at != at || entry.version != now {
            break 'cached;
        }
        return match &entry.object {
            Some(kept) => match kept.upgrade() {
                Some(object) => Some(object),
                // Gone, so the version is about to move: ask the store.
                None => break 'cached,
            },
            None => None,
        };
    }
    fill(at, index).ok()?.object
}

/// [`find`] when this CPU's cache cannot answer: kept apart, so that a
/// hit needs neither its registers nor its stack.
#[cold]
#[inline(never)]
fn fill(at: SlotRef, index: usize) -> Result<Found> {
    let (cap, object, version) =
        caps::get_to_cache(at, |cap, registry| cap.id().and_then(|id| get_to_cache(registry, id)))
            .map_err(cap_error)?;
    let kept = Cached { at, version, cap, object: object.as_ref().map(Kept::of) };
    // What the entry held before goes once preemption is back on: it may
    // be the last reference to an object's memory.
    let before = CACHE.with_own(|cache| cache[index].replace(kept));
    drop(before);
    Ok(Found { cap, object, held: false })
}

/// The capability in slot `slot` of `cspace`, if it may be used on a
/// `kind` with the rights `needs`, and the object it designates.
pub fn lookup(
    cspace: SpaceId,
    slot: u32,
    kind: ObjectKind,
    needs: Rights,
) -> Result<(Capability, Object)> {
    let mut place = None;
    let found = find(cspace, slot, |of| of == kind, &mut place)?;
    found.cap.check(kind, needs).map_err(cap_error)?;
    // Released since the capability was found: as good as gone.
    let object = found.object.take().ok_or(error(Kernel::Gone))?;
    Ok((found.cap, object))
}

/// The capability in slot `slot` of `cspace`, whatever it designates.
pub fn capability(cspace: SpaceId, slot: u32) -> Result<Capability> {
    let mut place = None;
    Ok(find(cspace, slot, |_| false, &mut place)?.cap)
}

/// The object `cap`, looked up a moment ago, designates, through [`get`].
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
fn remove(registry: &mut Registry, id: ObjectId) -> Option<Entry> {
    let slot = registry.table.get_mut(id.index as usize)?;
    if slot.as_ref().is_some_and(|entry| entry.generation == id.generation) {
        let entry = slot.take();
        if entry.as_ref().is_some_and(|entry| entry.cached) {
            caps::mark_stale();
        }
        entry
    } else {
        None
    }
}

/// The store released an object: it leaves the registry and is closed.
/// Called with the store locked, so the store's version moves after the
/// change.
pub fn released(registry: &mut Registry, released: &Released) {
    let Some(Entry { object, charge, .. }) = remove(registry, released.id) else { return };
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
        // the store's lock has gone (`fs::release`, `fs::drop_deferred`).
        Object::File(file) | Object::Directory(file) => crate::fs::release(file),
        Object::Space(_) | Object::Random | Object::KernelLog | Object::Power => {}
    }
    // What it was charged is given back once it has closed.
    drop(charge);
}

/// The numbers of the processes the registry holds, for `/processes`.
pub fn process_numbers() -> Vec<u64, Kmalloc> {
    let mut numbers = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    caps::with_registry(|registry| {
        let table = &registry.table;
        let count =
            table.iter().flatten().filter(|e| matches!(e.object, Object::Process(_))).count();
        if numbers.try_reserve_exact(count).is_err() {
            return;
        }
        for entry in table.iter().flatten() {
            if let Object::Process(process) = &entry.object {
                let _ = numbers.push_within_capacity(process.number());
            }
        }
    });
    numbers
}

/// The process numbered `number`, if the registry holds it.
pub fn process_by_number(number: u64) -> Option<ProcessRef> {
    caps::with_registry(|registry| {
        registry.table.iter().flatten().find_map(|entry| match &entry.object {
            Object::Process(process) if process.number() == number => Some(process.clone()),
            _ => None,
        })
    })?
}

/// How many objects of each kind `registry` holds, for `kdb` and the
/// user tests, which read it with the store's own census in one hold.
pub fn census(registry: &Registry) -> [u32; radix_cap::KINDS] {
    let mut counts = [0; radix_cap::KINDS];
    for entry in registry.table.iter().flatten() {
        counts[entry.object.kind().index()] += 1;
    }
    counts
}
