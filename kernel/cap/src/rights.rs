//! Rights: what a capability lets its holder do with its object.
//!
//! A capability's rights are a set of the twelve [`Rights`] below. Which of
//! them mean something depends on the object's kind
//! ([`crate::ObjectKind::rights`]); a capability never holds a right its
//! kind does not use. Derivation can only remove rights, never add them
//! (`docs/design/capabilities.md`).

use core::fmt;
use core::ops::{BitAnd, BitOr, Sub};

/// A set of rights.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Rights(u16);

impl Rights {
    /// No rights.
    pub const NONE: Rights = Rights(0);
    /// Read the object: map memory readable, read I/O ports, inspect a
    /// thread, process or address space.
    pub const READ: Rights = Rights(1 << 0);
    /// Write the object: map memory writable, write I/O ports.
    pub const WRITE: Rights = Rights(1 << 1);
    /// Map memory executable.
    pub const EXECUTE: Rights = Rights(1 << 2);
    /// Send on an endpoint, signal a notification.
    pub const SEND: Rights = Rights(1 << 3);
    /// Receive on an endpoint, wait on a notification or an interrupt.
    pub const RECEIVE: Rights = Rights(1 << 4);
    /// Pass capabilities in messages through an endpoint.
    pub const GRANT: Rights = Rights(1 << 5);
    /// Send a signal to a process (the plan's decision 6).
    pub const SIGNAL: Rights = Rights(1 << 6);
    /// Manage the object: retype memory, map into an address space, start
    /// and stop a thread or process, configure an interrupt line.
    pub const CONTROL: Rights = Rights(1 << 7);
    /// Seal capabilities with an endpoint as the key.
    pub const SEAL: Rights = Rights(1 << 8);
    /// Unseal capabilities sealed with an endpoint as the key.
    pub const UNSEAL: Rights = Rights(1 << 9);
    /// Make new capabilities from this one: copies, badged copies, copies
    /// with fewer rights or a smaller range, sealed copies, retyped
    /// objects.
    pub const COPY: Rights = Rights(1 << 10);
    /// Hand this capability, or one made from it, to another capability
    /// space.
    pub const TRANSFER: Rights = Rights(1 << 11);
    /// Every right.
    pub const ALL: Rights = Rights((1 << 12) - 1);

    /// Every right with its name, in bit order.
    pub const NAMES: [(Rights, &'static str); 12] = [
        (Rights::READ, "read"),
        (Rights::WRITE, "write"),
        (Rights::EXECUTE, "execute"),
        (Rights::SEND, "send"),
        (Rights::RECEIVE, "receive"),
        (Rights::GRANT, "grant"),
        (Rights::SIGNAL, "signal"),
        (Rights::CONTROL, "control"),
        (Rights::SEAL, "seal"),
        (Rights::UNSEAL, "unseal"),
        (Rights::COPY, "copy"),
        (Rights::TRANSFER, "transfer"),
    ];

    /// The set's bits.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// The set with these bits, or `None` if a bit names no right.
    pub const fn from_bits(bits: u16) -> Option<Rights> {
        if bits & !Rights::ALL.0 == 0 { Some(Rights(bits)) } else { None }
    }

    /// The set with these bits, ignoring bits that name no right.
    pub const fn from_bits_truncate(bits: u16) -> Rights {
        Rights(bits & Rights::ALL.0)
    }

    /// Whether every right in `other` is in this set.
    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The rights in either set.
    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    /// The rights in both sets.
    pub const fn intersection(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }

    /// The rights in this set and not in `other`.
    pub const fn difference(self, other: Rights) -> Rights {
        Rights(self.0 & !other.0)
    }

    /// How many rights the set holds.
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }
}

impl BitOr for Rights {
    type Output = Rights;

    fn bitor(self, other: Rights) -> Rights {
        self.union(other)
    }
}

impl BitAnd for Rights {
    type Output = Rights;

    fn bitand(self, other: Rights) -> Rights {
        self.intersection(other)
    }
}

impl Sub for Rights {
    type Output = Rights;

    fn sub(self, other: Rights) -> Rights {
        self.difference(other)
    }
}

/// Names separated by `+`, or `none`: `read+write+copy`.
impl fmt::Display for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let mut first = true;
        for (right, name) in Rights::NAMES {
            if self.contains(right) {
                if !first {
                    f.write_str("+")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rights({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cover_every_bit_once() {
        let mut seen = Rights::NONE;
        for (i, (right, _)) in Rights::NAMES.iter().enumerate() {
            assert_eq!(right.bits(), 1 << i);
            assert!(!seen.contains(*right));
            seen = seen | *right;
        }
        assert_eq!(seen, Rights::ALL);
    }

    #[test]
    fn set_operations() {
        let rw = Rights::READ | Rights::WRITE;
        assert!(rw.contains(Rights::READ));
        assert!(!rw.contains(Rights::READ | Rights::COPY));
        assert!(rw.contains(Rights::NONE));
        assert_eq!(rw & Rights::WRITE, Rights::WRITE);
        assert_eq!(rw - Rights::WRITE, Rights::READ);
        assert_eq!(rw.len(), 2);
        assert_eq!(Rights::from_bits(1 << 12), None);
        assert_eq!(Rights::from_bits_truncate(0xffff), Rights::ALL);
    }

    #[test]
    fn displays_names() {
        assert_eq!((Rights::READ | Rights::COPY).to_string(), "read+copy");
        assert_eq!(Rights::NONE.to_string(), "none");
    }
}
