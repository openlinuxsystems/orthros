//! Why a capability operation was refused.

use core::fmt;

use crate::object::ObjectKind;
use crate::rights::Rights;

/// Why a capability operation was refused. A refused operation changes
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapError {
    /// The capability space does not exist (any more).
    NoSuchSpace,
    /// The slot number is beyond the end of the capability space.
    NoSuchSlot,
    /// The slot holds no capability.
    Empty,
    /// The destination slot already holds a capability.
    Occupied,
    /// The capability designates another kind of object.
    WrongKind {
        /// The kind the operation needs.
        expected: ObjectKind,
        /// The kind the capability designates.
        found: ObjectKind,
    },
    /// The capability lacks rights the operation needs.
    MissingRights(Rights),
    /// The capability is sealed: it cannot be used or changed until the
    /// holder of its key unseals it.
    Sealed,
    /// Only a sealed capability can be unsealed.
    NotSealed,
    /// The key did not seal this capability.
    WrongKey,
    /// Capabilities to this kind of object carry no badge.
    NotBadgeable(ObjectKind),
    /// The capability already carries a badge, which never changes.
    AlreadyBadged,
    /// Badge 0 means "no badge" and cannot be given.
    InvalidBadge,
    /// Only interrupt lines, I/O ports and device registers have ranges.
    NotARange,
    /// The range is empty or reaches outside the capability's own.
    OutOfRange,
    /// The derivation would be deeper than [`crate::MAX_DEPTH`].
    TooDeep,
    /// The capability, or one it was derived from, is being revoked.
    Revoking,
    /// The memory object has no room left for the objects asked for.
    Exhausted,
    /// The kernel heap could not provide the memory the operation needs.
    NoMemory,
    /// An argument is outside what the operation accepts.
    InvalidArgument,
}

impl fmt::Display for CapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapError::NoSuchSpace => f.write_str("no such capability space"),
            CapError::NoSuchSlot => f.write_str("no such slot"),
            CapError::Empty => f.write_str("the slot is empty"),
            CapError::Occupied => f.write_str("the destination slot is in use"),
            CapError::WrongKind { expected, found } => {
                write!(f, "the capability designates a {found}, not a {expected}")
            }
            CapError::MissingRights(rights) => write!(f, "the capability lacks {rights}"),
            CapError::Sealed => f.write_str("the capability is sealed"),
            CapError::NotSealed => f.write_str("the capability is not sealed"),
            CapError::WrongKey => f.write_str("the key did not seal this capability"),
            CapError::NotBadgeable(kind) => write!(f, "{kind} capabilities carry no badge"),
            CapError::AlreadyBadged => f.write_str("the capability already carries a badge"),
            CapError::InvalidBadge => f.write_str("badge 0 means no badge"),
            CapError::NotARange => f.write_str("the capability designates no range"),
            CapError::OutOfRange => f.write_str("the range is outside the capability's own"),
            CapError::TooDeep => f.write_str("the derivation would be too deep"),
            CapError::Revoking => f.write_str("the capability is being revoked"),
            CapError::Exhausted => f.write_str("the memory object has no room left"),
            CapError::NoMemory => f.write_str("out of kernel memory"),
            CapError::InvalidArgument => f.write_str("invalid argument"),
        }
    }
}
