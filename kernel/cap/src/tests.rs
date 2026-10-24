// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests of the store: hand-written cases, then random operation
//! sequences checked against a simple model of the derivation tree.

use std::alloc::Global;
use std::collections::HashMap;

use proptest::prelude::*;

use crate::*;

type Store = Capabilities<Global>;

fn store() -> Store {
    Capabilities::new(Global)
}

fn at(space: SpaceId, slot: u32) -> SlotRef {
    SlotRef::new(space, slot)
}

/// Collects what the store releases.
#[derive(Default)]
struct Bin(Vec<Released>);

impl Bin {
    fn sink(&mut self) -> impl FnMut(Released) + '_ {
        |released| self.0.push(released)
    }
}

#[test]
fn derived_capabilities_never_grant_more() {
    let mut caps = store();
    let a = caps.create_space(8).unwrap();
    let b = caps.create_space(8).unwrap();
    caps.create(at(a, 0), ObjectKind::Endpoint).unwrap();
    let send = Rights::SEND | Rights::COPY | Rights::TRANSFER;
    caps.derive(at(a, 0), at(b, 0), Derivation { rights: send, badge: Some(9), range: None })
        .unwrap();
    let derived = caps.get(at(b, 0)).unwrap();
    assert_eq!((derived.rights(), derived.badge()), (send, 9));
    assert_eq!(caps.parent(at(b, 0)), Ok(Some(at(a, 0))));
    // Asking for every right gives back only those held.
    caps.derive(at(b, 0), at(b, 1), Derivation::COPY).unwrap();
    assert_eq!(caps.get(at(b, 1)).unwrap().rights(), send);
    assert_eq!(
        caps.lookup(at(b, 1), ObjectKind::Endpoint, Rights::RECEIVE),
        Err(CapError::MissingRights(Rights::RECEIVE))
    );
    assert_eq!(caps.depth(at(b, 1)), Ok(2));
    caps.audit().unwrap();
}

#[test]
fn objects_made_for_a_holder_get_only_the_rights_asked_for() {
    let mut caps = store();
    let a = caps.create_space(4).unwrap();
    let read = Rights::READ | Rights::COPY;
    // EXECUTE is not a file's right, so it is dropped.
    caps.create_with_rights(at(a, 0), ObjectKind::File, read | Rights::EXECUTE).unwrap();
    let cap = caps.get(at(a, 0)).unwrap();
    assert_eq!(cap.rights(), read);
    assert_eq!(caps.parent(at(a, 0)), Ok(None));
    // Nothing made from it gets more.
    caps.derive(at(a, 0), at(a, 1), Derivation::COPY).unwrap();
    assert_eq!(caps.get(at(a, 1)).unwrap().rights(), read);
    assert_eq!(
        caps.lookup(at(a, 1), ObjectKind::File, Rights::WRITE),
        Err(CapError::MissingRights(Rights::WRITE))
    );
    caps.audit().unwrap();
    // The object lives while either capability does.
    let mut bin = Bin::default();
    caps.delete(at(a, 0), &mut bin.sink()).unwrap();
    assert!(bin.0.is_empty());
    caps.delete(at(a, 1), &mut bin.sink()).unwrap();
    assert_eq!(bin.0.len(), 1);
    caps.audit().unwrap();
}

#[test]
fn transfer_is_a_right_of_its_own() {
    let mut caps = store();
    let a = caps.create_space(4).unwrap();
    let b = caps.create_space(4).unwrap();
    caps.create(at(a, 0), ObjectKind::Notification).unwrap();
    let local = Rights::ALL - Rights::TRANSFER;
    caps.derive(at(a, 0), at(a, 1), Derivation::with_rights(local)).unwrap();
    assert_eq!(
        caps.derive(at(a, 1), at(b, 0), Derivation::COPY),
        Err(CapError::MissingRights(Rights::TRANSFER))
    );
    assert_eq!(caps.move_cap(at(a, 1), at(b, 0)), Err(CapError::MissingRights(Rights::TRANSFER)));
    // Within its own space it moves freely.
    caps.move_cap(at(a, 1), at(a, 3)).unwrap();
    assert_eq!(caps.parent(at(a, 3)), Ok(Some(at(a, 0))));
    caps.move_cap(at(a, 0), at(b, 2)).unwrap();
    assert_eq!(caps.parent(at(a, 3)), Ok(Some(at(b, 2))));
    caps.audit().unwrap();
}

#[test]
fn revoke_reaches_every_space_and_survives_deletes() {
    let mut caps = store();
    let spaces: Vec<_> = (0..3).map(|_| caps.create_space(8).unwrap()).collect();
    let [a, b, c] = spaces[..] else { unreachable!() };
    caps.create(at(a, 0), ObjectKind::Endpoint).unwrap();
    caps.derive(at(a, 0), at(b, 0), Derivation::COPY).unwrap();
    caps.derive(at(b, 0), at(c, 0), Derivation::COPY).unwrap();
    caps.derive(at(c, 0), at(c, 1), Derivation::with_badge(5)).unwrap();
    caps.derive(at(b, 0), at(b, 1), Derivation::COPY).unwrap();
    // Deleting the middle of the chain moves its children up.
    let mut bin = Bin::default();
    caps.delete(at(b, 0), &mut bin.sink()).unwrap();
    assert_eq!(caps.parent(at(c, 0)), Ok(Some(at(a, 0))));
    assert_eq!(caps.parent(at(b, 1)), Ok(Some(at(a, 0))));
    caps.audit().unwrap();
    assert_eq!(caps.revoke(at(a, 0), 100, &mut bin.sink()), Ok(Progress::Done));
    for slot in [at(b, 1), at(c, 0), at(c, 1)] {
        assert_eq!(caps.get(slot), Err(CapError::Empty));
    }
    assert!(caps.get(at(a, 0)).is_ok());
    assert!(bin.0.is_empty(), "the endpoint still has its original capability");
    caps.delete(at(a, 0), &mut bin.sink()).unwrap();
    assert_eq!(bin.0.len(), 1);
    assert_eq!(caps.census().total_objects(), 0);
    caps.audit().unwrap();
}

#[test]
fn revoke_in_steps_blocks_new_derivations() {
    let mut caps = store();
    let a = caps.create_space(64).unwrap();
    caps.create(at(a, 0), ObjectKind::Endpoint).unwrap();
    for slot in 1..40 {
        caps.derive(at(a, slot / 2), at(a, slot), Derivation::COPY).unwrap();
    }
    let mut bin = Bin::default();
    assert_eq!(caps.revoke(at(a, 0), 10, &mut bin.sink()), Ok(Progress::More));
    assert_eq!(caps.is_revoking(at(a, 0)), Ok(true));
    // Everything left below the capability is frozen.
    let left: Vec<u32> = (1..40).filter(|&s| caps.get(at(a, s)).is_ok()).collect();
    assert_eq!(left.len(), 29);
    for &slot in &left {
        assert_eq!(caps.derive(at(a, slot), at(a, 63), Derivation::COPY), Err(CapError::Revoking));
    }
    assert_eq!(caps.derive(at(a, 0), at(a, 63), Derivation::COPY), Err(CapError::Revoking));
    assert_eq!(caps.move_cap(at(a, 0), at(a, 63)), Err(CapError::Revoking));
    caps.audit().unwrap();
    let mut calls = 1;
    loop {
        calls += 1;
        match caps.revoke(at(a, 0), 10, &mut bin.sink()) {
            Ok(Progress::More) => caps.audit().map(drop).unwrap(),
            Ok(Progress::Done) => break,
            Err(err) => panic!("{err}"),
        }
    }
    assert_eq!(calls, 4);
    assert_eq!(caps.census().total_caps(), 1);
    assert_eq!(caps.is_revoking(at(a, 0)), Ok(false));
    caps.derive(at(a, 0), at(a, 63), Derivation::COPY).unwrap();
    caps.audit().unwrap();
}

/// A revoke in steps whose capability is deleted between steps, and
/// another put in its slot, stops: the newcomer keeps its descendants.
#[test]
fn a_revoke_ends_with_its_capability() {
    let mut caps = store();
    let a = caps.create_space(64).unwrap();
    caps.create(at(a, 0), ObjectKind::Endpoint).unwrap();
    for slot in 1..20 {
        caps.derive(at(a, 0), at(a, slot), Derivation::COPY).unwrap();
    }
    let mut bin = Bin::default();
    assert_eq!(caps.revoke(at(a, 0), 5, &mut bin.sink()), Ok(Progress::More));
    // Another thread deletes it and puts a new endpoint, with copies of
    // its own, in the slot.
    caps.delete(at(a, 0), &mut bin.sink()).unwrap();
    caps.create(at(a, 0), ObjectKind::Endpoint).unwrap();
    for slot in 40..44 {
        caps.derive(at(a, 0), at(a, slot), Derivation::COPY).unwrap();
    }
    let left = caps.census().total_caps();
    assert_eq!(caps.continue_revoke(at(a, 0), 5, &mut bin.sink()), Ok(Progress::Done));
    assert_eq!(caps.census().total_caps(), left);
    for slot in 40..44 {
        assert!(caps.get(at(a, slot)).is_ok());
    }
    // A revoke that a second caller finished is done for the first too.
    assert_eq!(caps.revoke(at(a, 0), 1, &mut bin.sink()), Ok(Progress::More));
    assert_eq!(caps.revoke(at(a, 0), 100, &mut bin.sink()), Ok(Progress::Done));
    assert_eq!(caps.continue_revoke(at(a, 0), 1, &mut bin.sink()), Ok(Progress::Done));
    // An empty slot, or a space that is gone, has nothing left to revoke.
    caps.delete(at(a, 0), &mut bin.sink()).unwrap();
    assert_eq!(caps.continue_revoke(at(a, 0), 1, &mut bin.sink()), Ok(Progress::Done));
    caps.audit().unwrap();
}

#[test]
fn derivation_depth_is_bounded() {
    let mut caps = store();
    let a = caps.create_space(MAX_DEPTH as u32 + 2).unwrap();
    caps.create(at(a, 0), ObjectKind::Thread).unwrap();
    for slot in 1..=MAX_DEPTH as u32 {
        caps.derive(at(a, slot - 1), at(a, slot), Derivation::COPY).unwrap();
    }
    let last = at(a, MAX_DEPTH as u32);
    assert_eq!(caps.depth(last), Ok(MAX_DEPTH));
    assert_eq!(
        caps.derive(last, at(a, MAX_DEPTH as u32 + 1), Derivation::COPY),
        Err(CapError::TooDeep)
    );
    caps.audit().unwrap();
}

#[test]
fn sealed_capabilities_round_trip_through_the_store() {
    let mut caps = store();
    let client = caps.create_space(4).unwrap();
    let service = caps.create_space(4).unwrap();
    caps.create(at(service, 0), ObjectKind::Endpoint).unwrap();
    caps.create(at(client, 0), ObjectKind::Process).unwrap();
    caps.seal(at(client, 0), at(service, 0), at(client, 1)).unwrap();
    assert_eq!(
        caps.lookup(at(client, 1), ObjectKind::Process, Rights::NONE),
        Err(CapError::Sealed)
    );
    caps.move_cap(at(client, 1), at(service, 1)).unwrap();
    caps.unseal(at(service, 1), at(service, 0), at(service, 2)).unwrap();
    assert_eq!(caps.get(at(service, 2)), caps.get(at(client, 0)));
    assert_eq!(caps.parent(at(service, 2)), Ok(Some(at(service, 1))));
    let mut bin = Bin::default();
    caps.revoke(at(client, 0), 8, &mut bin.sink()).unwrap();
    assert_eq!(caps.get(at(service, 2)), Err(CapError::Empty));
    caps.audit().unwrap();
}

#[test]
fn retyped_memory_comes_back() {
    let mut caps = store();
    let a = caps.create_space(32).unwrap();
    let memory = caps.create_memory(at(a, 0), 0x400, 16).unwrap();
    caps.retype(at(a, 0), ObjectKind::Frame, 0, at(a, 1), 3).unwrap();
    caps.retype(at(a, 0), ObjectKind::Memory, 2, at(a, 4), 2).unwrap();
    let info = caps.object(memory).unwrap();
    assert_eq!((info.children, info.watermark), (5, 12));
    assert_eq!(caps.retype(at(a, 0), ObjectKind::Frame, 3, at(a, 6), 1), Err(CapError::Exhausted));
    assert_eq!(
        caps.retype(at(a, 0), ObjectKind::Endpoint, 0, at(a, 6), 1),
        Err(CapError::InvalidArgument)
    );
    assert_eq!(caps.retype(at(a, 0), ObjectKind::Frame, 0, at(a, 3), 2), Err(CapError::Occupied));
    assert_eq!(
        caps.retype(at(a, 0), ObjectKind::Frame, 0, at(a, 31), 2),
        Err(CapError::NoSuchSlot)
    );
    // The smaller memory can be retyped in turn.
    caps.retype(at(a, 4), ObjectKind::Dma, 1, at(a, 8), 2).unwrap();
    let frame = caps.get(at(a, 1)).unwrap();
    assert_eq!(
        caps.object(frame.id().unwrap()).unwrap().extent,
        Some(Extent { base: 0x400, pages: 1 })
    );
    let dma = caps.get(at(a, 9)).unwrap();
    assert_eq!(
        caps.object(dma.id().unwrap()).unwrap().extent,
        Some(Extent { base: 0x406, pages: 2 })
    );
    caps.audit().unwrap();
    // A frame copied elsewhere keeps the memory in use until it goes too.
    caps.derive(at(a, 1), at(a, 20), Derivation::COPY).unwrap();
    let mut bin = Bin::default();
    caps.delete(at(a, 1), &mut bin.sink()).unwrap();
    assert!(bin.0.is_empty());
    assert_eq!(caps.revoke(at(a, 0), 100, &mut bin.sink()), Ok(Progress::Done));
    assert_eq!(bin.0.len(), 7, "three frames, two memories and two DMA buffers: {:?}", bin.0);
    assert!(bin.0.iter().all(|released| released.memory.is_none()));
    let info = caps.object(memory).unwrap();
    assert_eq!((info.children, info.watermark), (0, 0));
    caps.retype(at(a, 0), ObjectKind::Frame, 4, at(a, 1), 1).unwrap();
    caps.audit().unwrap();
    // The last capability takes the memory with it.
    bin.0.clear();
    caps.destroy_space(a, &mut bin.sink()).unwrap();
    let given_back: Vec<_> = bin.0.iter().filter_map(|r| r.memory).collect();
    assert_eq!(given_back, [Extent { base: 0x400, pages: 16 }]);
    assert_eq!(caps.census(), Census::default());
    caps.audit().unwrap();
}

/// Deleting the capability to a memory object retyped from another moves
/// what was retyped from it up to the outer memory's capability, which
/// still covers it (a regression the property test found).
#[test]
fn nested_memory_survives_deleting_the_middle() {
    let mut caps = store();
    let a = caps.create_space(8).unwrap();
    caps.create_memory(at(a, 0), 0x400, 4).unwrap();
    caps.retype(at(a, 0), ObjectKind::Memory, 1, at(a, 1), 1).unwrap();
    caps.retype(at(a, 1), ObjectKind::Memory, 0, at(a, 2), 1).unwrap();
    caps.retype(at(a, 2), ObjectKind::Frame, 0, at(a, 3), 1).unwrap();
    caps.derive(at(a, 3), at(a, 4), Derivation::with_rights(Rights::READ | Rights::COPY)).unwrap();
    let mut bin = Bin::default();
    caps.delete(at(a, 1), &mut bin.sink()).unwrap();
    caps.delete(at(a, 2), &mut bin.sink()).unwrap();
    caps.delete(at(a, 3), &mut bin.sink()).unwrap();
    assert!(bin.0.is_empty(), "the frame keeps both memories alive: {:?}", bin.0);
    assert_eq!(caps.parent(at(a, 4)), Ok(Some(at(a, 0))));
    caps.audit().unwrap();
    // The outer memory still revokes it, and everything is given back.
    assert_eq!(caps.revoke(at(a, 0), 10, &mut bin.sink()), Ok(Progress::Done));
    assert_eq!(bin.0.len(), 3, "the frame and two memories: {:?}", bin.0);
    let info = caps.object(caps.get(at(a, 0)).unwrap().id().unwrap()).unwrap();
    assert_eq!((info.children, info.watermark), (0, 0));
    caps.audit().unwrap();
}

#[test]
fn stale_names_are_refused() {
    let mut caps = store();
    let a = caps.create_space(2).unwrap();
    let endpoint = caps.create(at(a, 0), ObjectKind::Endpoint).unwrap();
    caps.destroy_space(a, &mut |_| {}).unwrap();
    assert_eq!(caps.get(at(a, 0)), Err(CapError::NoSuchSpace));
    let b = caps.create_space(2).unwrap();
    assert_eq!(b.index, a.index);
    assert_ne!(b, a);
    assert_eq!(caps.get(at(a, 0)), Err(CapError::NoSuchSpace));
    assert_eq!(caps.get(at(b, 2)), Err(CapError::NoSuchSlot));
    assert_eq!(caps.get(at(b, 0)), Err(CapError::Empty));
    let again = caps.create(at(b, 0), ObjectKind::Endpoint).unwrap();
    assert_eq!(again.index, endpoint.index);
    assert_ne!(again, endpoint);
    assert_eq!(caps.object(endpoint), None);
    assert_eq!(caps.create_space(0), Err(CapError::InvalidArgument));
    assert_eq!(caps.create_space(MAX_SLOTS + 1), Err(CapError::InvalidArgument));
}

#[test]
fn slot_flags_stay_with_the_slot() {
    let mut caps = store();
    let a = caps.create_space(4).unwrap();
    caps.create(at(a, 0), ObjectKind::Endpoint).unwrap();
    assert_eq!(caps.slot_flags(at(a, 1)), Err(CapError::Empty));
    assert_eq!(caps.set_slot_flags(at(a, 0), 1), Ok(0));
    assert_eq!(caps.slot_flags(at(a, 0)), Ok(1));
    assert_eq!(caps.find_flagged(a, 1, 0), Ok(Some(0)));
    assert_eq!(caps.find_flagged(a, 1, 1), Ok(None));
    // A copy starts clear, and a move leaves the flags behind.
    caps.derive(at(a, 0), at(a, 1), Derivation::COPY).unwrap();
    assert_eq!(caps.slot_flags(at(a, 1)), Ok(0));
    caps.move_cap(at(a, 0), at(a, 2)).unwrap();
    assert_eq!(caps.slot_flags(at(a, 2)), Ok(0));
    assert_eq!(caps.find_flagged(a, 1, 0), Ok(None));
    // Deleting clears them too.
    caps.set_slot_flags(at(a, 2), 1).unwrap();
    caps.delete(at(a, 2), &mut |_| {}).unwrap();
    caps.derive(at(a, 1), at(a, 2), Derivation::COPY).unwrap();
    assert_eq!(caps.slot_flags(at(a, 2)), Ok(0));
    assert_eq!(caps.used_slots(a, 0), Ok(0b110));
    assert_eq!(caps.used_slots(a, 2), Ok(0b1));
    caps.audit().unwrap();
}

#[test]
fn a_cached_capability_leaving_its_slot_makes_the_store_stale() {
    let mut caps = store();
    let a = caps.create_space(4).unwrap();
    caps.create(at(a, 0), ObjectKind::Endpoint).unwrap();
    caps.derive(at(a, 0), at(a, 1), Derivation::COPY).unwrap();
    assert!(!caps.take_stale());
    // Deleting or moving a capability nobody cached changes nothing cached.
    caps.move_cap(at(a, 1), at(a, 2)).unwrap();
    caps.delete(at(a, 2), &mut |_| {}).unwrap();
    assert!(!caps.take_stale());
    assert_eq!(caps.get_to_cache(at(a, 1)), Err(CapError::Empty));
    // Once cached, moving it out of its slot or deleting it does.
    let cap = caps.get_to_cache(at(a, 0)).unwrap();
    assert_eq!(caps.get(at(a, 0)), Ok(cap));
    caps.derive(at(a, 0), at(a, 1), Derivation::COPY).unwrap();
    assert!(!caps.take_stale());
    caps.move_cap(at(a, 0), at(a, 3)).unwrap();
    assert!(caps.take_stale());
    assert!(!caps.take_stale());
    // Its new slot is not cached.
    caps.delete(at(a, 3), &mut |_| {}).unwrap();
    assert!(!caps.take_stale());
    // A revoke of its parent, and destroying its space, take it out too.
    caps.get_to_cache(at(a, 1)).unwrap();
    caps.create(at(a, 0), ObjectKind::Notification).unwrap();
    caps.derive(at(a, 0), at(a, 2), Derivation::COPY).unwrap();
    caps.get_to_cache(at(a, 2)).unwrap();
    assert_eq!(caps.revoke(at(a, 0), usize::MAX, &mut |_| {}), Ok(Progress::Done));
    assert!(caps.take_stale());
    caps.destroy_space(a, &mut |_| {}).unwrap();
    assert!(caps.take_stale());
    caps.audit().unwrap();
}

#[test]
fn spaces_grow_and_keep_what_they_hold() {
    let mut caps = store();
    let a = caps.create_space(2).unwrap();
    let b = caps.create_space(2).unwrap();
    caps.create(at(a, 1), ObjectKind::Endpoint).unwrap();
    caps.derive(at(a, 1), at(b, 0), Derivation::COPY).unwrap();
    assert_eq!(caps.get(at(a, 5)), Err(CapError::NoSuchSlot));
    assert_eq!(caps.grow_space(a, 8), Ok(8));
    assert_eq!(caps.slots(a), Ok(8));
    // Never smaller.
    assert_eq!(caps.grow_space(a, 4), Ok(8));
    assert_eq!(caps.grow_space(a, MAX_SLOTS + 1), Err(CapError::InvalidArgument));
    caps.derive(at(b, 0), at(a, 7), Derivation::COPY).unwrap();
    assert_eq!(caps.parent(at(b, 0)), Ok(Some(at(a, 1))));
    assert_eq!(caps.revoke(at(a, 1), 100, &mut |_| {}), Ok(Progress::Done));
    assert_eq!(caps.get(at(a, 7)), Err(CapError::Empty));
    caps.audit().unwrap();
}

#[test]
fn device_ranges_narrow() {
    let mut caps = store();
    let a = caps.create_space(4).unwrap();
    caps.grant_range(at(a, 0), ObjectKind::IoPorts, 0, 1 << 16).unwrap();
    caps.derive(at(a, 0), at(a, 1), Derivation::with_range(0x60, 5)).unwrap();
    assert_eq!(caps.get(at(a, 1)).unwrap().range(), Some((0x60, 5)));
    assert_eq!(
        caps.derive(at(a, 1), at(a, 2), Derivation::with_range(0, 6)),
        Err(CapError::OutOfRange)
    );
    assert_eq!(
        caps.grant_range(at(a, 2), ObjectKind::IoPorts, 1, 1 << 16),
        Err(CapError::OutOfRange)
    );
    assert_eq!(caps.grant_range(at(a, 2), ObjectKind::Frame, 0, 1), Err(CapError::NotARange));
    assert_eq!(caps.create(at(a, 2), ObjectKind::Frame), Err(CapError::InvalidArgument));
    caps.audit().unwrap();
}

// ---- Random operation sequences against a model ----

const SPACES: u32 = 3;
const SLOTS: u32 = 6;

#[derive(Clone, Debug)]
enum Op {
    Create {
        at: (u32, u32),
        kind: usize,
    },
    Memory {
        at: (u32, u32),
        pages: u64,
    },
    Ports {
        at: (u32, u32),
        base: u64,
        len: u64,
    },
    Derive {
        from: (u32, u32),
        to: (u32, u32),
        rights: u16,
        badge: Option<u64>,
        range: Option<(u64, u64)>,
    },
    Seal {
        from: (u32, u32),
        key: (u32, u32),
        to: (u32, u32),
    },
    Unseal {
        from: (u32, u32),
        key: (u32, u32),
        to: (u32, u32),
    },
    Retype {
        from: (u32, u32),
        kind: usize,
        order: u32,
        to: (u32, u32),
        count: u32,
    },
    Move {
        from: (u32, u32),
        to: (u32, u32),
    },
    Delete {
        at: (u32, u32),
    },
    Revoke {
        at: (u32, u32),
        budget: usize,
    },
    Recreate {
        space: u32,
    },
}

/// A slot: (space, slot) as given, or, with a space number of `SPACES`,
/// the n-th of the slots that suit the operation (see `Run::resolve`), so
/// most operations find a capability to work on.
fn slot() -> impl Strategy<Value = (u32, u32)> {
    prop_oneof![1 => (0..SPACES, 0..SLOTS), 3 => (Just(SPACES), any::<u32>())]
}

/// What an operation wants in a slot it names.
#[derive(Clone, Copy)]
enum Want {
    Empty,
    Full(fn(&Capability) -> bool),
}

/// Mostly rights masks that keep `copy` and `transfer`, so chains grow.
fn mask() -> impl Strategy<Value = u16> {
    prop_oneof![
        3 => any::<u16>().prop_map(|bits| bits | Rights::COPY.bits() | Rights::TRANSFER.bits()),
        1 => any::<u16>(),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    // Endpoints twice as often as other kernel objects: they are the keys.
    let kind = prop_oneof![2..7usize, Just(5usize)];
    let made_from_memory = prop_oneof![Just(0usize), Just(1), Just(10), Just(1), Just(5)];
    prop_oneof![
        2 => (slot(), kind).prop_map(|(at, kind)| Op::Create { at, kind }),
        1 => (slot(), 1..17u64).prop_map(|(at, pages)| Op::Memory { at, pages }),
        1 => (slot(), 0..8u64, 0..9u64).prop_map(|(at, base, len)| Op::Ports { at, base, len }),
        8 => (slot(), slot(), mask(), proptest::option::weighted(0.2, 0..3u64),
              proptest::option::weighted(0.2, (0..5u64, 0..5u64)))
            .prop_map(|(from, to, rights, badge, range)| Op::Derive { from, to, rights, badge, range }),
        2 => (slot(), slot(), slot()).prop_map(|(from, key, to)| Op::Seal { from, key, to }),
        2 => (slot(), slot(), slot()).prop_map(|(from, key, to)| Op::Unseal { from, key, to }),
        3 => (slot(), made_from_memory, 0..3u32, slot(), prop_oneof![1 => Just(0u32), 6 => 1..3u32])
            .prop_map(|(from, kind, order, to, count)| Op::Retype { from, kind, order, to, count }),
        3 => (slot(), slot()).prop_map(|(from, to)| Op::Move { from, to }),
        3 => slot().prop_map(|at| Op::Delete { at }),
        2 => (slot(), 0..4usize).prop_map(|(at, budget)| Op::Revoke { at, budget }),
        1 => (0..SPACES).prop_map(|space| Op::Recreate { space }),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Node {
    cap: Capability,
    parent: Option<(u32, u32)>,
    depth: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Obj {
    caps: u32,
    children: u32,
    extent: Option<Extent>,
    watermark: u64,
    origin: Option<ObjectId>,
}

/// The derivation tree as plain maps, with each operation written out
/// the simplest way: what the store must do.
#[derive(Default)]
struct Model {
    nodes: HashMap<(u32, u32), Node>,
    objects: HashMap<ObjectId, Obj>,
    released: Vec<ObjectId>,
}

impl Model {
    fn children(&self, of: (u32, u32)) -> Vec<(u32, u32)> {
        self.nodes.iter().filter(|(_, n)| n.parent == Some(of)).map(|(&k, _)| k).collect()
    }

    fn descendants(&self, of: (u32, u32)) -> Vec<(u32, u32)> {
        let mut found = self.children(of);
        let mut i = 0;
        while i < found.len() {
            let more = self.children(found[i]);
            found.extend(more);
            i += 1;
        }
        found
    }

    fn may_derive(&self, from: (u32, u32), to: (u32, u32), cap: &Capability) -> bool {
        (from.0 == to.0 || cap.rights().contains(Rights::TRANSFER))
            && self.nodes[&from].depth < MAX_DEPTH
    }

    fn add(&mut self, at: (u32, u32), cap: Capability, parent: Option<(u32, u32)>) {
        let depth = parent.map_or(0, |p| self.nodes[&p].depth + 1);
        self.nodes.insert(at, Node { cap, parent, depth });
        if let Some(id) = cap.id() {
            self.objects.entry(id).or_insert(Obj {
                caps: 0,
                children: 0,
                extent: None,
                watermark: 0,
                origin: None,
            });
            self.objects.get_mut(&id).unwrap().caps += 1;
        }
    }

    fn remove(&mut self, at: (u32, u32)) {
        let node = self.nodes.remove(&at).unwrap();
        for child in self.children(at) {
            self.nodes.get_mut(&child).unwrap().parent = node.parent;
        }
        let Some(mut id) = node.cap.id() else { return };
        self.objects.get_mut(&id).unwrap().caps -= 1;
        loop {
            let obj = self.objects[&id];
            if obj.caps > 0 || obj.children > 0 {
                return;
            }
            self.objects.remove(&id);
            self.released.push(id);
            let Some(origin) = obj.origin else { return };
            let parent = self.objects.get_mut(&origin).unwrap();
            parent.children -= 1;
            if parent.children == 0 {
                parent.watermark = 0;
            }
            id = origin;
        }
    }

    fn clear_space(&mut self, space: u32) {
        for slot in 0..SLOTS {
            if self.nodes.contains_key(&(space, slot)) {
                self.remove((space, slot));
            }
        }
    }
}

struct Run {
    caps: Store,
    spaces: Vec<SpaceId>,
    model: Model,
}

impl Run {
    fn new() -> Run {
        let mut caps = store();
        let spaces = (0..SPACES).map(|_| caps.create_space(SLOTS).unwrap()).collect();
        Run { caps, spaces, model: Model::default() }
    }

    fn at(&self, (space, slot): (u32, u32)) -> SlotRef {
        SlotRef::new(self.spaces[space as usize], slot)
    }

    /// The slot `pick` names, as `slot()` describes.
    fn pick(&self, (space, n): (u32, u32), want: Want) -> (u32, u32) {
        if space < SPACES {
            return (space, n);
        }
        let all = (0..SPACES * SLOTS).map(|k| (k / SLOTS, k % SLOTS));
        let nodes = &self.model.nodes;
        let mut candidates: Vec<_> = match want {
            Want::Empty => all.filter(|k| !nodes.contains_key(k)).collect(),
            Want::Full(suits) => {
                all.clone().filter(|k| nodes.get(k).is_some_and(|n| suits(&n.cap))).collect()
            }
        };
        if candidates.is_empty() {
            candidates = (0..SPACES * SLOTS).map(|k| (k / SLOTS, k % SLOTS)).collect();
        }
        candidates[n as usize % candidates.len()]
    }

    /// `op` with its picked slots resolved.
    fn resolve(&self, op: &Op) -> Op {
        let any: fn(&Capability) -> bool = |_| true;
        let full = |pick, suits| self.pick(pick, Want::Full(suits));
        let empty = |pick| self.pick(pick, Want::Empty);
        match *op {
            Op::Create { at, kind } => Op::Create { at: empty(at), kind },
            Op::Memory { at, pages } => Op::Memory { at: empty(at), pages },
            Op::Ports { at, base, len } => Op::Ports { at: empty(at), base, len },
            Op::Derive { from, to, rights, badge, range } => {
                Op::Derive { from: full(from, any), to: empty(to), rights, badge, range }
            }
            Op::Seal { from, key, to } => Op::Seal {
                from: full(from, |c| c.sealed_by().is_none()),
                key: full(key, |c| c.kind() == ObjectKind::Endpoint),
                to: empty(to),
            },
            Op::Unseal { from, key, to } => {
                let from = full(from, |c| c.sealed_by().is_some());
                let sealer = self.model.nodes.get(&from).and_then(|n| n.cap.sealed_by());
                let key = match sealer {
                    Some(sealer) if key.0 == SPACES => {
                        let keys: Vec<_> = (0..SPACES * SLOTS)
                            .map(|k| (k / SLOTS, k % SLOTS))
                            .filter(|k| {
                                self.model.nodes.get(k).is_some_and(|n| n.cap.id() == Some(sealer))
                            })
                            .collect();
                        if keys.is_empty() {
                            full(key, any)
                        } else {
                            keys[key.1 as usize % keys.len()]
                        }
                    }
                    _ => full(key, any),
                };
                Op::Unseal { from, key, to: empty(to) }
            }
            Op::Retype { from, kind, order, to, count } => Op::Retype {
                from: full(from, |c| c.kind() == ObjectKind::Memory),
                kind,
                order,
                to: empty(to),
                count,
            },
            Op::Move { from, to } => Op::Move { from: full(from, any), to: empty(to) },
            Op::Delete { at } => Op::Delete { at: full(at, any) },
            Op::Revoke { at, budget } => {
                Op::Revoke { at: full(at, |c| c.rights().contains(Rights::COPY)), budget }
            }
            Op::Recreate { space } => Op::Recreate { space },
        }
    }

    /// Runs `op` on the store and the model; true if the store accepted it.
    fn apply(&mut self, op: &Op) -> bool {
        let op = &self.resolve(op);
        let mut released = Vec::new();
        let mut sink = |r: Released| released.push(r.id);
        let spaces = self.spaces.clone();
        let place = |(space, slot): (u32, u32)| SlotRef::new(spaces[space as usize], slot);
        let model = &mut self.model;
        let accepted = match *op {
            Op::Create { at, kind } => {
                let kind = ObjectKind::ALL[kind];
                let result = self.caps.create(place(at), kind);
                assert_eq!(result.is_ok(), !model.nodes.contains_key(&at));
                if let Ok(id) = result {
                    model.add(at, self.caps.get(place(at)).unwrap(), None);
                    assert_eq!(model.nodes[&at].cap.id(), Some(id));
                }
                result.is_ok()
            }
            Op::Memory { at, pages } => {
                let slot = place(at);
                let result = self.caps.create_memory(slot, 0x1000 + 0x40 * u64::from(at.1), pages);
                assert_eq!(result.is_ok(), !model.nodes.contains_key(&at));
                if let Ok(id) = result {
                    model.add(at, self.caps.get(slot).unwrap(), None);
                    let obj = model.objects.get_mut(&id).unwrap();
                    obj.extent = Some(Extent { base: 0x1000 + 0x40 * u64::from(at.1), pages });
                }
                result.is_ok()
            }
            Op::Ports { at, base, len } => {
                let slot = place(at);
                let result = self.caps.grant_range(slot, ObjectKind::IoPorts, base, len);
                assert_eq!(result.is_ok(), !model.nodes.contains_key(&at) && len > 0);
                if result.is_ok() {
                    model.add(at, self.caps.get(slot).unwrap(), None);
                }
                result.is_ok()
            }
            Op::Derive { from, to, rights, badge, range } => {
                let how = Derivation { rights: Rights::from_bits_truncate(rights), badge, range };
                let (src, dst) = (place(from), place(to));
                let result = self.caps.derive(src, dst, how);
                let expected = match model.nodes.get(&from) {
                    Some(node) if !model.nodes.contains_key(&to) => {
                        node.cap.derive(how).ok().filter(|_| model.may_derive(from, to, &node.cap))
                    }
                    _ => None,
                };
                assert_eq!(result.is_ok(), expected.is_some(), "{result:?}");
                if let Some(cap) = expected {
                    model.add(to, cap, Some(from));
                }
                result.is_ok()
            }
            Op::Seal { from, key, to } | Op::Unseal { from, key, to } => {
                let seal = matches!(op, Op::Seal { .. });
                let (src, k, dst) = (place(from), place(key), place(to));
                let result =
                    if seal { self.caps.seal(src, k, dst) } else { self.caps.unseal(src, k, dst) };
                let expected = match (model.nodes.get(&from), model.nodes.get(&key)) {
                    (Some(node), Some(key)) if !model.nodes.contains_key(&to) => {
                        let made = if seal {
                            node.cap.seal_with(&key.cap)
                        } else {
                            node.cap.unseal_with(&key.cap)
                        };
                        made.ok().filter(|_| model.may_derive(from, to, &node.cap))
                    }
                    _ => None,
                };
                assert_eq!(result.is_ok(), expected.is_some(), "{op:?}: {result:?}");
                if let Some(cap) = expected {
                    model.add(to, cap, Some(from));
                }
                result.is_ok()
            }
            Op::Retype { from, kind, order, to, count } => {
                let kind = ObjectKind::ALL[kind];
                let result = self.caps.retype(place(from), kind, order, place(to), count);
                let fits = count > 0
                    && to.1 + count <= SLOTS
                    && (to.1..to.1 + count).all(|s| !model.nodes.contains_key(&(to.0, s)));
                let expected = match model.nodes.get(&from) {
                    Some(node) if fits => {
                        let cap = node.cap;
                        let ok = cap.retyped(kind, ObjectId { index: 0, generation: 0 }).is_ok()
                            && model.may_derive(from, to, &cap);
                        let carved = ok
                            .then(|| {
                                let memory = model.objects[&cap.id().unwrap()];
                                let extent = memory.extent.unwrap();
                                carve(
                                    extent.base,
                                    extent.pages,
                                    memory.watermark,
                                    order,
                                    count.into(),
                                )
                                .ok()
                            })
                            .flatten();
                        carved.map(|(offset, watermark)| {
                            (cap, extent_of(model, &cap), offset, watermark)
                        })
                    }
                    _ => None,
                };
                assert_eq!(result.is_ok(), expected.is_some(), "{op:?}: {result:?}");
                if let Some((cap, base, offset, watermark)) = expected {
                    let memory = cap.id().unwrap();
                    for i in 0..count {
                        let slot = (to.0, to.1 + i);
                        let made = self.caps.get(place(slot)).unwrap();
                        assert_eq!(made, cap.retyped(kind, made.id().unwrap()).unwrap());
                        model.add(slot, made, Some(from));
                        let obj = model.objects.get_mut(&made.id().unwrap()).unwrap();
                        obj.extent = Some(Extent {
                            base: base + offset + (u64::from(i) << order),
                            pages: 1 << order,
                        });
                        obj.origin = Some(memory);
                    }
                    let parent = model.objects.get_mut(&memory).unwrap();
                    parent.children += count;
                    parent.watermark = watermark;
                }
                result.is_ok()
            }
            Op::Move { from, to } => {
                let result = self.caps.move_cap(place(from), place(to));
                let expected = match model.nodes.get(&from) {
                    Some(node) if !model.nodes.contains_key(&to) => {
                        from.0 == to.0 || node.cap.rights().contains(Rights::TRANSFER)
                    }
                    _ => false,
                };
                assert_eq!(result.is_ok(), expected, "{op:?}: {result:?}");
                if expected {
                    let node = model.nodes.remove(&from).unwrap();
                    for child in model.children(from) {
                        model.nodes.get_mut(&child).unwrap().parent = Some(to);
                    }
                    model.nodes.insert(to, node);
                }
                result.is_ok()
            }
            Op::Delete { at } => {
                let result = self.caps.delete(place(at), &mut sink);
                assert_eq!(result.is_ok(), model.nodes.contains_key(&at));
                if result.is_ok() {
                    model.remove(at);
                }
                result.is_ok()
            }
            Op::Revoke { at, budget } => {
                let slot = place(at);
                let mut result = self.caps.revoke(slot, budget, &mut sink);
                assert_eq!(result.is_ok(), model.nodes.contains_key(&at));
                let free = (0..SPACES * SLOTS)
                    .map(|n| (n / SLOTS, n % SLOTS))
                    .find(|k| !model.nodes.contains_key(k));
                // Finish it in steps of at least one, auditing in between;
                // meanwhile nothing can be made from it.
                while result == Ok(Progress::More) {
                    self.caps.audit().unwrap();
                    if let Some(free) = free {
                        assert!(self.caps.derive(slot, place(free), Derivation::COPY).is_err());
                    }
                    result = self.caps.revoke(slot, budget.max(1), &mut sink);
                }
                if result.is_ok() {
                    // Deepest first, as the store does.
                    let mut below = model.descendants(at);
                    below.sort_by_key(|k| std::cmp::Reverse(model.nodes[k].depth));
                    for node in below {
                        model.remove(node);
                    }
                }
                result.is_ok()
            }
            Op::Recreate { space } => {
                self.caps.destroy_space(spaces[space as usize], &mut sink).unwrap();
                model.clear_space(space);
                let id = self.caps.create_space(SLOTS).unwrap();
                assert_ne!(id, self.spaces[space as usize]);
                self.spaces[space as usize] = id;
                true
            }
        };
        released.sort_by_key(|id| (id.index, id.generation));
        self.model.released.sort_by_key(|id| (id.index, id.generation));
        assert_eq!(released, std::mem::take(&mut self.model.released), "released by {op:?}");
        accepted
    }

    fn compare(&self) {
        let census = self.caps.audit().unwrap_or_else(|err| panic!("audit: {err}"));
        assert_eq!(census.total_caps(), self.model.nodes.len() as u64);
        assert_eq!(census.total_objects(), self.model.objects.len() as u64);
        for space in 0..SPACES {
            for slot in 0..SLOTS {
                let at = self.at((space, slot));
                match self.model.nodes.get(&(space, slot)) {
                    None => assert_eq!(self.caps.get(at), Err(CapError::Empty)),
                    Some(node) => {
                        assert_eq!(self.caps.get(at), Ok(node.cap));
                        assert_eq!(self.caps.parent(at), Ok(node.parent.map(|p| self.at(p))));
                        assert_eq!(self.caps.depth(at), Ok(node.depth));
                    }
                }
            }
        }
        for (&id, obj) in &self.model.objects {
            let info = self.caps.object(id).expect("the model's object exists");
            assert_eq!(
                (info.caps, info.children, info.extent, info.watermark, info.origin),
                (obj.caps, obj.children, obj.extent, obj.watermark, obj.origin),
                "object {id}"
            );
        }
    }
}

fn extent_of(model: &Model, cap: &Capability) -> u64 {
    model.objects[&cap.id().unwrap()].extent.unwrap().base
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// Every operation does what the model says, and leaves the store
    /// consistent: after each one the audit passes and the store holds the
    /// model's capabilities, with the model's parents, depths and object
    /// counts.
    #[test]
    fn the_store_follows_the_model(ops in proptest::collection::vec(op(), 1..60)) {
        let mut run = Run::new();
        for op in &ops {
            run.apply(op);
            run.compare();
        }
        // Tearing everything down releases every object.
        for space in 0..SPACES {
            let mut sink = |_| {};
            run.caps.destroy_space(run.spaces[space as usize], &mut sink).unwrap();
        }
        prop_assert_eq!(run.caps.census(), Census::default());
    }

    /// A cache of capabilities looked up with `get_to_cache`, emptied
    /// whenever the store says it is stale, never holds a capability its
    /// slot no longer has: what the kernel's per-CPU caches rely on.
    #[test]
    fn a_cache_the_store_keeps_fresh_is_never_wrong(
        ops in proptest::collection::vec((op(), slot()), 1..60)
    ) {
        let mut run = Run::new();
        let mut cache = HashMap::new();
        for (op, look) in &ops {
            run.apply(op);
            if run.caps.take_stale() {
                cache.clear();
            }
            for (&at, cap) in &cache {
                prop_assert_eq!(run.caps.get(at), Ok(*cap), "after {:?}", op);
            }
            let at = run.at(run.pick(*look, Want::Full(|_| true)));
            if let Ok(cap) = run.caps.get_to_cache(at) {
                cache.insert(at, cap);
            }
        }
    }

    /// Whatever the sequence, no capability in the store holds a right
    /// its kind lacks or grants more than its parent, and none is further
    /// from a root than its depth says.
    #[test]
    fn no_sequence_widens_rights(ops in proptest::collection::vec(op(), 1..60)) {
        let mut run = Run::new();
        for op in &ops {
            run.apply(op);
        }
        for space in 0..SPACES {
            for slot in 0..SLOTS {
                let at = run.at((space, slot));
                let Ok(cap) = run.caps.get(at) else { continue };
                prop_assert!(cap.kind().rights().contains(cap.rights()));
                let mut steps = 0u8;
                let mut below = cap;
                let mut up = run.caps.parent(at).unwrap();
                while let Some(parent) = up {
                    let above = run.caps.get(parent).unwrap();
                    prop_assert!(above.rights().contains(below.rights()));
                    steps += 1;
                    below = above;
                    up = run.caps.parent(parent).unwrap();
                }
                prop_assert!(steps <= run.caps.depth(at).unwrap());
            }
        }
    }
}
