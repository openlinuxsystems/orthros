// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Kani proof harnesses for the rights checks.
//!
//! Kani checks each harness for every value its `kani::any()` inputs can
//! take, so these are proofs over all capabilities, rights, badges, ranges
//! and keys, not samples. `ob test kani` runs them;
//! `docs/design/capabilities.md` lists what each establishes.

use crate::cap::{Capability, Derivation, carve};
use crate::error::CapError;
use crate::object::{KINDS, Object, ObjectId, ObjectKind, Target};
use crate::rights::Rights;

fn any_kind() -> ObjectKind {
    let index: usize = kani::any_where(|&n| n < KINDS);
    ObjectKind::ALL[index]
}

fn any_id() -> ObjectId {
    ObjectId { index: kani::any(), generation: kani::any() }
}

/// Any set of rights, including bits that name no right.
fn any_mask() -> Rights {
    Rights::from_bits_truncate(kani::any())
}

/// Any capability the store can hold: rights among its kind's, a badge
/// only where its kind carries one, a range inside its kind's limit or a
/// kernel object, sealed or not.
fn any_cap() -> Capability {
    let kind = any_kind();
    let target = match kind.range_limit() {
        Some(limit) => {
            let (base, len): (u64, u64) = (kani::any(), kani::any());
            kani::assume(len >= 1 && base.checked_add(len).is_some_and(|end| end <= limit));
            Target::Range { base, len }
        }
        None => Target::Object(any_id()),
    };
    let rights = any_mask();
    kani::assume(kind.rights().contains(rights));
    let badge = if kind.badged() { kani::any() } else { 0 };
    let sealed_by = if kani::any() { Some(any_id()) } else { None };
    Capability { object: Object { kind, target }, rights, badge, sealed_by }
}

fn any_derivation() -> Derivation {
    Derivation {
        rights: any_mask(),
        badge: if kani::any() { Some(kani::any()) } else { None },
        range: if kani::any() { Some((kani::any(), kani::any())) } else { None },
    }
}

/// The capability a store can hold: what `any_cap` assumes.
fn valid(cap: &Capability) -> bool {
    let kind = cap.kind();
    kind.rights().contains(cap.rights)
        && (cap.badge == 0 || kind.badged())
        && match cap.object.target {
            Target::Range { base, len } => kind
                .range_limit()
                .is_some_and(|limit| len >= 1 && base.checked_add(len).is_some_and(|e| e <= limit)),
            Target::Object(_) => !kind.is_range(),
        }
}

/// A capability may be used exactly when it is unsealed, designates the
/// kind asked for, and holds every right asked for.
#[kani::proof]
fn check_is_exact() {
    let cap = any_cap();
    let kind = any_kind();
    let needs = any_mask();
    let allowed = cap.check(kind, needs).is_ok();
    assert_eq!(
        allowed,
        cap.sealed_by.is_none() && cap.kind() == kind && cap.rights.contains(needs)
    );
}

/// A derived capability grants no more than its source: its rights are
/// the source's and the mask's, it designates the same object or part of
/// the same range, it keeps the source's badge (or gets one only if the
/// source had none), its seal is the source's, and it is a valid
/// capability. A derivation is refused only for a stated reason.
#[kani::proof]
fn derivation_never_widens() {
    let cap = any_cap();
    let how = any_derivation();
    match cap.derive(how) {
        Ok(derived) => {
            assert!(derived.within(&cap));
            assert!(valid(&derived));
            assert_eq!(derived.rights, cap.rights & how.rights);
            assert_eq!(derived.kind(), cap.kind());
            assert_eq!(derived.sealed_by, cap.sealed_by);
            assert!(cap.rights.contains(Rights::COPY));
            match how.badge {
                None => {
                    assert_eq!(derived.badge, cap.badge);
                }
                Some(badge) => {
                    assert!(cap.badge == 0 && badge != 0 && cap.kind().badged());
                    assert_eq!(derived.badge, badge);
                }
            }
            if how.range.is_none() {
                assert_eq!(derived.object, cap.object);
            }
            kani::cover!(how.badge.is_some(), "a badged copy is made");
            kani::cover!(how.range.is_some(), "a copy of part of a range is made");
            kani::cover!(cap.sealed_by.is_some(), "a sealed capability is copied");
        }
        Err(CapError::MissingRights(missing)) => {
            assert_eq!(missing, Rights::COPY);
            assert!(!cap.rights.contains(Rights::COPY));
        }
        Err(CapError::Sealed) => {
            assert!(cap.sealed_by.is_some());
        }
        Err(CapError::NotBadgeable(kind)) => {
            assert!(!kind.badged() && how.badge.is_some());
        }
        Err(CapError::InvalidBadge) => {
            assert_eq!(how.badge, Some(0));
        }
        Err(CapError::AlreadyBadged) => {
            assert!(cap.badge != 0 && how.badge.is_some());
        }
        Err(CapError::NotARange) => {
            assert!(how.range.is_some() && cap.range().is_none());
        }
        Err(CapError::OutOfRange) => {
            let (offset, len) = how.range.unwrap();
            let (_, own) = cap.range().unwrap();
            assert!(len == 0 || offset.checked_add(len).is_none_or(|end| end > own));
        }
        Err(_) => panic!("derive refused for a reason it does not give"),
    }
}

/// A sealed capability can only be copied exactly, and cannot be used,
/// sealed again or retyped.
#[kani::proof]
fn sealed_capabilities_stay_sealed() {
    let cap = any_cap();
    kani::assume(cap.sealed_by.is_some());
    if let Ok(derived) = cap.derive(any_derivation()) {
        assert_eq!(derived, cap);
    }
    let key = any_cap();
    assert!(cap.seal_with(&key).is_err());
    assert_eq!(cap.check(any_kind(), any_mask()), Err(CapError::Sealed));
    assert!(cap.retyped(any_kind(), any_id()).is_err());
}

/// Sealing needs an unsealed endpoint with the `seal` right as the key and
/// the `copy` right on the capability, and changes nothing but the seal.
#[kani::proof]
fn sealing_changes_only_the_seal() {
    let cap = any_cap();
    let key = any_cap();
    let allowed = key.check(ObjectKind::Endpoint, Rights::SEAL).is_ok()
        && cap.rights.contains(Rights::COPY)
        && cap.sealed_by.is_none();
    match cap.seal_with(&key) {
        Ok(sealed) => {
            assert!(allowed);
            assert_eq!(sealed.sealed_by, key.id());
            assert!(sealed.sealed_by.is_some());
            assert!(sealed.within(&cap) && cap.within(&sealed));
            assert_eq!(Capability { sealed_by: None, ..sealed }, cap);
        }
        Err(_) => {
            assert!(!allowed);
        }
    }
}

/// Only the key a capability was sealed with, as an unsealed endpoint
/// capability with the `unseal` right, unseals it, and unsealing gives
/// back exactly the capability that was sealed.
#[kani::proof]
fn unsealing_needs_the_key() {
    let cap = any_cap();
    let key = any_cap();
    let allowed = key.check(ObjectKind::Endpoint, Rights::UNSEAL).is_ok()
        && cap.sealed_by.is_some()
        && cap.sealed_by == key.id();
    match cap.unseal_with(&key) {
        Ok(unsealed) => {
            assert!(allowed);
            assert_eq!(Capability { sealed_by: cap.sealed_by, ..unsealed }, cap);
            assert!(unsealed.sealed_by.is_none());
            kani::cover!(true, "a capability is unsealed");
        }
        Err(_) => {
            assert!(!allowed);
        }
    }
}

/// `within`, the relation between every capability and the one it was
/// made from, is transitive: no chain of derivations grants more than
/// the capability it started from.
#[kani::proof]
fn within_is_transitive() {
    let (a, b, c) = (any_cap(), any_cap(), any_cap());
    if a.within(&b) && b.within(&c) {
        assert!(a.within(&c));
    }
}

/// An object retyped from memory gets no right the memory capability
/// lacks or its kind lacks, and retyping needs an unsealed memory
/// capability with `control` and `copy`.
#[kani::proof]
fn retyping_never_widens() {
    let cap = any_cap();
    let kind = any_kind();
    if let Ok(made) = cap.retyped(kind, any_id()) {
        assert_eq!(cap.kind(), ObjectKind::Memory);
        assert!(cap.sealed_by.is_none());
        assert!(cap.rights.contains(Rights::CONTROL | Rights::COPY));
        assert!(kind.from_memory());
        assert!(cap.rights.contains(made.rights) && kind.rights().contains(made.rights));
        assert_eq!((made.badge, made.sealed_by, made.kind()), (0, None, kind));
        assert!(valid(&made));
    }
}

/// Retyping takes blocks from above the watermark only, aligned to their
/// size in physical memory, and inside the memory object: so objects
/// made from one memory object never overlap.
#[kani::proof]
fn carving_stays_above_the_watermark() {
    let (base, pages, watermark): (u64, u64, u64) = (kani::any(), kani::any(), kani::any());
    let order: u32 = kani::any_where(|&o| o <= 12);
    let count: u64 = kani::any_where(|&c| c <= 1024);
    if let Ok((first, end)) = carve(base, pages, watermark, order, count) {
        kani::cover!(first > watermark, "blocks are aligned past the watermark");
        assert!(watermark <= first && first <= end && end <= pages);
        assert_eq!((base + first) % (1 << order), 0);
        assert_eq!(end - first, count << order);
        assert!(count >= 1);
    }
}
