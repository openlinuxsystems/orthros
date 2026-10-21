// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
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

use alloc::vec::Vec;

use radix_abi::errors::Kernel;
use radix_cap::{Capability, ObjectId, ObjectKind, Released, Rights, SlotRef, SpaceId};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::Mutex;

use super::ipc::EndpointRef;
use super::memory::{FrameRef, SpaceRef};
use super::notify::NotificationRef;
use super::ring::RingRef;
use super::{Result, cap_error, error};
use crate::caps;
use crate::sched::ThreadRef;

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
    /// The kernel's random number generator.
    Random,
    /// The kernel log.
    KernelLog,
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
            Object::Random => ObjectKind::Random,
            Object::KernelLog => ObjectKind::KernelLog,
        }
    }
}

struct Entry {
    generation: u32,
    object: Object,
}

static REGISTRY: Mutex<Option<Vec<Option<Entry>, Kmalloc>>> = Mutex::new(None);

/// Sets up the registry.
pub fn init() {
    *REGISTRY.lock() = Some(Vec::new_in(HEAP.with(Flags::MAY_SLEEP)));
}

/// The registry is out of memory.
#[derive(Clone, Copy, Debug)]
pub struct NoMemory;

/// Makes room for `count` more objects past the highest index now used,
/// so that inserting them cannot fail.
pub fn reserve(count: usize) -> core::result::Result<(), NoMemory> {
    let mut registry = REGISTRY.lock();
    let Some(table) = registry.as_mut() else { return Err(NoMemory) };
    table.try_reserve(count).map_err(|_| NoMemory)
}

/// Records `object` under `id`.
pub fn insert(id: ObjectId, object: Object) -> core::result::Result<(), NoMemory> {
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
    table[index] = Some(Entry { generation: id.generation, object });
    Ok(())
}

/// The object named `id`, if it exists.
pub fn get(id: ObjectId) -> Option<Object> {
    let registry = REGISTRY.lock();
    let entry = registry.as_ref()?.get(id.index as usize)?.as_ref()?;
    (entry.generation == id.generation).then(|| entry.object.clone())
}

/// The capability in slot `slot` of `cspace`, if it may be used on a
/// `kind` with the rights `needs`, and the object it designates.
pub fn lookup(
    cspace: SpaceId,
    slot: u32,
    kind: ObjectKind,
    needs: Rights,
) -> Result<(Capability, Object)> {
    let cap = caps::with(|store| store.lookup(SlotRef::new(cspace, slot), kind, needs))
        .and_then(|found| found)
        .map_err(cap_error)?;
    // Released since the lookup: as good as gone.
    let object = cap.id().and_then(get).ok_or(error(Kernel::Gone))?;
    Ok((cap, object))
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

/// Takes the object named `id` out of the registry.
fn remove(id: ObjectId) -> Option<Object> {
    let mut registry = REGISTRY.lock();
    let slot = registry.as_mut()?.get_mut(id.index as usize)?;
    if slot.as_ref().is_some_and(|entry| entry.generation == id.generation) {
        slot.take().map(|entry| entry.object)
    } else {
        None
    }
}

/// The store released an object: it leaves the registry and is closed.
/// Called with the store locked.
pub fn released(released: &Released) {
    let Some(object) = remove(released.id) else { return };
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
        Object::Space(_) | Object::Random | Object::KernelLog => {}
    }
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
