// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The capability store: capability spaces, the object table and the
//! derivation tree.
//!
//! A capability space (CSpace) is an array of slots; a process will name a
//! capability by its slot number and nothing else. Every capability in
//! every space is a node of one derivation tree: a capability made from
//! another is that one's child, wherever the two are kept. Each slot holds
//! its links (parent, first child, previous and next sibling), so the tree
//! costs nothing to walk and needs no memory beyond the slots.
//!
//! - Deleting a capability makes its children children of its parent, so
//!   whoever could revoke the deleted one can still revoke them.
//! - Revoking a capability deletes everything derived from it, in every
//!   space, and keeps the capability itself. It works in steps of a given
//!   size, so the kernel can let go of its lock in between; while a revoke
//!   is under way, nothing can be derived from the capability or from
//!   anything below it, so the work only shrinks and always ends.
//! - Moving a capability keeps its place in the tree.
//!
//! Kernel objects (every kind but the hardware ranges: interrupt lines,
//! I/O ports and device registers) have an entry in the object table
//! that counts the capabilities naming them and the objects retyped from
//! them. When both counts reach zero the object is gone, and the store
//! tells the caller through a [`Released`], so the kernel can give its
//! memory back.

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::cap::{Capability, Derivation, carve};
use crate::error::CapError;
use crate::object::{KINDS, Object, ObjectId, ObjectKind, Target};
use crate::rights::Rights;

/// The longest chain of derivations: a capability is at most this many
/// steps from one the kernel made. It bounds every walk up the tree.
pub const MAX_DEPTH: u8 = 64;

/// The most slots a capability space can have.
pub const MAX_SLOTS: u32 = 1 << 16;

/// The most objects one retype can make.
pub const MAX_RETYPE: u32 = 1024;

/// The bytes each slot of a capability space takes, for charging spaces
/// to whoever asked for them.
pub const SLOT_BYTES: usize = core::mem::size_of::<Slot>();

/// The name of a capability space: its index in the store and that
/// index's generation, so a name stays wrong once the space is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SpaceId {
    /// Index in the store.
    pub index: u32,
    /// Generation of the index when the space was made.
    pub generation: u32,
}

impl fmt::Display for SpaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "space {}.{}", self.index, self.generation)
    }
}

/// A slot in a capability space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlotRef {
    /// The space.
    pub space: SpaceId,
    /// The slot's number in it.
    pub slot: u32,
}

impl SlotRef {
    /// Slot `slot` of `space`.
    pub const fn new(space: SpaceId, slot: u32) -> SlotRef {
        SlotRef { space, slot }
    }
}

impl fmt::Display for SlotRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} slot {}", self.space, self.slot)
    }
}

/// Whether a revoke finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Nothing derived from the capability is left.
    Done,
    /// The step's budget ran out; call again to go on.
    More,
}

/// A range of page frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// The first page frame number.
    pub base: u64,
    /// How many pages.
    pub pages: u64,
}

/// A kernel object whose last capability is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Released {
    /// Its kind.
    pub kind: ObjectKind,
    /// Its name, which no longer refers to anything.
    pub id: ObjectId,
    /// For memory the kernel gave the store ([`Capabilities::create_memory`]),
    /// the pages to take back. Objects retyped from memory return their
    /// pages to that memory instead.
    pub memory: Option<Extent>,
}

/// What the store knows about a kernel object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Its kind.
    pub kind: ObjectKind,
    /// Capabilities that name it.
    pub caps: u32,
    /// Objects retyped from it that still exist.
    pub children: u32,
    /// The memory it covers, for memory, frames and DMA buffers.
    pub extent: Option<Extent>,
    /// For memory: how many pages from its start are given to objects.
    pub watermark: u64,
    /// The memory object it was retyped from.
    pub origin: Option<ObjectId>,
}

/// Counts of what the store holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    /// Capability spaces.
    pub spaces: u32,
    /// Slots in all spaces.
    pub slots: u64,
    /// Capabilities, by the kind of object they designate.
    pub caps: [u64; KINDS],
    /// Kernel objects, by kind.
    pub objects: [u64; KINDS],
    /// Pages of memory the kernel gave the store.
    pub memory_pages: u64,
    /// The deepest capability's depth.
    pub deepest: u8,
    /// Revokes under way.
    pub revoking: u32,
}

impl Census {
    /// All capabilities.
    pub fn total_caps(&self) -> u64 {
        self.caps.iter().sum()
    }

    /// All kernel objects.
    pub fn total_objects(&self) -> u64 {
        self.objects.iter().sum()
    }
}

/// A broken invariant that [`Capabilities::audit`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inconsistency {
    /// What is wrong.
    pub what: &'static str,
    /// Where, if it is about a slot.
    pub at: Option<SlotRef>,
}

impl fmt::Display for Inconsistency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.at {
            Some(at) => write!(f, "{} ({at})", self.what),
            None => f.write_str(self.what),
        }
    }
}

/// A slot of a space by index, inside the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Link {
    space: u32,
    slot: u32,
}

/// A slot's place among its relatives in the derivation tree.
#[derive(Clone, Copy, Debug)]
struct Links {
    parent: Option<Link>,
    child: Option<Link>,
    prev: Option<Link>,
    next: Option<Link>,
}

/// One slot: a capability and its place in the derivation tree.
#[derive(Clone, Copy, Debug)]
struct Slot {
    cap: Option<Capability>,
    parent: Option<Link>,
    child: Option<Link>,
    prev: Option<Link>,
    next: Option<Link>,
    /// More than the parent's depth; at most [`MAX_DEPTH`]. Deleting a
    /// capability gives its children a parent nearer the root, so this is
    /// an upper bound on the distance to a root.
    depth: u8,
    /// A revoke of this capability is under way.
    revoking: bool,
    /// The slot's own flags (Phase 8), which stay with the slot rather
    /// than the capability: the kernel's close-on-exec. Clear in an empty
    /// slot, and in one a capability was just put in.
    flags: u8,
    /// Its capability may be in a cache outside the store: it was looked
    /// up with [`Capabilities::get_to_cache`]. Taking it out of the slot
    /// then makes the store stale ([`Capabilities::take_stale`]).
    cached: bool,
}

impl Slot {
    const EMPTY: Slot = Slot {
        cap: None,
        parent: None,
        child: None,
        prev: None,
        next: None,
        depth: 0,
        revoking: false,
        flags: 0,
        cached: false,
    };

    /// Makes it [`Slot::EMPTY`] in place, a field at a time: storing a
    /// whole slot is a string move.
    fn clear(&mut self) {
        let Slot { cap, parent, child, prev, next, depth, revoking, flags, cached } = self;
        *cap = None;
        *parent = None;
        *child = None;
        *prev = None;
        *next = None;
        *depth = 0;
        *revoking = false;
        *flags = 0;
        *cached = false;
    }
}

struct Space<A: Allocator> {
    generation: u32,
    live: bool,
    slots: Vec<Slot, A>,
    /// Next free entry, while this one is free.
    next_free: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    kind: ObjectKind,
    caps: u32,
    children: u32,
    extent: Option<Extent>,
    watermark: u64,
    origin: Option<ObjectId>,
}

#[derive(Clone, Copy, Debug)]
struct Record {
    generation: u32,
    entry: Option<Entry>,
    /// Next free record, while this one is free.
    next_free: Option<u32>,
}

/// Every capability space, the object table and the derivation tree.
///
/// Every operation takes `&mut self`; the kernel keeps the store behind
/// one lock. Operations that delete capabilities report the objects that
/// went with them to a `released` callback, which runs with the store
/// borrowed and must not call back into it.
pub struct Capabilities<A: Allocator + Clone> {
    alloc: A,
    spaces: Vec<Space<A>, A>,
    free_space: Option<u32>,
    objects: Vec<Record, A>,
    free_object: Option<u32>,
    /// A capability looked up with [`Capabilities::get_to_cache`] has
    /// left its slot since [`Capabilities::take_stale`] last asked.
    stale: bool,
}

/// The next generation number, skipping 0.
fn bump(generation: u32) -> u32 {
    generation.wrapping_add(1).max(1)
}

impl<A: Allocator + Clone> Capabilities<A> {
    /// An empty store whose memory comes from `alloc`.
    pub fn new(alloc: A) -> Self {
        Capabilities {
            spaces: Vec::new_in(alloc.clone()),
            free_space: None,
            objects: Vec::new_in(alloc.clone()),
            free_object: None,
            stale: false,
            alloc,
        }
    }

    // ---- Spaces ----

    /// A new capability space with `slots` empty slots (1 to [`MAX_SLOTS`]).
    pub fn create_space(&mut self, slots: u32) -> Result<SpaceId, CapError> {
        if slots == 0 || slots > MAX_SLOTS {
            return Err(CapError::InvalidArgument);
        }
        let mut array = Vec::new_in(self.alloc.clone());
        array.try_reserve_exact(slots as usize).map_err(|_| CapError::NoMemory)?;
        for _ in 0..slots {
            // Cannot fail: the capacity was reserved.
            let _ = array.push_within_capacity(Slot::EMPTY);
        }
        let index = match self.free_space {
            Some(index) => {
                let space = &mut self.spaces[index as usize];
                self.free_space = space.next_free.take();
                space.slots = array;
                space.live = true;
                index
            }
            None => {
                let index = u32::try_from(self.spaces.len()).map_err(|_| CapError::NoMemory)?;
                self.spaces.try_reserve(1).map_err(|_| CapError::NoMemory)?;
                let space = Space { generation: 1, live: true, slots: array, next_free: None };
                // Cannot fail: the capacity was reserved.
                let _ = self.spaces.push_within_capacity(space);
                index
            }
        };
        Ok(SpaceId { index, generation: self.spaces[index as usize].generation })
    }

    /// Deletes every capability in `space` (as [`Capabilities::delete`]
    /// does, so capabilities derived from them elsewhere stay, under the
    /// deleted ones' parents) and frees the space.
    pub fn destroy_space(
        &mut self,
        space: SpaceId,
        released: &mut dyn FnMut(Released),
    ) -> Result<(), CapError> {
        let len = self.space(space)?.slots.len() as u32;
        for slot in 0..len {
            let link = Link { space: space.index, slot };
            if self.slot(link).cap.is_some() {
                self.remove(link, released);
            }
        }
        let entry = &mut self.spaces[space.index as usize];
        entry.slots = Vec::new_in(self.alloc.clone());
        entry.live = false;
        entry.generation = bump(entry.generation);
        entry.next_free = self.free_space;
        self.free_space = Some(space.index);
        Ok(())
    }

    /// How many slots `space` has.
    pub fn slots(&self, space: SpaceId) -> Result<u32, CapError> {
        Ok(self.space(space)?.slots.len() as u32)
    }

    /// Grows `space` to `slots` empty slots in all (at most
    /// [`MAX_SLOTS`]); a space never shrinks, so asking for fewer than it
    /// has changes nothing. Returns how many it has.
    pub fn grow_space(&mut self, space: SpaceId, slots: u32) -> Result<u32, CapError> {
        if slots > MAX_SLOTS {
            return Err(CapError::InvalidArgument);
        }
        // Checked before the space is borrowed mutably.
        self.space(space)?;
        let array = &mut self.spaces[space.index as usize].slots;
        let more = (slots as usize).saturating_sub(array.len());
        array.try_reserve_exact(more).map_err(|_| CapError::NoMemory)?;
        for _ in 0..more {
            // Cannot fail: the capacity was reserved.
            let _ = array.push_within_capacity(Slot::EMPTY);
        }
        Ok(array.len() as u32)
    }

    /// The flags of the slot `at`, which must hold a capability.
    pub fn slot_flags(&self, at: SlotRef) -> Result<u8, CapError> {
        let link = self.full(at)?;
        Ok(self.slot(link).flags)
    }

    /// Sets the flags of the slot `at`, which must hold a capability, and
    /// returns them as they were.
    pub fn set_slot_flags(&mut self, at: SlotRef, flags: u8) -> Result<u8, CapError> {
        let link = self.full(at)?;
        Ok(core::mem::replace(&mut self.slot_mut(link).flags, flags))
    }

    /// The first slot of `space` from `from` on that holds a capability
    /// and has any of the flags in `mask`, if one does.
    pub fn find_flagged(
        &self,
        space: SpaceId,
        mask: u8,
        from: u32,
    ) -> Result<Option<u32>, CapError> {
        let slots = &self.space(space)?.slots;
        let found = slots
            .iter()
            .enumerate()
            .skip(from as usize)
            .find(|(_, slot)| slot.cap.is_some() && slot.flags & mask != 0);
        Ok(found.map(|(index, _)| index as u32))
    }

    /// Which of the 64 slots of `space` from `first` on hold a
    /// capability, one bit each from bit 0; slots past the end count as
    /// empty.
    pub fn used_slots(&self, space: SpaceId, first: u32) -> Result<u64, CapError> {
        let slots = &self.space(space)?.slots;
        let mut used = 0u64;
        for (bit, slot) in slots.iter().skip(first as usize).take(64).enumerate() {
            if slot.cap.is_some() {
                used |= 1 << bit;
            }
        }
        Ok(used)
    }

    // ---- Looking capabilities up ----

    /// The capability in `at`.
    pub fn get(&self, at: SlotRef) -> Result<Capability, CapError> {
        let link = self.link(at)?;
        self.slot(link).cap.ok_or(CapError::Empty)
    }

    /// The capability in `at`, as [`Capabilities::get`] gives it, for a
    /// cache outside the store to keep: from now on, the store is stale
    /// once it leaves the slot (deleted, revoked, moved, or its space
    /// destroyed), until [`Capabilities::take_stale`] says so. Nothing
    /// else changes a capability in a slot, so until then the cached copy
    /// is what the slot holds.
    pub fn get_to_cache(&mut self, at: SlotRef) -> Result<Capability, CapError> {
        let link = self.link(at)?;
        let slot = self.slot_mut(link);
        let cap = slot.cap.ok_or(CapError::Empty)?;
        slot.cached = true;
        Ok(cap)
    }

    /// Whether a capability looked up with [`Capabilities::get_to_cache`]
    /// has left its slot since the last call: if so, whatever cache holds
    /// it must forget it. Asking clears the answer.
    pub fn take_stale(&mut self) -> bool {
        core::mem::take(&mut self.stale)
    }

    /// The capability in `at`, if it may be used on a `kind` with the
    /// rights `needs` ([`Capability::check`]). Every use of a capability
    /// goes through this.
    pub fn lookup(
        &self,
        at: SlotRef,
        kind: ObjectKind,
        needs: Rights,
    ) -> Result<Capability, CapError> {
        let cap = self.get(at)?;
        cap.check(kind, needs)?;
        Ok(cap)
    }

    /// How many steps `at` is from a capability the kernel made, at most.
    pub fn depth(&self, at: SlotRef) -> Result<u8, CapError> {
        let link = self.full(at)?;
        Ok(self.slot(link).depth)
    }

    /// The slot of the capability `at` was derived from, if it was.
    pub fn parent(&self, at: SlotRef) -> Result<Option<SlotRef>, CapError> {
        let link = self.full(at)?;
        Ok(self.slot(link).parent.map(|parent| self.slot_ref(parent)))
    }

    /// Whether a revoke of `at` is under way.
    pub fn is_revoking(&self, at: SlotRef) -> Result<bool, CapError> {
        let link = self.full(at)?;
        Ok(self.slot(link).revoking)
    }

    /// What the store knows about the kernel object `id`.
    pub fn object(&self, id: ObjectId) -> Option<ObjectInfo> {
        let entry = self.entry(id)?;
        Some(ObjectInfo {
            kind: entry.kind,
            caps: entry.caps,
            children: entry.children,
            extent: entry.extent,
            watermark: entry.watermark,
            origin: entry.origin,
        })
    }

    /// How many records the object table has, in use or free. Every
    /// object's index is below it, and `n` objects made from now on have
    /// indexes below it plus `n`: a table kept beside this one, indexed the
    /// same way, needs no more room than that.
    pub fn object_records(&self) -> usize {
        self.objects.len()
    }

    // ---- Making objects (the kernel only) ----

    /// Makes a new kernel object of `kind` (an address space, thread,
    /// process, endpoint, notification or resource domain) and puts its original
    /// capability, with every right of its kind, in `at`.
    pub fn create(&mut self, at: SlotRef, kind: ObjectKind) -> Result<ObjectId, CapError> {
        self.create_with_rights(at, kind, kind.rights())
    }

    /// Makes a new kernel object of `kind`, as [`Capabilities::create`]
    /// does, but with only `rights` (those of them its kind has) on its
    /// original capability: for an object the kernel makes on a holder's
    /// behalf, which must get no more than the holder may have (a file a
    /// directory capability opens).
    pub fn create_with_rights(
        &mut self,
        at: SlotRef,
        kind: ObjectKind,
        rights: Rights,
    ) -> Result<ObjectId, CapError> {
        if kind.is_range() || kind.from_memory() {
            return Err(CapError::InvalidArgument);
        }
        let link = self.empty(at)?;
        let id = self.new_object(Entry {
            kind,
            caps: 1,
            children: 0,
            extent: None,
            watermark: 0,
            origin: None,
        })?;
        let mut cap = Capability::original(Object { kind, target: Target::Object(id) });
        cap.rights = rights.intersection(kind.rights());
        self.place(link, cap, None);
        Ok(id)
    }

    /// Makes a memory object for `pages` pages from page frame `base`,
    /// which the kernel hands over until the store releases them, and puts
    /// its original capability in `at`.
    pub fn create_memory(
        &mut self,
        at: SlotRef,
        base: u64,
        pages: u64,
    ) -> Result<ObjectId, CapError> {
        if pages == 0 || base.checked_add(pages).is_none_or(|end| end > 1 << 40) {
            return Err(CapError::InvalidArgument);
        }
        let link = self.empty(at)?;
        let id = self.new_object(Entry {
            kind: ObjectKind::Memory,
            caps: 1,
            children: 0,
            extent: Some(Extent { base, pages }),
            watermark: 0,
            origin: None,
        })?;
        let cap =
            Capability::original(Object { kind: ObjectKind::Memory, target: Target::Object(id) });
        self.place(link, cap, None);
        Ok(id)
    }

    /// Puts in `at` an original capability for `len` interrupt lines, I/O
    /// ports or pages of device registers from `base`.
    pub fn grant_range(
        &mut self,
        at: SlotRef,
        kind: ObjectKind,
        base: u64,
        len: u64,
    ) -> Result<(), CapError> {
        let Some(limit) = kind.range_limit() else {
            return Err(CapError::NotARange);
        };
        if len == 0 || base.checked_add(len).is_none_or(|end| end > limit) {
            return Err(CapError::OutOfRange);
        }
        let link = self.empty(at)?;
        let cap = Capability::original(Object { kind, target: Target::Range { base, len } });
        self.place(link, cap, None);
        Ok(())
    }

    // ---- Deriving ----

    /// Puts in `to` a capability derived from the one in `from`
    /// ([`Capability::derive`]), as its child. Into another space this
    /// also needs the `transfer` right.
    pub fn derive(&mut self, from: SlotRef, to: SlotRef, how: Derivation) -> Result<(), CapError> {
        let (src, dst, cap) = self.source_and_destination(from, to)?;
        let derived = cap.derive(how)?;
        self.may_derive(src, &cap, from.space != to.space)?;
        self.place(dst, derived, Some(src));
        self.retain(&derived);
        Ok(())
    }

    /// Puts in `to` a copy of the capability in `from` sealed with the
    /// endpoint in `key` ([`Capability::seal_with`]), as its child.
    pub fn seal(&mut self, from: SlotRef, key: SlotRef, to: SlotRef) -> Result<(), CapError> {
        let key = self.get(key)?;
        let (src, dst, cap) = self.source_and_destination(from, to)?;
        let sealed = cap.seal_with(&key)?;
        self.may_derive(src, &cap, from.space != to.space)?;
        self.place(dst, sealed, Some(src));
        self.retain(&sealed);
        Ok(())
    }

    /// Puts in `to` an unsealed copy of the sealed capability in `from`,
    /// if `key` holds the endpoint it was sealed with
    /// ([`Capability::unseal_with`]), as its child.
    pub fn unseal(&mut self, from: SlotRef, key: SlotRef, to: SlotRef) -> Result<(), CapError> {
        let key = self.get(key)?;
        let (src, dst, cap) = self.source_and_destination(from, to)?;
        let unsealed = cap.unseal_with(&key)?;
        self.may_derive(src, &cap, from.space != to.space)?;
        self.place(dst, unsealed, Some(src));
        self.retain(&unsealed);
        Ok(())
    }

    /// Makes `count` objects of `kind` (memory, frames or DMA buffers) of
    /// 2^`order` pages each from the memory the capability in `memory`
    /// designates, and puts their capabilities, children of that one, in
    /// the `count` slots from `to`. It needs the `control` and `copy`
    /// rights. The memory goes back to the memory object when every object
    /// made from it is gone.
    pub fn retype(
        &mut self,
        memory: SlotRef,
        kind: ObjectKind,
        order: u32,
        to: SlotRef,
        count: u32,
    ) -> Result<(), CapError> {
        let src = self.full(memory)?;
        let cap = self.slot(src).cap.ok_or(CapError::Empty)?;
        if count == 0 || count > MAX_RETYPE {
            return Err(CapError::InvalidArgument);
        }
        let first = self.link(to)?;
        let end = to.slot.checked_add(count).ok_or(CapError::NoSuchSlot)?;
        if end as usize > self.spaces[first.space as usize].slots.len() {
            return Err(CapError::NoSuchSlot);
        }
        for slot in to.slot..end {
            if self.slot(Link { space: first.space, slot }).cap.is_some() {
                return Err(CapError::Occupied);
            }
        }
        // Checks the rights and the kind; the name is filled in below.
        cap.retyped(kind, ObjectId { index: 0, generation: 0 })?;
        let Some(memory_id) = cap.id() else {
            return Err(CapError::InvalidArgument);
        };
        let parent = self.entry(memory_id).ok_or(CapError::Empty)?;
        let extent = parent.extent.ok_or(CapError::InvalidArgument)?;
        let (offset, watermark) =
            carve(extent.base, extent.pages, parent.watermark, order, u64::from(count))?;
        self.may_derive(src, &cap, memory.space != to.space)?;
        // Room for the records first, so nothing below can fail.
        let free = self.free_records();
        if free < count as usize {
            self.objects.try_reserve(count as usize - free).map_err(|_| CapError::NoMemory)?;
        }
        for i in 0..count {
            let base = extent.base + offset + (u64::from(i) << order);
            let id = self.new_object(Entry {
                kind,
                caps: 1,
                children: 0,
                extent: Some(Extent { base, pages: 1 << order }),
                watermark: 0,
                origin: Some(memory_id),
            })?;
            let child = cap.retyped(kind, id)?;
            self.place(Link { space: first.space, slot: to.slot + i }, child, Some(src));
        }
        let parent = self.entry_mut(memory_id).ok_or(CapError::Empty)?;
        parent.children += count;
        parent.watermark = watermark;
        Ok(())
    }

    // ---- Moving and deleting ----

    /// Moves the capability in `from` to `to`, keeping its place in the
    /// derivation tree. Into another space it needs the `transfer` right.
    pub fn move_cap(&mut self, from: SlotRef, to: SlotRef) -> Result<(), CapError> {
        let src = self.full(from)?;
        let dst = self.empty(to)?;
        let node = *self.slot(src);
        let cap = node.cap.ok_or(CapError::Empty)?;
        if from.space != to.space && !cap.rights.contains(Rights::TRANSFER) {
            return Err(CapError::MissingRights(Rights::TRANSFER));
        }
        if node.revoking {
            return Err(CapError::Revoking);
        }
        // The flags stay behind: they belong to the slot. A cache may
        // hold the capability under its old slot, not its new one.
        *self.slot_mut(dst) = Slot { flags: 0, cached: false, ..node };
        *self.slot_mut(src) = Slot::EMPTY;
        self.stale |= node.cached;
        match node.prev {
            Some(prev) => self.slot_mut(prev).next = Some(dst),
            None => {
                if let Some(parent) = node.parent {
                    self.slot_mut(parent).child = Some(dst);
                }
            }
        }
        if let Some(next) = node.next {
            self.slot_mut(next).prev = Some(dst);
        }
        let mut child = node.child;
        while let Some(c) = child {
            let slot = self.slot_mut(c);
            slot.parent = Some(dst);
            child = slot.next;
        }
        Ok(())
    }

    /// Deletes the capability in `at`. Capabilities derived from it stay,
    /// as children of its parent. If it was the last capability to its
    /// object, and nothing retyped from the object is left, the object
    /// goes too and `released` hears of it.
    pub fn delete(
        &mut self,
        at: SlotRef,
        released: &mut dyn FnMut(Released),
    ) -> Result<(), CapError> {
        let link = self.full(at)?;
        self.remove(link, released);
        Ok(())
    }

    /// Deletes every capability derived from the one in `at`, in every
    /// space, keeping that one; at most `budget` of them in this call.
    /// Returns [`Progress::More`] when the budget ran out: until a call
    /// returns [`Progress::Done`], nothing can be derived from the
    /// capability or from those below it, and it cannot be moved.
    pub fn revoke(
        &mut self,
        at: SlotRef,
        budget: usize,
        released: &mut dyn FnMut(Released),
    ) -> Result<Progress, CapError> {
        let top = self.full(at)?;
        self.slot_mut(top).revoking = true;
        let mut deleted = 0;
        let mut at = top;
        loop {
            if let Some(child) = self.slot(at).child {
                at = child;
                continue;
            }
            if at == top {
                break;
            }
            if deleted == budget {
                return Ok(Progress::More);
            }
            // Below `top`, so it has a parent.
            let parent = self.slot(at).parent.unwrap_or(top);
            self.remove(at, released);
            deleted += 1;
            at = parent;
        }
        self.slot_mut(top).revoking = false;
        Ok(Progress::Done)
    }

    /// Goes on with a revoke of the capability in `at` that
    /// [`Capabilities::revoke`] left at [`Progress::More`]. The caller let
    /// go of the store in between, so the capability may have gone (it was
    /// deleted, and another may be in its slot now, whose descendants
    /// nobody asked to revoke), or another revoke of it may have finished:
    /// either way the slot has no revoke mark, and nothing is left of what
    /// was asked, so this is [`Progress::Done`].
    pub fn continue_revoke(
        &mut self,
        at: SlotRef,
        budget: usize,
        released: &mut dyn FnMut(Released),
    ) -> Result<Progress, CapError> {
        match self.link(at) {
            Ok(link) if self.slot(link).revoking => self.revoke(at, budget, released),
            _ => Ok(Progress::Done),
        }
    }

    // ---- Checking ----

    /// Counts what the store holds.
    pub fn census(&self) -> Census {
        let mut census = Census::default();
        for space in self.spaces.iter().filter(|space| space.live) {
            census.spaces += 1;
            census.slots += space.slots.len() as u64;
            for slot in &space.slots {
                if let Some(cap) = slot.cap {
                    census.caps[cap.kind().index()] += 1;
                    census.deepest = census.deepest.max(slot.depth);
                    census.revoking += u32::from(slot.revoking);
                }
            }
        }
        for entry in self.objects.iter().filter_map(|record| record.entry.as_ref()) {
            census.objects[entry.kind.index()] += 1;
            if entry.origin.is_none()
                && let Some(extent) = entry.extent
            {
                census.memory_pages += extent.pages;
            }
        }
        census
    }

    /// Checks every invariant of the store: the tree's links agree with
    /// each other, depths grow down the tree, every capability grants no
    /// more than its parent, no capability holds a right its kind lacks or
    /// a badge its kind cannot carry, every capability names a live object
    /// of its kind, and every object's counts are right. Takes time in
    /// proportion to the store's size, and memory for two counts per
    /// object.
    pub fn audit(&self) -> Result<Census, Inconsistency> {
        let fail = |what, at| Inconsistency { what, at };
        let mut census = Census::default();
        let mut children_listed = 0u64;
        let mut non_roots = 0u64;
        let mut counts: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        counts
            .try_reserve_exact(self.objects.len())
            .map_err(|_| fail("out of memory for the audit", None))?;
        for _ in 0..self.objects.len() {
            let _ = counts.push_within_capacity((0, 0));
        }
        for (index, space) in self.spaces.iter().enumerate() {
            if !space.live {
                if !space.slots.is_empty() {
                    return Err(fail("a free space has slots", None));
                }
                continue;
            }
            census.spaces += 1;
            census.slots += space.slots.len() as u64;
            let id = SpaceId { index: index as u32, generation: space.generation };
            for (number, slot) in space.slots.iter().enumerate() {
                let here = Link { space: index as u32, slot: number as u32 };
                let at = Some(SlotRef { space: id, slot: number as u32 });
                let Some(cap) = slot.cap else {
                    if slot.parent.is_some()
                        || slot.child.is_some()
                        || slot.prev.is_some()
                        || slot.next.is_some()
                        || slot.revoking
                        || slot.depth != 0
                    {
                        return Err(fail("an empty slot has links", at));
                    }
                    continue;
                };
                let kind = cap.kind();
                census.caps[kind.index()] += 1;
                census.deepest = census.deepest.max(slot.depth);
                census.revoking += u32::from(slot.revoking);
                if slot.depth > MAX_DEPTH {
                    return Err(fail("a capability is deeper than MAX_DEPTH", at));
                }
                if !kind.rights().contains(cap.rights) {
                    return Err(fail("a capability holds a right its kind does not have", at));
                }
                if cap.badge != 0 && !kind.badged() {
                    return Err(fail("a capability carries a badge its kind cannot", at));
                }
                match cap.object.target {
                    Target::Object(id) => {
                        let Some(entry) = self.entry(id) else {
                            return Err(fail("a capability names an object that is gone", at));
                        };
                        if entry.kind != kind || kind.is_range() {
                            return Err(fail("a capability names an object of another kind", at));
                        }
                        counts[id.index as usize].0 += 1;
                    }
                    Target::Range { base, len } => {
                        let limit = kind.range_limit();
                        if len == 0 || limit.is_none_or(|limit| base.saturating_add(len) > limit) {
                            return Err(fail("a capability has a bad range", at));
                        }
                    }
                }
                if let Some(sealer) = cap.sealed_by
                    && self.entry(sealer).is_some_and(|entry| entry.kind != ObjectKind::Endpoint)
                {
                    return Err(fail("a capability is sealed by something not an endpoint", at));
                }
                match slot.parent {
                    None => {
                        if slot.prev.is_some() || slot.next.is_some() {
                            return Err(fail("a capability without a parent has siblings", at));
                        }
                    }
                    Some(parent) => {
                        non_roots += 1;
                        let Some(above) = self.try_slot(parent) else {
                            return Err(fail("a parent link leads nowhere", at));
                        };
                        let Some(parent_cap) = above.cap else {
                            return Err(fail("a capability's parent slot is empty", at));
                        };
                        if above.depth >= slot.depth {
                            return Err(fail("a capability is no deeper than its parent", at));
                        }
                        if !cap.within(&parent_cap) && !self.retyped_from(&cap, &parent_cap) {
                            return Err(fail("a capability grants more than its parent", at));
                        }
                    }
                }
                // The children list: each child names this slot as its
                // parent, and the list's links agree in both directions.
                let mut prev = None;
                let mut child = slot.child;
                let mut steps = 0u64;
                while let Some(c) = child {
                    let Some(below) = self.try_slot(c) else {
                        return Err(fail("a child link leads nowhere", at));
                    };
                    if below.cap.is_none() || below.parent != Some(here) || below.prev != prev {
                        return Err(fail("a children list is inconsistent", at));
                    }
                    steps += 1;
                    if steps > u64::from(MAX_SLOTS) * self.spaces.len() as u64 {
                        return Err(fail("a children list has a cycle", at));
                    }
                    prev = Some(c);
                    child = below.next;
                }
                children_listed += steps;
            }
        }
        if children_listed != non_roots {
            return Err(fail("a capability is missing from its parent's children", None));
        }
        for record in &self.objects {
            if let Some(entry) = &record.entry
                && let Some(origin) = entry.origin
            {
                let Some(parent) = self.entry(origin) else {
                    return Err(fail("an object was retyped from memory that is gone", None));
                };
                let (Some(own), Some(from)) = (entry.extent, parent.extent) else {
                    return Err(fail("a retyped object has no extent", None));
                };
                if own.base < from.base || own.base + own.pages > from.base + parent.watermark {
                    return Err(fail("a retyped object lies outside its memory's used part", None));
                }
                counts[origin.index as usize].1 += 1;
            }
        }
        for (record, &(caps, children)) in self.objects.iter().zip(counts.iter()) {
            let Some(entry) = &record.entry else { continue };
            census.objects[entry.kind.index()] += 1;
            if entry.origin.is_none()
                && let Some(extent) = entry.extent
            {
                census.memory_pages += extent.pages;
            }
            if entry.caps != caps || entry.children != children {
                return Err(fail("an object's counts are wrong", None));
            }
            if caps == 0 && children == 0 {
                return Err(fail("an object outlived its capabilities", None));
            }
            if entry.watermark > entry.extent.map_or(0, |extent| extent.pages)
                || (children == 0 && entry.watermark != 0)
            {
                return Err(fail("a memory object's watermark is wrong", None));
            }
        }
        Ok(census)
    }

    // ---- Internals ----

    fn space(&self, id: SpaceId) -> Result<&Space<A>, CapError> {
        match self.spaces.get(id.index as usize) {
            Some(space) if space.live && space.generation == id.generation => Ok(space),
            _ => Err(CapError::NoSuchSpace),
        }
    }

    /// The link for `at`, which must name a slot of a live space.
    fn link(&self, at: SlotRef) -> Result<Link, CapError> {
        if at.slot as usize >= self.space(at.space)?.slots.len() {
            return Err(CapError::NoSuchSlot);
        }
        Ok(Link { space: at.space.index, slot: at.slot })
    }

    fn slot_ref(&self, link: Link) -> SlotRef {
        let generation = self.spaces[link.space as usize].generation;
        SlotRef { space: SpaceId { index: link.space, generation }, slot: link.slot }
    }

    /// The link for `at`, which must hold a capability.
    fn full(&self, at: SlotRef) -> Result<Link, CapError> {
        let link = self.link(at)?;
        if self.slot(link).cap.is_none() {
            return Err(CapError::Empty);
        }
        Ok(link)
    }

    /// The link for `at`, which must be empty.
    fn empty(&self, at: SlotRef) -> Result<Link, CapError> {
        let link = self.link(at)?;
        if self.slot(link).cap.is_some() {
            return Err(CapError::Occupied);
        }
        Ok(link)
    }

    fn source_and_destination(
        &self,
        from: SlotRef,
        to: SlotRef,
    ) -> Result<(Link, Link, Capability), CapError> {
        let src = self.full(from)?;
        let dst = self.empty(to)?;
        let cap = self.slot(src).cap.ok_or(CapError::Empty)?;
        Ok((src, dst, cap))
    }

    // Links inside the store always name a slot of a live space: spaces
    // lose their capabilities, and so every link into them, before they
    // are freed. A bad link is a bug in this module, and indexing panics.
    fn slot(&self, link: Link) -> &Slot {
        &self.spaces[link.space as usize].slots[link.slot as usize]
    }

    fn slot_mut(&mut self, link: Link) -> &mut Slot {
        &mut self.spaces[link.space as usize].slots[link.slot as usize]
    }

    /// For the audit, which must not panic on a bad link.
    fn try_slot(&self, link: Link) -> Option<&Slot> {
        self.spaces.get(link.space as usize)?.slots.get(link.slot as usize)
    }

    /// Whether a capability made from `src` may be placed: not too deep,
    /// not below a revoke under way, and with `transfer` if it goes to
    /// another space.
    ///
    /// Deleting a capability moves its children nearer the root while
    /// their depths stay, so a depth may be more than the distance to the
    /// root. Too deep is judged by the distance, and the depths on the way
    /// from `src` to its root become their distances: otherwise a
    /// capability copied and deleted in turn, as a shell saves and
    /// restores a descriptor with dup, grew deeper each time until
    /// nothing more could be derived from it.
    fn may_derive(&mut self, src: Link, cap: &Capability, elsewhere: bool) -> Result<(), CapError> {
        if elsewhere && !cap.rights.contains(Rights::TRANSFER) {
            return Err(CapError::MissingRights(Rights::TRANSFER));
        }
        // Depths fall strictly towards the root, so these end within
        // MAX_DEPTH steps, and the distance fits.
        let mut distance = 0u8;
        let mut revoking = false;
        let mut at = Some(src);
        while let Some(link) = at {
            let slot = self.slot(link);
            revoking |= slot.revoking;
            at = slot.parent;
            distance += u8::from(at.is_some());
        }
        if distance >= MAX_DEPTH {
            return Err(CapError::TooDeep);
        }
        if revoking {
            return Err(CapError::Revoking);
        }
        // Each depth only falls, and to one more than its parent's new
        // one, so every depth stays more than its parent's.
        let mut depth = distance;
        let mut at = Some(src);
        while let Some(link) = at {
            let slot = self.slot_mut(link);
            slot.depth = depth;
            depth = depth.saturating_sub(1);
            at = slot.parent;
        }
        Ok(())
    }

    /// Puts `cap` in the empty slot `link`, as the first child of
    /// `parent`.
    fn place(&mut self, link: Link, cap: Capability, parent: Option<Link>) {
        // Only an empty slot, which no cache can hold, takes a capability.
        debug_assert!(self.slot(link).cap.is_none(), "placing a capability in a full slot");
        let (depth, next) = match parent {
            Some(parent) => {
                let above = self.slot_mut(parent);
                (above.depth + 1, above.child.replace(link))
            }
            None => (0, None),
        };
        if let Some(next) = next {
            self.slot_mut(next).prev = Some(link);
        }
        // Written where it stays, not made aside and moved in.
        let slot = self.slot_mut(link);
        slot.clear();
        slot.cap = Some(cap);
        slot.parent = parent;
        slot.next = next;
        slot.depth = depth;
    }

    /// Takes the capability out of `link`: its children move up to its
    /// parent, in its place among its siblings, and its object loses a
    /// capability.
    fn remove(&mut self, link: Link, released: &mut dyn FnMut(Released)) {
        // What the rest needs of the slot, without a copy of all of it.
        let slot = self.slot_mut(link);
        let held = slot.cap.as_ref().and_then(Capability::id);
        let node =
            Links { parent: slot.parent, child: slot.child, prev: slot.prev, next: slot.next };
        let cached = slot.cached;
        slot.clear();
        self.stale |= cached;
        match (node.parent, node.child) {
            (Some(parent), Some(first)) => {
                let mut last = first;
                let mut child = Some(first);
                while let Some(c) = child {
                    let slot = self.slot_mut(c);
                    slot.parent = Some(parent);
                    last = c;
                    child = slot.next;
                }
                self.slot_mut(first).prev = node.prev;
                match node.prev {
                    Some(prev) => self.slot_mut(prev).next = Some(first),
                    None => self.slot_mut(parent).child = Some(first),
                }
                self.slot_mut(last).next = node.next;
                if let Some(next) = node.next {
                    self.slot_mut(next).prev = Some(last);
                }
            }
            (Some(parent), None) => {
                match node.prev {
                    Some(prev) => self.slot_mut(prev).next = node.next,
                    None => self.slot_mut(parent).child = node.next,
                }
                if let Some(next) = node.next {
                    self.slot_mut(next).prev = node.prev;
                }
            }
            (None, first) => {
                // The children become roots, with no siblings.
                let mut child = first;
                while let Some(c) = child {
                    let slot = self.slot_mut(c);
                    child = slot.next;
                    slot.parent = None;
                    slot.prev = None;
                    slot.next = None;
                }
            }
        }
        if let Some(id) = held {
            self.release(id, released);
        }
    }

    fn retain(&mut self, cap: &Capability) {
        if let Some(id) = cap.id()
            && let Some(entry) = self.entry_mut(id)
        {
            entry.caps += 1;
        }
    }

    /// Drops a capability's hold on the object `id`, and the object if
    /// that was the last hold, and so on up the memory it was retyped from.
    fn release(&mut self, mut id: ObjectId, released: &mut dyn FnMut(Released)) {
        let Some(entry) = self.entry_mut(id) else { return };
        entry.caps -= 1;
        loop {
            let Some(entry) = self.entry(id).copied() else { return };
            if entry.caps > 0 || entry.children > 0 {
                return;
            }
            let record = &mut self.objects[id.index as usize];
            record.entry = None;
            record.generation = bump(record.generation);
            record.next_free = self.free_object;
            self.free_object = Some(id.index);
            released(Released {
                kind: entry.kind,
                id,
                memory: if entry.origin.is_none() { entry.extent } else { None },
            });
            let Some(origin) = entry.origin else { return };
            let Some(parent) = self.entry_mut(origin) else { return };
            parent.children -= 1;
            if parent.children == 0 {
                parent.watermark = 0;
            }
            id = origin;
        }
    }

    fn entry(&self, id: ObjectId) -> Option<&Entry> {
        let record = self.objects.get(id.index as usize)?;
        if record.generation != id.generation {
            return None;
        }
        record.entry.as_ref()
    }

    fn entry_mut(&mut self, id: ObjectId) -> Option<&mut Entry> {
        let record = self.objects.get_mut(id.index as usize)?;
        if record.generation != id.generation {
            return None;
        }
        record.entry.as_mut()
    }

    /// Records on the free list and spare capacity.
    fn free_records(&self) -> usize {
        let mut free = self.objects.capacity() - self.objects.len();
        let mut at = self.free_object;
        while let Some(index) = at {
            free += 1;
            at = self.objects[index as usize].next_free;
        }
        free
    }

    fn new_object(&mut self, entry: Entry) -> Result<ObjectId, CapError> {
        let index = match self.free_object {
            Some(index) => {
                let record = &mut self.objects[index as usize];
                self.free_object = record.next_free.take();
                record.entry = Some(entry);
                index
            }
            None => {
                let index = u32::try_from(self.objects.len()).map_err(|_| CapError::NoMemory)?;
                self.objects.try_reserve(1).map_err(|_| CapError::NoMemory)?;
                let record = Record { generation: 1, entry: Some(entry), next_free: None };
                // Cannot fail: the capacity was reserved.
                let _ = self.objects.push_within_capacity(record);
                index
            }
        };
        Ok(ObjectId { index, generation: self.objects[index as usize].generation })
    }

    /// Whether `cap` names an object retyped from the memory `parent`
    /// names, directly or through smaller memory retyped from it, with no
    /// more of its rights. Deleting the capability to that smaller memory
    /// moves the objects retyped from it up to `parent`.
    fn retyped_from(&self, cap: &Capability, parent: &Capability) -> bool {
        let (Some(id), Some(memory)) = (cap.id(), parent.id()) else { return false };
        if parent.kind() != ObjectKind::Memory
            || !cap.kind().from_memory()
            || !parent.rights.contains(cap.rights)
            || cap.badge != 0
        {
            return false;
        }
        // Every object on the way stays while something retyped from it
        // does; the bound only guards a corrupt store against a cycle.
        let mut origin = self.entry(id).and_then(|entry| entry.origin);
        for _ in 0..self.objects.len() {
            match origin {
                Some(above) if above == memory => return true,
                Some(above) => origin = self.entry(above).and_then(|entry| entry.origin),
                None => return false,
            }
        }
        false
    }
}
