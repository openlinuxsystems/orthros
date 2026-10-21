//! Capabilities and the checks that guard them.
//!
//! A [`Capability`] is a value: the object it designates, the rights it
//! grants, a badge and whether it is sealed. It has no public constructor.
//! The store makes the original capability for each object, and every
//! other capability comes from one of the methods here, each of which
//! refuses anything that would grant more than the capability it starts
//! from. These are the rights checks the Kani proofs cover
//! (`docs/design/capabilities.md`):
//!
//! - [`Capability::check`], which every use of a capability goes through;
//! - [`Capability::derive`], for copies, badged copies, and copies with
//!   fewer rights or a smaller range;
//! - [`Capability::seal_with`] and [`Capability::unseal_with`];
//! - [`Capability::retyped`] and [`carve`], for objects made from memory.

use crate::error::CapError;
use crate::object::{Object, ObjectId, ObjectKind, Target};
use crate::rights::Rights;

/// A capability: an object, the rights it grants on it, a badge, and the
/// key it is sealed with, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    pub(crate) object: Object,
    pub(crate) rights: Rights,
    pub(crate) badge: u64,
    pub(crate) sealed_by: Option<ObjectId>,
}

/// How [`Capability::derive`] makes a capability from another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Derivation {
    /// Rights to keep; the new capability has the old one's rights and
    /// these, so this can only take rights away.
    pub rights: Rights,
    /// A badge for a capability that has none yet.
    pub badge: Option<u64>,
    /// A part of the old capability's range, as (offset, length).
    pub range: Option<(u64, u64)>,
}

impl Derivation {
    /// A plain copy, with every right of the original.
    pub const COPY: Derivation = Derivation { rights: Rights::ALL, badge: None, range: None };

    /// A copy with only `rights` (of those the original has).
    pub const fn with_rights(rights: Rights) -> Derivation {
        Derivation { rights, badge: None, range: None }
    }

    /// A copy carrying `badge`.
    pub const fn with_badge(badge: u64) -> Derivation {
        Derivation { rights: Rights::ALL, badge: Some(badge), range: None }
    }

    /// A copy for `len` items of the range from `offset`.
    pub const fn with_range(offset: u64, len: u64) -> Derivation {
        Derivation { rights: Rights::ALL, badge: None, range: Some((offset, len)) }
    }
}

impl Default for Derivation {
    fn default() -> Derivation {
        Derivation::COPY
    }
}

impl Capability {
    /// The original capability for a new object: every right its kind has,
    /// no badge, not sealed.
    pub(crate) const fn original(object: Object) -> Capability {
        Capability { object, rights: object.kind.rights(), badge: 0, sealed_by: None }
    }

    /// The object it designates.
    pub const fn object(&self) -> Object {
        self.object
    }

    /// The kind of object it designates.
    pub const fn kind(&self) -> ObjectKind {
        self.object.kind
    }

    /// The rights it grants.
    pub const fn rights(&self) -> Rights {
        self.rights
    }

    /// Its badge; 0 means none.
    pub const fn badge(&self) -> u64 {
        self.badge
    }

    /// The endpoint it is sealed with, if it is sealed.
    pub const fn sealed_by(&self) -> Option<ObjectId> {
        self.sealed_by
    }

    /// The kernel object it designates, if it designates one.
    pub const fn id(&self) -> Option<ObjectId> {
        match self.object.target {
            Target::Object(id) => Some(id),
            Target::Range { .. } => None,
        }
    }

    /// The range it designates, as (base, length), if it designates one.
    pub const fn range(&self) -> Option<(u64, u64)> {
        match self.object.target {
            Target::Range { base, len } => Some((base, len)),
            Target::Object(_) => None,
        }
    }

    /// The check every use of a capability goes through: it must not be
    /// sealed, must designate a `kind`, and must hold every right in
    /// `needs`.
    pub const fn check(&self, kind: ObjectKind, needs: Rights) -> Result<(), CapError> {
        if self.sealed_by.is_some() {
            return Err(CapError::Sealed);
        }
        if self.object.kind as u8 != kind as u8 {
            return Err(CapError::WrongKind { expected: kind, found: self.object.kind });
        }
        if !self.rights.contains(needs) {
            return Err(CapError::MissingRights(needs.difference(self.rights)));
        }
        Ok(())
    }

    /// A capability made from this one as `how` says, or why that is
    /// refused. It needs the `copy` right. The new capability designates
    /// the same object (or part of the same range) with at most this one's
    /// rights, and this one's badge unless this one has none. A sealed
    /// capability can only be copied as it is.
    pub const fn derive(&self, how: Derivation) -> Result<Capability, CapError> {
        if !self.rights.contains(Rights::COPY) {
            return Err(CapError::MissingRights(Rights::COPY));
        }
        if self.sealed_by.is_some()
            && (!how.rights.contains(self.rights) || how.badge.is_some() || how.range.is_some())
        {
            return Err(CapError::Sealed);
        }
        let mut derived = *self;
        derived.rights = self.rights.intersection(how.rights);
        if let Some(badge) = how.badge {
            if !self.object.kind.badged() {
                return Err(CapError::NotBadgeable(self.object.kind));
            }
            if badge == 0 {
                return Err(CapError::InvalidBadge);
            }
            if self.badge != 0 {
                return Err(CapError::AlreadyBadged);
            }
            derived.badge = badge;
        }
        if let Some((offset, len)) = how.range {
            let Target::Range { base, len: own } = self.object.target else {
                return Err(CapError::NotARange);
            };
            let Some(end) = offset.checked_add(len) else {
                return Err(CapError::OutOfRange);
            };
            if len == 0 || end > own {
                return Err(CapError::OutOfRange);
            }
            // `base + offset` cannot overflow: `base + own` did not.
            derived.object.target = Target::Range { base: base + offset, len };
        }
        Ok(derived)
    }

    /// A sealed copy of this capability, with `key` (an endpoint with the
    /// `seal` right) as its key. It needs the `copy` right. A sealed
    /// capability cannot be used, changed or sealed again; it can only be
    /// copied as it is, transferred, deleted, or unsealed with the key.
    pub const fn seal_with(&self, key: &Capability) -> Result<Capability, CapError> {
        if let Err(err) = key.check(ObjectKind::Endpoint, Rights::SEAL) {
            return Err(err);
        }
        let Some(key) = key.id() else {
            return Err(CapError::WrongKey);
        };
        if !self.rights.contains(Rights::COPY) {
            return Err(CapError::MissingRights(Rights::COPY));
        }
        if self.sealed_by.is_some() {
            return Err(CapError::Sealed);
        }
        let mut sealed = *self;
        sealed.sealed_by = Some(key);
        Ok(sealed)
    }

    /// An unsealed copy of this sealed capability, if `key` is the
    /// endpoint it was sealed with and has the `unseal` right.
    pub const fn unseal_with(&self, key: &Capability) -> Result<Capability, CapError> {
        if let Err(err) = key.check(ObjectKind::Endpoint, Rights::UNSEAL) {
            return Err(err);
        }
        let Some(sealed_by) = self.sealed_by else {
            return Err(CapError::NotSealed);
        };
        let Some(key) = key.id() else {
            return Err(CapError::WrongKey);
        };
        if sealed_by.index != key.index || sealed_by.generation != key.generation {
            return Err(CapError::WrongKey);
        }
        let mut unsealed = *self;
        unsealed.sealed_by = None;
        Ok(unsealed)
    }

    /// The capability for object `id` of `kind`, made from the memory this
    /// capability designates. It needs the `control` and `copy` rights,
    /// and keeps those of this capability's rights that `kind` has.
    pub const fn retyped(&self, kind: ObjectKind, id: ObjectId) -> Result<Capability, CapError> {
        if let Err(err) = self.check(ObjectKind::Memory, Rights::CONTROL.union(Rights::COPY)) {
            return Err(err);
        }
        if !kind.from_memory() {
            return Err(CapError::InvalidArgument);
        }
        Ok(Capability {
            object: Object { kind, target: Target::Object(id) },
            rights: self.rights.intersection(kind.rights()),
            badge: 0,
            sealed_by: None,
        })
    }

    /// Whether this capability grants no more than `parent`: its rights
    /// are among the parent's, it designates the parent's object or part
    /// of its range, and it carries the parent's badge or the parent has
    /// none. Every capability [`Capability::derive`], [`Capability::seal_with`]
    /// and [`Capability::unseal_with`] make is related so to the one they
    /// start from, and the relation is transitive, so no chain of them
    /// grants more than where it began.
    pub const fn within(&self, parent: &Capability) -> bool {
        parent.rights.contains(self.rights)
            && self.object.within(&parent.object)
            && (self.badge == parent.badge || parent.badge == 0)
    }
}

/// Where in a memory object `count` blocks of 2^`order` pages go, as
/// (first block's offset, new watermark), both in pages from the object's
/// start: at the first offset past `watermark` where the blocks are
/// aligned to their size in physical memory, if they fit in `pages`.
/// Retyping takes memory only from above the watermark, which only grows
/// until every object made from the memory is gone, so objects made from
/// one memory object never overlap.
pub const fn carve(
    base: u64,
    pages: u64,
    watermark: u64,
    order: u32,
    count: u64,
) -> Result<(u64, u64), CapError> {
    if order >= 63 || count == 0 || watermark > pages {
        return Err(CapError::InvalidArgument);
    }
    let size = 1u64 << order;
    let Some(start) = base.checked_add(watermark) else {
        return Err(CapError::InvalidArgument);
    };
    let Some(aligned) = start.checked_next_multiple_of(size) else {
        return Err(CapError::Exhausted);
    };
    let first = aligned - base;
    let Some(total) = size.checked_mul(count) else {
        return Err(CapError::Exhausted);
    };
    let Some(end) = first.checked_add(total) else {
        return Err(CapError::Exhausted);
    };
    if end > pages {
        return Err(CapError::Exhausted);
    }
    Ok((first, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(index: u32) -> ObjectId {
        ObjectId { index, generation: 1 }
    }

    fn of(kind: ObjectKind, index: u32) -> Capability {
        Capability::original(Object { kind, target: Target::Object(id(index)) })
    }

    fn ports(base: u64, len: u64) -> Capability {
        Capability::original(Object {
            kind: ObjectKind::IoPorts,
            target: Target::Range { base, len },
        })
    }

    #[test]
    fn originals_hold_their_kinds_rights() {
        let frame = of(ObjectKind::Frame, 1);
        assert_eq!(frame.rights(), ObjectKind::Frame.rights());
        assert_eq!(frame.check(ObjectKind::Frame, Rights::READ | Rights::WRITE), Ok(()));
        assert_eq!(
            frame.check(ObjectKind::Frame, Rights::CONTROL),
            Err(CapError::MissingRights(Rights::CONTROL))
        );
        assert_eq!(
            frame.check(ObjectKind::Endpoint, Rights::NONE),
            Err(CapError::WrongKind { expected: ObjectKind::Endpoint, found: ObjectKind::Frame })
        );
    }

    #[test]
    fn derivation_only_takes_away() {
        let frame = of(ObjectKind::Frame, 1);
        let read = frame.derive(Derivation::with_rights(Rights::READ | Rights::COPY)).unwrap();
        assert_eq!(read.rights(), Rights::READ | Rights::COPY);
        let again = read.derive(Derivation::COPY).unwrap();
        assert_eq!(again.rights(), Rights::READ | Rights::COPY);
        assert!(again.within(&read) && read.within(&frame));
        let no_copy = read.derive(Derivation::with_rights(Rights::READ)).unwrap();
        assert_eq!(no_copy.derive(Derivation::COPY), Err(CapError::MissingRights(Rights::COPY)));
    }

    #[test]
    fn badges_are_set_once() {
        let endpoint = of(ObjectKind::Endpoint, 2);
        let badged = endpoint.derive(Derivation::with_badge(7)).unwrap();
        assert_eq!(badged.badge(), 7);
        assert_eq!(badged.derive(Derivation::with_badge(8)), Err(CapError::AlreadyBadged));
        assert_eq!(badged.derive(Derivation::COPY).unwrap().badge(), 7);
        assert_eq!(endpoint.derive(Derivation::with_badge(0)), Err(CapError::InvalidBadge));
        assert_eq!(
            of(ObjectKind::Frame, 1).derive(Derivation::with_badge(7)),
            Err(CapError::NotBadgeable(ObjectKind::Frame))
        );
    }

    #[test]
    fn ranges_only_shrink() {
        let all = ports(0, 0x100);
        let keyboard = all.derive(Derivation::with_range(0x60, 5)).unwrap();
        assert_eq!(keyboard.range(), Some((0x60, 5)));
        assert_eq!(keyboard.derive(Derivation::with_range(4, 2)), Err(CapError::OutOfRange));
        assert_eq!(keyboard.derive(Derivation::with_range(1, 0)), Err(CapError::OutOfRange));
        assert_eq!(keyboard.derive(Derivation::with_range(u64::MAX, 2)), Err(CapError::OutOfRange));
        assert_eq!(keyboard.derive(Derivation::with_range(4, 1)).unwrap().range(), Some((0x64, 1)));
        assert_eq!(
            of(ObjectKind::Frame, 1).derive(Derivation::with_range(0, 1)),
            Err(CapError::NotARange)
        );
    }

    #[test]
    fn sealed_capabilities_need_their_key() {
        let frame = of(ObjectKind::Frame, 1);
        let key = of(ObjectKind::Endpoint, 2);
        let other = of(ObjectKind::Endpoint, 3);
        let sealed = frame.seal_with(&key).unwrap();
        assert_eq!(sealed.check(ObjectKind::Frame, Rights::NONE), Err(CapError::Sealed));
        assert_eq!(sealed.seal_with(&key), Err(CapError::Sealed));
        assert_eq!(sealed.derive(Derivation::with_rights(Rights::READ)), Err(CapError::Sealed));
        assert_eq!(sealed.derive(Derivation::COPY), Ok(sealed));
        assert_eq!(sealed.unseal_with(&other), Err(CapError::WrongKey));
        assert_eq!(frame.unseal_with(&key), Err(CapError::NotSealed));
        let seal_only = key.derive(Derivation::with_rights(Rights::SEAL | Rights::COPY)).unwrap();
        assert_eq!(sealed.unseal_with(&seal_only), Err(CapError::MissingRights(Rights::UNSEAL)));
        assert_eq!(
            frame.seal_with(&frame),
            Err(CapError::WrongKind { expected: ObjectKind::Endpoint, found: ObjectKind::Frame })
        );
        assert_eq!(sealed.unseal_with(&key), Ok(frame));
    }

    #[test]
    fn retyping_needs_control() {
        let memory = of(ObjectKind::Memory, 1);
        let frame = memory.retyped(ObjectKind::Frame, id(9)).unwrap();
        assert_eq!(frame.rights(), ObjectKind::Frame.rights());
        assert_eq!(memory.retyped(ObjectKind::Endpoint, id(9)), Err(CapError::InvalidArgument));
        let no_control =
            memory.derive(Derivation::with_rights(Rights::ALL - Rights::CONTROL)).unwrap();
        assert_eq!(
            no_control.retyped(ObjectKind::Frame, id(9)),
            Err(CapError::MissingRights(Rights::CONTROL))
        );
        let read_only = memory
            .derive(Derivation::with_rights(Rights::READ | Rights::CONTROL | Rights::COPY))
            .unwrap();
        assert_eq!(
            read_only.retyped(ObjectKind::Frame, id(9)).unwrap().rights(),
            Rights::READ | Rights::COPY
        );
    }

    #[test]
    fn carving_is_aligned_and_never_overlaps() {
        // 16 pages at frame 0x100: two order-2 blocks after one page.
        assert_eq!(carve(0x100, 16, 1, 2, 2), Ok((4, 12)));
        assert_eq!(carve(0x100, 16, 12, 2, 2), Err(CapError::Exhausted));
        assert_eq!(carve(0x101, 16, 0, 1, 1), Ok((1, 3)));
        assert_eq!(carve(0, 16, 0, 2, 0), Err(CapError::InvalidArgument));
        assert_eq!(carve(0, 16, 17, 0, 1), Err(CapError::InvalidArgument));
        assert_eq!(carve(0, u64::MAX, 0, 62, 4), Err(CapError::Exhausted));
        assert_eq!(carve(u64::MAX, 2, 1, 1, 1), Err(CapError::InvalidArgument));
    }
}
