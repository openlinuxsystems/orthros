// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The `caps` self-test: the capability core's test suite, run in the
//! kernel on its own store and heap.
//!
//! It works in capability spaces of its own, which it destroys at the
//! end, and checks, in order:
//!
//! 1. forging: naming an empty slot, a slot past the end, another or a
//!    stale space, an object of another kind, or a deleted capability
//!    gets nothing, and no operation overwrites a capability;
//! 2. widening: for every one of the 4,096 rights masks, a capability
//!    derived with the mask, and anything derived from that, holds the
//!    original's rights and the mask's and no others, and every use
//!    needing another right is refused; copying needs `copy`, and handing
//!    to another space needs `transfer`;
//! 3. badges, sealing and ranges: badges are set once, a sealed capability
//!    is useless without its key, ranges only shrink;
//! 4. memory: frames, DMA buffers and memory retyped from a memory object
//!    lie inside it and never overlap, and its pages go back to the frame
//!    allocator when its last capability goes;
//! 5. revocation: a revoke reaches every space, survives deletes in the
//!    middle of the tree, works in steps, and refuses derivations below it
//!    while it runs; derivation depth is bounded;
//! 6. random operations, with the store's full audit in between;
//! 7. all CPUs at once: the other CPUs keep deriving from a tree while
//!    the boot CPU revokes it, and nothing they made survives.
//!
//! Then it checks that the store, the heap and the frame allocator are as
//! they were. It reports `selftest caps: passed` with the counts.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use radix_cap::{
    CapError, Capability, Derivation, Extent, MAX_DEPTH, ObjectKind, Progress, Rights, SlotRef,
    SpaceId,
};
use radix_hal::{CpuLocal, MAX_CPUS, Smp, Timer};
use radix_klog::{error, info};
use radix_mm::{FRAMES, HEAP, Owner};

use crate::caps::{self, Locked};
use crate::{Arch, work};

/// What went wrong, and the error the store gave if that was the surprise.
struct Failure {
    what: &'static str,
    got: Option<CapError>,
}

type Check<T = ()> = Result<T, Failure>;

/// Counts for the report.
#[derive(Default)]
struct Tally {
    /// Attempts to use or make a capability without holding one, refused.
    forgeries: u64,
    /// Attempts to gain a right, a badge, a range or a use not held, refused.
    widenings: u64,
    /// Random operations run, and how many the store refused.
    random: u64,
    random_refused: u64,
    /// Derivations tried by other CPUs during the revoke, those refused
    /// as revoking, the CPUs, and the revoke's steps.
    concurrent: (u64, u64, usize, u64),
}

fn fail<T>(what: &'static str) -> Check<T> {
    Err(Failure { what, got: None })
}

fn ensure(condition: bool, what: &'static str) -> Check {
    if condition { Ok(()) } else { fail(what) }
}

/// Runs `f` on the store with its lock held.
fn op<R>(f: impl FnOnce(&mut Locked) -> Result<R, CapError>) -> Result<R, CapError> {
    caps::with(f).and_then(|result| result)
}

fn ok<T>(result: Result<T, CapError>, what: &'static str) -> Check<T> {
    result.map_err(|err| Failure { what, got: Some(err) })
}

/// Checks that `result` is the refusal `want`, and counts it.
fn refused<T>(
    result: Result<T, CapError>,
    want: CapError,
    what: &'static str,
    count: &mut u64,
) -> Check {
    match result.err() {
        Some(err) if err == want => {
            *count += 1;
            Ok(())
        }
        Some(err) => Err(Failure { what, got: Some(err) }),
        None => fail(what),
    }
}

fn audit() -> Check {
    match caps::with(|store| store.audit()) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(inconsistency)) => {
            error!("selftest caps: the audit found: {inconsistency}");
            fail("the store's audit failed")
        }
        Err(err) => Err(Failure { what: "the store is not set up", got: Some(err) }),
    }
}

fn space(slots: u32) -> Check<SpaceId> {
    ok(op(|store| store.create_space(slots)), "a capability space could not be made")
}

fn at(space: SpaceId, slot: u32) -> SlotRef {
    SlotRef::new(space, slot)
}

fn get(slot: SlotRef) -> Result<Capability, CapError> {
    op(|store| store.get(slot))
}

fn derive(from: SlotRef, to: SlotRef, how: Derivation) -> Result<(), CapError> {
    op(|store| store.derive(from, to, how))
}

/// A small, fast pseudo-random generator (xorshift64*).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Runs the self-test.
pub fn run() {
    info!(
        "selftest caps: forging, widening, badges, sealing, ranges, memory, revocation, random and concurrent operations"
    );
    let census = |store: &mut Locked| store.census();
    let frames = || FRAMES.owner_pages(Owner::Memory);
    HEAP.drain();
    let before = (caps::with(census).ok(), frames(), HEAP.stats());
    let started = Arch::now_ns();
    let mut tally = Tally::default();
    let result = forging(&mut tally)
        .and_then(|()| widening(&mut tally))
        .and_then(|()| badges_seals_and_ranges(&mut tally))
        .and_then(|()| memory(&mut tally))
        .and_then(|()| revocation(&mut tally))
        .and_then(|()| random(&mut tally))
        .and_then(|()| concurrent(&mut tally));
    if let Err(failure) = result {
        match failure.got {
            Some(err) => error!("selftest caps: failed; {} (the store said: {err})", failure.what),
            None => error!("selftest caps: failed; {}", failure.what),
        }
        return;
    }
    HEAP.drain();
    let after = (caps::with(census).ok(), frames(), HEAP.stats());
    let millis = (Arch::now_ns() - started) / 1_000_000;
    if after.0 != before.0 {
        error!("selftest caps: failed; capabilities or objects are left in the store");
        return;
    }
    if after.1 != before.1 {
        error!(
            "selftest caps: failed; {} pages are held by memory objects, {} before",
            after.1, before.1
        );
        return;
    }
    // The store keeps its two tables, which may have grown.
    let held = |stats: &radix_mm::HeapStats| stats.objects + stats.large_blocks;
    if held(&after.2) > held(&before.2) + 2 {
        error!(
            "selftest caps: failed; the heap holds {} more objects than before",
            held(&after.2) - held(&before.2)
        );
        return;
    }
    let (tried, revoking, cpus, steps) = tally.concurrent;
    info!(
        "selftest caps: {} forgeries and {} widenings refused; {} random operations ({} refused); \
         {tried} derivations by {cpus} other CPUs during a {steps}-step revoke ({revoking} refused \
         as revoking)",
        tally.forgeries, tally.widenings, tally.random, tally.random_refused
    );
    info!(
        "selftest caps: passed in {}.{:03} s; no capability was forged or widened, and nothing \
         leaked",
        millis / 1000,
        millis % 1000
    );
}

// ---- 1. Forging ----

fn forging(tally: &mut Tally) -> Check {
    let a = space(16)?;
    let b = space(16)?;
    let n = &mut tally.forgeries;
    ok(op(|store| store.create(at(a, 0), ObjectKind::Endpoint)), "an endpoint could not be made")?;
    refused(get(at(a, 1)), CapError::Empty, "an empty slot gave a capability", n)?;
    refused(get(at(a, 16)), CapError::NoSuchSlot, "a slot past the end gave a capability", n)?;
    refused(get(at(a, u32::MAX)), CapError::NoSuchSlot, "slot 2^32-1 gave a capability", n)?;
    refused(get(at(b, 0)), CapError::Empty, "a capability showed up in another space", n)?;
    let forged = SpaceId { index: a.index, generation: a.generation.wrapping_add(1) };
    refused(get(at(forged, 0)), CapError::NoSuchSpace, "a made-up space name worked", n)?;
    let nowhere = SpaceId { index: u32::MAX, generation: 1 };
    refused(get(at(nowhere, 0)), CapError::NoSuchSpace, "a space that never was worked", n)?;
    let gone = space(4)?;
    ok(op(|store| store.create(at(gone, 0), ObjectKind::Thread)), "a thread could not be made")?;
    ok(caps::destroy_space(gone), "a space could not be destroyed")?;
    refused(get(at(gone, 0)), CapError::NoSuchSpace, "a destroyed space's capability lived on", n)?;
    let reused = space(4)?;
    refused(get(at(gone, 0)), CapError::NoSuchSpace, "an old space name reached its successor", n)?;
    refused(get(at(reused, 0)), CapError::Empty, "a new space inherited a capability", n)?;
    ok(caps::destroy_space(reused), "a space could not be destroyed")?;
    let wrong = ObjectKind::Frame;
    refused(
        op(|store| store.lookup(at(a, 0), wrong, Rights::NONE)),
        CapError::WrongKind { expected: wrong, found: ObjectKind::Endpoint },
        "an endpoint capability was used as a frame",
        n,
    )?;
    refused(
        op(|store| store.create(at(a, 0), ObjectKind::Endpoint)),
        CapError::Occupied,
        "a capability was overwritten by a new object",
        n,
    )?;
    ok(derive(at(a, 0), at(a, 1), Derivation::COPY), "a copy was refused")?;
    refused(
        derive(at(a, 0), at(a, 1), Derivation::COPY),
        CapError::Occupied,
        "a copy overwrote a capability",
        n,
    )?;
    refused(
        op(|store| store.move_cap(at(a, 0), at(a, 1))),
        CapError::Occupied,
        "a move overwrote a capability",
        n,
    )?;
    ok(caps::delete(at(a, 1)), "a delete was refused")?;
    refused(get(at(a, 1)), CapError::Empty, "a deleted capability lived on", n)?;
    refused(
        derive(at(a, 1), at(a, 2), Derivation::COPY),
        CapError::Empty,
        "a deleted capability was copied",
        n,
    )?;
    refused(caps::delete(at(a, 1)), CapError::Empty, "a capability was deleted twice", n)?;
    // Moving keeps nothing behind.
    ok(op(|store| store.move_cap(at(a, 0), at(b, 0))), "a move was refused")?;
    refused(get(at(a, 0)), CapError::Empty, "a moved capability stayed behind", n)?;
    ok(caps::destroy_space(a), "a space could not be destroyed")?;
    ok(caps::destroy_space(b), "a space could not be destroyed")?;
    audit()
}

// ---- 2. Widening ----

fn widening(tally: &mut Tally) -> Check {
    let a = space(4)?;
    let b = space(4)?;
    let n = &mut tally.widenings;
    ok(op(|store| store.create(at(a, 0), ObjectKind::Endpoint)), "an endpoint could not be made")?;
    let original = ObjectKind::Endpoint.rights();
    for bits in 0..=Rights::ALL.bits() {
        let mask = Rights::from_bits_truncate(bits);
        let expected = original & mask;
        ok(
            derive(at(a, 0), at(a, 1), Derivation::with_rights(mask)),
            "a derivation with a mask was refused",
        )?;
        let derived = ok(get(at(a, 1)), "a derived capability is missing")?;
        ensure(derived.rights() == expected, "a derived capability has other rights than asked")?;
        // Asking for everything from the derived one gives nothing more.
        match derive(at(a, 1), at(a, 2), Derivation::COPY) {
            Ok(()) => {
                let again = ok(get(at(a, 2)), "a copy is missing")?;
                ensure(again.rights() == expected, "a copy of a copy gained rights")?;
                ensure(expected.contains(Rights::COPY), "a capability without copy was copied")?;
            }
            Err(CapError::MissingRights(missing)) if missing == Rights::COPY => *n += 1,
            Err(err) => return Err(Failure { what: "a copy failed oddly", got: Some(err) }),
        }
        for (right, _) in Rights::NAMES {
            if !expected.contains(right) {
                refused(
                    op(|store| store.lookup(at(a, 1), ObjectKind::Endpoint, right)),
                    CapError::MissingRights(right),
                    "a capability was used with a right it lacks",
                    n,
                )?;
            }
        }
        if !expected.contains(Rights::TRANSFER) && expected.contains(Rights::COPY) {
            refused(
                derive(at(a, 1), at(b, 0), Derivation::COPY),
                CapError::MissingRights(Rights::TRANSFER),
                "a capability without transfer went to another space",
                n,
            )?;
            refused(
                op(|store| store.move_cap(at(a, 1), at(b, 0))),
                CapError::MissingRights(Rights::TRANSFER),
                "a capability without transfer moved to another space",
                n,
            )?;
        }
        ensure(
            ok(caps::revoke(at(a, 0)), "a revoke was refused")? == 1,
            "a small revoke took steps",
        )?;
        ensure(get(at(a, 1)) == Err(CapError::Empty), "a revoke left a capability")?;
    }
    ok(caps::destroy_space(a), "a space could not be destroyed")?;
    ok(caps::destroy_space(b), "a space could not be destroyed")?;
    audit()
}

// ---- 3. Badges, sealing and ranges ----

fn badges_seals_and_ranges(tally: &mut Tally) -> Check {
    let a = space(16)?;
    let n = &mut tally.widenings;
    ok(op(|store| store.create(at(a, 0), ObjectKind::Endpoint)), "an endpoint could not be made")?;
    ok(derive(at(a, 0), at(a, 1), Derivation::with_badge(42)), "a badge was refused")?;
    ensure(get(at(a, 1)).map(|c| c.badge()) == Ok(42), "the badge did not stick")?;
    refused(
        derive(at(a, 1), at(a, 2), Derivation::with_badge(43)),
        CapError::AlreadyBadged,
        "a badge was changed",
        n,
    )?;
    refused(
        derive(at(a, 0), at(a, 2), Derivation::with_badge(0)),
        CapError::InvalidBadge,
        "badge 0 was given",
        n,
    )?;
    ok(derive(at(a, 1), at(a, 2), Derivation::COPY), "a badged copy was refused")?;
    ensure(get(at(a, 2)).map(|c| c.badge()) == Ok(42), "a copy lost its badge")?;
    // Sealing: a process capability sealed with the endpoint as key.
    ok(op(|store| store.create(at(a, 3), ObjectKind::Process)), "a process could not be made")?;
    ok(op(|store| store.create(at(a, 4), ObjectKind::Endpoint)), "an endpoint could not be made")?;
    let seal_only = Rights::SEAL | Rights::COPY;
    ok(
        derive(at(a, 0), at(a, 5), Derivation::with_rights(seal_only)),
        "a sealer could not be made",
    )?;
    ok(op(|store| store.seal(at(a, 3), at(a, 5), at(a, 6))), "sealing was refused")?;
    refused(
        op(|store| store.lookup(at(a, 6), ObjectKind::Process, Rights::NONE)),
        CapError::Sealed,
        "a sealed capability was used",
        n,
    )?;
    refused(
        derive(at(a, 6), at(a, 7), Derivation::with_rights(Rights::SIGNAL | Rights::COPY)),
        CapError::Sealed,
        "a sealed capability was changed",
        n,
    )?;
    refused(
        op(|store| store.seal(at(a, 6), at(a, 5), at(a, 7))),
        CapError::Sealed,
        "a capability was sealed twice",
        n,
    )?;
    refused(
        op(|store| store.unseal(at(a, 6), at(a, 4), at(a, 7))),
        CapError::WrongKey,
        "another key unsealed",
        n,
    )?;
    refused(
        op(|store| store.unseal(at(a, 6), at(a, 5), at(a, 7))),
        CapError::MissingRights(Rights::UNSEAL),
        "a key without unseal unsealed",
        n,
    )?;
    refused(
        op(|store| store.seal(at(a, 3), at(a, 3), at(a, 7))),
        CapError::WrongKind { expected: ObjectKind::Endpoint, found: ObjectKind::Process },
        "a process was used as a key",
        n,
    )?;
    ok(
        derive(at(a, 6), at(a, 7), Derivation::COPY),
        "an exact copy of a sealed capability was refused",
    )?;
    ok(op(|store| store.unseal(at(a, 7), at(a, 0), at(a, 8))), "the key did not unseal")?;
    ensure(get(at(a, 8)) == get(at(a, 3)), "unsealing did not give back the original")?;
    // Ranges: I/O ports for the test only; nothing touches the hardware.
    ok(
        op(|store| store.grant_range(at(a, 9), ObjectKind::IoPorts, 0, 1 << 16)),
        "ports were refused",
    )?;
    ok(
        derive(at(a, 9), at(a, 10), Derivation::with_range(0x60, 5)),
        "a smaller range was refused",
    )?;
    ensure(get(at(a, 10)).map(|c| c.range()) == Ok(Some((0x60, 5))), "the range is wrong")?;
    for (offset, len) in [(0, 6), (5, 1), (1, 0), (u64::MAX, 2), (4, u64::MAX)] {
        refused(
            derive(at(a, 10), at(a, 11), Derivation::with_range(offset, len)),
            CapError::OutOfRange,
            "a range grew",
            n,
        )?;
    }
    refused(
        op(|store| store.grant_range(at(a, 11), ObjectKind::IoPorts, 1, 1 << 16)),
        CapError::OutOfRange,
        "ports past 0xffff were given",
        n,
    )?;
    refused(
        derive(at(a, 0), at(a, 11), Derivation::with_range(0, 1)),
        CapError::NotARange,
        "an endpoint got a range",
        n,
    )?;
    refused(
        derive(at(a, 9), at(a, 11), Derivation::with_badge(1)),
        CapError::NotBadgeable(ObjectKind::IoPorts),
        "ports got a badge",
        n,
    )?;
    ok(caps::destroy_space(a), "a space could not be destroyed")?;
    audit()
}

// ---- 4. Memory ----

fn memory(tally: &mut Tally) -> Check {
    let a = space(32)?;
    let n = &mut tally.widenings;
    let pages_before = FRAMES.owner_pages(Owner::Memory);
    let memory = ok(caps::create_memory(at(a, 0), 4), "a memory object could not be made")?;
    ensure(
        FRAMES.owner_pages(Owner::Memory) == pages_before + 16,
        "the memory object's pages are not accounted",
    )?;
    let extent = ok(
        caps::with(|store| store.object(memory))
            .ok()
            .flatten()
            .and_then(|info| info.extent)
            .ok_or(CapError::Empty),
        "the memory object has no extent",
    )?;
    ok(
        op(|store| store.retype(at(a, 0), ObjectKind::Frame, 0, at(a, 1), 4)),
        "frames were refused",
    )?;
    ok(
        op(|store| store.retype(at(a, 0), ObjectKind::Dma, 1, at(a, 5), 2)),
        "DMA buffers were refused",
    )?;
    ok(
        op(|store| store.retype(at(a, 0), ObjectKind::Memory, 2, at(a, 7), 1)),
        "a smaller memory was refused",
    )?;
    ok(
        op(|store| store.retype(at(a, 7), ObjectKind::Frame, 1, at(a, 8), 2)),
        "frames from it were refused",
    )?;
    refused(
        op(|store| store.retype(at(a, 0), ObjectKind::Frame, 3, at(a, 10), 1)),
        CapError::Exhausted,
        "a memory object gave more than it has",
        n,
    )?;
    refused(
        op(|store| store.retype(at(a, 0), ObjectKind::Endpoint, 0, at(a, 10), 1)),
        CapError::InvalidArgument,
        "memory became an endpoint",
        n,
    )?;
    refused(
        op(|store| store.retype(at(a, 1), ObjectKind::Frame, 0, at(a, 10), 1)),
        CapError::WrongKind { expected: ObjectKind::Memory, found: ObjectKind::Frame },
        "a frame was retyped",
        n,
    )?;
    ok(
        derive(at(a, 0), at(a, 11), Derivation::with_rights(Rights::ALL - Rights::CONTROL)),
        "a copy was refused",
    )?;
    refused(
        op(|store| store.retype(at(a, 11), ObjectKind::Frame, 0, at(a, 12), 1)),
        CapError::MissingRights(Rights::CONTROL),
        "memory without control was retyped",
        n,
    )?;
    ok(
        derive(
            at(a, 0),
            at(a, 12),
            Derivation::with_rights(Rights::READ | Rights::CONTROL | Rights::COPY),
        ),
        "a copy was refused",
    )?;
    ok(
        op(|store| store.retype(at(a, 12), ObjectKind::Frame, 0, at(a, 13), 1)),
        "read-only memory could not be retyped",
    )?;
    // Every object made lies inside the memory and none overlap.
    let mut extents = [Extent { base: 0, pages: 0 }; 10];
    let mut found = 0;
    for slot in (1..11).chain(13..14) {
        if let Ok(cap) = get(at(a, slot))
            && let Some(id) = cap.id()
        {
            let info = caps::with(|store| store.object(id)).ok().flatten();
            let Some(own) = info.and_then(|info| info.extent) else {
                return fail("a retyped object has no extent");
            };
            ensure(
                own.base >= extent.base && own.base + own.pages <= extent.base + extent.pages,
                "a retyped object lies outside its memory",
            )?;
            if cap.kind() != ObjectKind::Memory && found < extents.len() {
                extents[found] = own;
                found += 1;
            }
        }
    }
    for (i, x) in extents[..found].iter().enumerate() {
        for y in &extents[i + 1..found] {
            ensure(
                x.base + x.pages <= y.base || y.base + y.pages <= x.base,
                "two retyped objects overlap",
            )?;
        }
    }
    let frame = ok(get(at(a, 13)), "a frame from read-only memory is missing")?;
    ensure(
        frame.rights() == Rights::READ | Rights::COPY,
        "a frame from read-only memory got rights",
    )?;
    audit()?;
    // Revoking the memory takes everything made from it, and it can be
    // used again from the start.
    ok(caps::revoke(at(a, 0)), "a revoke was refused")?;
    let info = caps::with(|store| store.object(memory)).ok().flatten();
    ensure(
        info.is_some_and(|info| info.children == 0 && info.watermark == 0),
        "revoked memory is still in use",
    )?;
    ok(
        op(|store| store.retype(at(a, 0), ObjectKind::Frame, 4, at(a, 1), 1)),
        "revoked memory could not be reused",
    )?;
    // Its pages go back when the last capability goes.
    ok(caps::destroy_space(a), "a space could not be destroyed")?;
    ensure(
        FRAMES.owner_pages(Owner::Memory) == pages_before,
        "a memory object's pages were not given back",
    )?;
    ensure(
        caps::with(|store| store.object(memory)).ok().flatten().is_none(),
        "a memory object outlived its capabilities",
    )?;
    audit()
}

// ---- 5. Revocation ----

fn revocation(tally: &mut Tally) -> Check {
    let spaces = [space(128)?, space(128)?, space(128)?];
    ok(
        op(|store| store.create(at(spaces[0], 0), ObjectKind::Notification)),
        "a notification could not be made",
    )?;
    // A tree of 300 capabilities over the three spaces, each derived from
    // a random earlier one.
    let mut random = Random(0x0cab_1e55);
    let mut placed = [(0usize, 0u32); 300];
    let mut count = 1;
    while count < placed.len() {
        let (space, slot) = placed[random.below(count as u64) as usize];
        let to = (count % 3, (count / 3 + 1) as u32);
        let from = at(spaces[space], slot);
        if derive(from, at(spaces[to.0], to.1), Derivation::COPY).is_ok() {
            placed[count] = to;
            count += 1;
        } else {
            return fail("a tree could not be built");
        }
    }
    // Deleting capabilities in the middle keeps their children revocable.
    for &(space, slot) in placed[1..].iter().step_by(7) {
        ok(caps::delete(at(spaces[space], slot)), "a delete in the tree was refused")?;
    }
    audit()?;
    // A revoke in steps: in between, nothing below the top can be used to
    // derive, and the top cannot move.
    let top = at(spaces[0], 0);
    let step = ok(op(|store| store.revoke(top, 20)), "a revoke was refused")?;
    ensure(step == Progress::More, "a revoke of 250 capabilities finished in one step of 20")?;
    let n = &mut tally.widenings;
    for &(space, slot) in &placed[1..] {
        let from = at(spaces[space], slot);
        if get(from).is_ok() {
            refused(
                derive(from, at(spaces[2], 127), Derivation::COPY),
                CapError::Revoking,
                "a capability was derived during a revoke",
                n,
            )?;
        }
    }
    refused(
        derive(top, at(spaces[2], 127), Derivation::COPY),
        CapError::Revoking,
        "the revoked capability was copied during its revoke",
        n,
    )?;
    refused(
        op(|store| store.move_cap(top, at(spaces[2], 127))),
        CapError::Revoking,
        "a capability moved during its revoke",
        n,
    )?;
    audit()?;
    let steps = ok(caps::revoke(top), "a revoke was refused")?;
    ensure(steps >= 3, "the rest of a revoke of 256 capabilities took fewer than 3 steps of 64")?;
    for &(space, slot) in &placed[1..] {
        ensure(
            get(at(spaces[space], slot)) == Err(CapError::Empty),
            "a capability survived the revoke",
        )?;
    }
    ensure(get(top).is_ok(), "the revoke took the capability itself")?;
    ok(
        derive(top, at(spaces[2], 127), Derivation::COPY),
        "a capability could not be copied after its revoke",
    )?;
    // Depth is bounded.
    let chain = space(MAX_DEPTH as u32 + 2)?;
    ok(op(|store| store.create(at(chain, 0), ObjectKind::Thread)), "a thread could not be made")?;
    for slot in 1..=u32::from(MAX_DEPTH) {
        ok(
            derive(at(chain, slot - 1), at(chain, slot), Derivation::COPY),
            "a chain could not be built",
        )?;
    }
    refused(
        derive(
            at(chain, u32::from(MAX_DEPTH)),
            at(chain, u32::from(MAX_DEPTH) + 1),
            Derivation::COPY,
        ),
        CapError::TooDeep,
        "a derivation went deeper than the limit",
        n,
    )?;
    for space in spaces.into_iter().chain([chain]) {
        ok(caps::destroy_space(space), "a space could not be destroyed")?;
    }
    audit()
}

// ---- 6. Random operations ----

const RANDOM_OPERATIONS: u64 = 100_000;
const RANDOM_SPACES: usize = 4;
const RANDOM_SLOTS: u32 = 32;

fn random(tally: &mut Tally) -> Check {
    let mut spaces = [SpaceId { index: 0, generation: 0 }; RANDOM_SPACES];
    for space in &mut spaces {
        *space = self::space(RANDOM_SLOTS)?;
    }
    let mut random = Random(0x5eed_cab5);
    let pick = |random: &mut Random| {
        at(
            spaces[random.below(RANDOM_SPACES as u64) as usize],
            random.below(u64::from(RANDOM_SLOTS)) as u32,
        )
    };
    for done in 0..RANDOM_OPERATIONS {
        let (a, b, c) = (pick(&mut random), pick(&mut random), pick(&mut random));
        let roll = random.below(100);
        let result = match roll {
            0..8 => {
                let kind = [
                    ObjectKind::Endpoint,
                    ObjectKind::Notification,
                    ObjectKind::Process,
                    ObjectKind::Thread,
                ][random.below(4) as usize];
                op(|store| store.create(a, kind)).map(drop)
            }
            8..10 => caps::create_memory(a, random.below(3) as u32).map(drop),
            10..40 => {
                let mask = Rights::from_bits_truncate(random.next() as u16)
                    | if random.below(4) > 0 {
                        Rights::COPY | Rights::TRANSFER
                    } else {
                        Rights::NONE
                    };
                let badge = (random.below(5) == 0).then(|| random.below(3));
                derive(a, b, Derivation { rights: mask, badge, range: None })
            }
            40..48 => op(|store| store.seal(a, b, c)),
            48..54 => op(|store| store.unseal(a, b, c)),
            54..62 => {
                let kind = [ObjectKind::Frame, ObjectKind::Dma, ObjectKind::Memory]
                    [random.below(3) as usize];
                let (order, count) = (random.below(2) as u32, 1 + random.below(2) as u32);
                op(|store| store.retype(a, kind, order, b, count))
            }
            62..76 => op(|store| store.move_cap(a, b)),
            76..92 => caps::delete(a),
            _ => caps::revoke(a).map(drop),
        };
        tally.random += 1;
        if result.is_err() {
            tally.random_refused += 1;
        }
        // Whatever was made grants no more than what it was made from.
        if result.is_ok() && (10..62).contains(&roll) {
            let made = if (40..54).contains(&roll) { c } else { b };
            let pair = caps::with(|store| {
                let parent = store.parent(made).ok().flatten()?;
                Some((store.get(made).ok()?, store.get(parent).ok()?))
            });
            match pair {
                Ok(Some((cap, parent))) => ensure(
                    cap.within(&parent)
                        || (parent.kind() == ObjectKind::Memory
                            && parent.rights().contains(cap.rights())),
                    "a capability grants more than the one it was made from",
                )?,
                _ => return fail("a capability that was made has no parent"),
            }
        }
        if done % 1024 == 1023 {
            audit()?;
        }
    }
    for space in spaces {
        ok(caps::destroy_space(space), "a space could not be destroyed")?;
    }
    audit()
}

// ---- 7. All CPUs at once ----

/// The tree the boot CPU revokes: node `n` is slot `n / 2` of space
/// `n % 2`, node 0 its root.
static TREE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
const TREE_SLOTS: u32 = 2048;
const NODES: u32 = 2 * TREE_SLOTS;
/// Each other CPU's own space, packed as in [`pack`].
static OWN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
const OWN_SLOTS: u32 = 512;
/// The CPU that revokes.
static REVOKER: AtomicUsize = AtomicUsize::new(0);
/// Other CPUs that have started deriving, and how many there are.
static READY: AtomicUsize = AtomicUsize::new(0);
static DERIVERS: AtomicUsize = AtomicUsize::new(0);
/// Set when the revoke is done.
static STOP: AtomicBool = AtomicBool::new(false);
/// Derivations that succeeded, were refused as revoking, found an empty
/// source or a full destination, or failed in a way they should not.
static OUTCOMES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// The revoke's steps.
static STEPS: AtomicU64 = AtomicU64::new(0);

fn pack(space: SpaceId) -> u64 {
    u64::from(space.index) << 32 | u64::from(space.generation)
}

fn unpack(packed: u64) -> SpaceId {
    SpaceId { index: (packed >> 32) as u32, generation: packed as u32 }
}

fn node(tree: [SpaceId; 2], n: u32) -> SlotRef {
    at(tree[(n % 2) as usize], n / 2)
}

fn others() -> impl Iterator<Item = usize> {
    let me = Arch::current_cpu();
    (0..Arch::cpu_count())
        .filter(move |&cpu| cpu != me)
        .filter(|&cpu| Arch::cpu_info(cpu).is_some_and(|info| info.online))
}

fn concurrent(tally: &mut Tally) -> Check {
    let tree = [space(TREE_SLOTS)?, space(TREE_SLOTS)?];
    ok(
        op(|store| store.create(node(tree, 0), ObjectKind::Endpoint)),
        "an endpoint could not be made",
    )?;
    let mut random = Random(0xc0c0_a5a5);
    for n in 1..NODES {
        let from = node(tree, random.below(u64::from(n)) as u32);
        ok(derive(from, node(tree, n), Derivation::COPY), "a tree could not be built")?;
    }
    TREE[0].store(pack(tree[0]), Ordering::Relaxed);
    TREE[1].store(pack(tree[1]), Ordering::Relaxed);
    let mut derivers = 0;
    for cpu in others() {
        OWN[cpu].store(pack(space(OWN_SLOTS)?), Ordering::Relaxed);
        derivers += 1;
    }
    let me = Arch::current_cpu();
    REVOKER.store(me, Ordering::Relaxed);
    DERIVERS.store(derivers, Ordering::Relaxed);
    READY.store(0, Ordering::Relaxed);
    STOP.store(false, Ordering::Release);
    work::on_all(me, concurrent_worker);
    let [made, revoking, missed, wrong] = OUTCOMES.each_ref().map(|n| n.load(Ordering::Relaxed));
    tally.concurrent =
        (made + revoking + missed, revoking, derivers, STEPS.load(Ordering::Relaxed));
    ensure(wrong == 0, "another CPU saw a derivation fail in a way it should not")?;
    ensure(tally.concurrent.3 > 0, "the tree could not be revoked")?;
    // Everything the other CPUs derived came from the tree, so the revoke
    // took it all.
    for cpu in others() {
        let own = unpack(OWN[cpu].load(Ordering::Relaxed));
        for slot in 0..OWN_SLOTS {
            ensure(
                get(at(own, slot)) == Err(CapError::Empty),
                "a capability derived during a revoke survived it",
            )?;
        }
        ok(caps::destroy_space(own), "a space could not be destroyed")?;
    }
    for n in 1..NODES {
        ensure(
            get(node(tree, n)) == Err(CapError::Empty),
            "a capability in the tree survived the revoke",
        )?;
    }
    for space in tree {
        ok(caps::destroy_space(space), "a space could not be destroyed")?;
    }
    audit()
}

/// The revoking CPU waits for the others to start, then revokes the
/// tree; the others derive from random nodes below the root into their
/// own spaces until it is done.
fn concurrent_worker() {
    let me = Arch::current_cpu();
    let tree = TREE.each_ref().map(|packed| unpack(packed.load(Ordering::Relaxed)));
    if me == REVOKER.load(Ordering::Relaxed) {
        let deadline = Arch::now_ns() + 2_000_000_000;
        while READY.load(Ordering::Acquire) < DERIVERS.load(Ordering::Relaxed)
            && Arch::now_ns() < deadline
        {
            core::hint::spin_loop();
        }
        let steps = caps::revoke(node(tree, 0)).unwrap_or(0);
        STEPS.store(u64::from(steps), Ordering::Relaxed);
        STOP.store(true, Ordering::Release);
        return;
    }
    let own = unpack(OWN[me].load(Ordering::Relaxed));
    let mut random = Random(0x1234_5678 + me as u64);
    READY.fetch_add(1, Ordering::Release);
    while !STOP.load(Ordering::Acquire) {
        let from = node(tree, 1 + random.below(u64::from(NODES - 1)) as u32);
        let to = at(own, random.below(u64::from(OWN_SLOTS)) as u32);
        let outcome = match derive(from, to, Derivation::COPY) {
            Ok(()) => 0,
            Err(CapError::Revoking) => 1,
            Err(CapError::Empty | CapError::Occupied) => 2,
            Err(_) => 3,
        };
        OUTCOMES[outcome].fetch_add(1, Ordering::Relaxed);
    }
}
