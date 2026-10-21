// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a capability designates: kernel objects and hardware resources.
//!
//! Nine kinds are kernel objects with an entry in the store's object
//! table, named by an [`ObjectId`]: memory, frames, DMA buffers, address
//! spaces, threads, processes, endpoints, notifications and resource
//! domains (Phase 4: a CPU weight and a memory limit that threads and
//! their memory are charged to, after Linux's cgroups). Three kinds are
//! ranges of hardware resources, named by the range itself: interrupt
//! lines, I/O ports and device registers (MMIO). The kernel creates the
//! first capability to each; every other capability is derived from one.

use core::fmt;

use crate::rights::Rights;

/// The kind of object a capability designates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ObjectKind {
    /// Physical memory not yet given a use, from which frames, DMA buffers
    /// and smaller memory objects are made (retyped).
    Memory = 0,
    /// A block of 2^order pages of RAM that can be mapped.
    Frame = 1,
    /// A virtual address space (page tables).
    AddressSpace = 2,
    /// A thread.
    Thread = 3,
    /// A process: an address space, a capability space and its threads.
    Process = 4,
    /// An IPC endpoint.
    Endpoint = 5,
    /// An asynchronous notification word.
    Notification = 6,
    /// A range of interrupt lines.
    Irq = 7,
    /// A range of x86 I/O ports.
    IoPorts = 8,
    /// A range of device registers, in pages of physical address space.
    Mmio = 9,
    /// Memory a device reads or writes directly.
    Dma = 10,
    /// A resource domain: the CPU weight and memory limit its threads
    /// share, and its class for the out-of-memory policy.
    ResourceDomain = 11,
}

/// How many kinds there are.
pub const KINDS: usize = 12;

impl ObjectKind {
    /// Every kind, in the order of their numbers.
    pub const ALL: [ObjectKind; KINDS] = [
        ObjectKind::Memory,
        ObjectKind::Frame,
        ObjectKind::AddressSpace,
        ObjectKind::Thread,
        ObjectKind::Process,
        ObjectKind::Endpoint,
        ObjectKind::Notification,
        ObjectKind::Irq,
        ObjectKind::IoPorts,
        ObjectKind::Mmio,
        ObjectKind::Dma,
        ObjectKind::ResourceDomain,
    ];

    /// The kind numbered `n`.
    pub const fn from_index(n: usize) -> Option<ObjectKind> {
        if n < KINDS { Some(ObjectKind::ALL[n]) } else { None }
    }

    /// The kind's number.
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The rights that mean something for this kind. The capability the
    /// kernel creates for a new object holds all of them; no capability
    /// holds any other.
    pub const fn rights(self) -> Rights {
        let own = match self {
            ObjectKind::Memory => {
                Rights::READ.union(Rights::WRITE).union(Rights::EXECUTE).union(Rights::CONTROL)
            }
            ObjectKind::Frame => Rights::READ.union(Rights::WRITE).union(Rights::EXECUTE),
            ObjectKind::AddressSpace | ObjectKind::Thread | ObjectKind::ResourceDomain => {
                Rights::READ.union(Rights::CONTROL)
            }
            ObjectKind::Process => Rights::READ.union(Rights::CONTROL).union(Rights::SIGNAL),
            ObjectKind::Endpoint => Rights::SEND
                .union(Rights::RECEIVE)
                .union(Rights::GRANT)
                .union(Rights::SEAL)
                .union(Rights::UNSEAL),
            ObjectKind::Notification => Rights::SEND.union(Rights::RECEIVE),
            ObjectKind::Irq => Rights::RECEIVE.union(Rights::CONTROL),
            ObjectKind::IoPorts | ObjectKind::Mmio | ObjectKind::Dma => {
                Rights::READ.union(Rights::WRITE)
            }
        };
        own.union(Rights::COPY).union(Rights::TRANSFER)
    }

    /// Whether capabilities to this kind can carry a badge: endpoints, whose
    /// receiver sees the sender's badge, and notifications, which add the
    /// signaller's badge to their word.
    pub const fn badged(self) -> bool {
        matches!(self, ObjectKind::Endpoint | ObjectKind::Notification)
    }

    /// Whether this kind is a range of hardware resources rather than a
    /// kernel object.
    pub const fn is_range(self) -> bool {
        matches!(self, ObjectKind::Irq | ObjectKind::IoPorts | ObjectKind::Mmio)
    }

    /// Whether memory can be retyped into this kind.
    pub const fn from_memory(self) -> bool {
        matches!(self, ObjectKind::Memory | ObjectKind::Frame | ObjectKind::Dma)
    }

    /// The largest range of this kind, if it is a range kind: 256
    /// interrupt lines, 65,536 I/O ports, and 2^40 pages (the 52-bit
    /// physical address space).
    pub const fn range_limit(self) -> Option<u64> {
        match self {
            ObjectKind::Irq => Some(256),
            ObjectKind::IoPorts => Some(1 << 16),
            ObjectKind::Mmio => Some(1 << 40),
            _ => None,
        }
    }

    /// Lower-case name for reports.
    pub const fn name(self) -> &'static str {
        match self {
            ObjectKind::Memory => "memory",
            ObjectKind::Frame => "frame",
            ObjectKind::AddressSpace => "address space",
            ObjectKind::Thread => "thread",
            ObjectKind::Process => "process",
            ObjectKind::Endpoint => "endpoint",
            ObjectKind::Notification => "notification",
            ObjectKind::Irq => "IRQ",
            ObjectKind::IoPorts => "I/O ports",
            ObjectKind::Mmio => "MMIO",
            ObjectKind::Dma => "DMA",
            ObjectKind::ResourceDomain => "domain",
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The name of a kernel object: its entry in the object table and that
/// entry's generation, so a name stays wrong once the object is gone and
/// the entry is reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObjectId {
    /// Index in the object table.
    pub index: u32,
    /// Generation of the entry when the object was made.
    pub generation: u32,
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}.{}", self.index, self.generation)
    }
}

/// What a capability designates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// A kernel object.
    Object(ObjectId),
    /// `len` resources from `base`: interrupt lines, I/O ports or pages of
    /// device registers.
    Range {
        /// First line, port or page frame number.
        base: u64,
        /// How many, at least one.
        len: u64,
    },
}

/// An object kind and what of that kind a capability designates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Object {
    /// The kind.
    pub kind: ObjectKind,
    /// The kernel object or range.
    pub target: Target,
}

impl Object {
    /// Whether `self` designates `other` or part of it: the same kernel
    /// object, or a range of the same kind inside the other's.
    pub const fn within(&self, other: &Object) -> bool {
        if self.kind as u8 != other.kind as u8 {
            return false;
        }
        match (self.target, other.target) {
            (Target::Object(a), Target::Object(b)) => {
                a.index == b.index && a.generation == b.generation
            }
            (Target::Range { base, len }, Target::Range { base: outer, len: outer_len }) => {
                match (base.checked_add(len), outer.checked_add(outer_len)) {
                    (Some(end), Some(outer_end)) => base >= outer && end <= outer_end,
                    _ => false,
                }
            }
            _ => false,
        }
    }
}

impl fmt::Display for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.target {
            Target::Object(id) => write!(f, "{} {id}", self.kind),
            Target::Range { base, len } => match self.kind {
                ObjectKind::IoPorts => write!(f, "I/O ports {base:#06x}-{:#06x}", base + len - 1),
                ObjectKind::Mmio => {
                    write!(f, "MMIO {:#x}-{:#x}", base << 12, ((base + len) << 12) - 1)
                }
                _ => write!(f, "{} {base}-{}", self.kind, base + len - 1),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_numbered_in_order() {
        for (i, kind) in ObjectKind::ALL.iter().enumerate() {
            assert_eq!(kind.index(), i);
            assert_eq!(ObjectKind::from_index(i), Some(*kind));
        }
        assert_eq!(ObjectKind::from_index(KINDS), None);
    }

    #[test]
    fn every_kind_can_be_copied_and_transferred() {
        for kind in ObjectKind::ALL {
            assert!(kind.rights().contains(Rights::COPY | Rights::TRANSFER), "{kind}");
            assert_eq!(kind.is_range(), kind.range_limit().is_some());
        }
        assert!(!ObjectKind::Frame.rights().contains(Rights::CONTROL));
        assert!(ObjectKind::Process.rights().contains(Rights::SIGNAL));
    }

    #[test]
    fn ranges_nest() {
        let ports =
            |base, len| Object { kind: ObjectKind::IoPorts, target: Target::Range { base, len } };
        assert!(ports(0x60, 5).within(&ports(0, 0x100)));
        assert!(ports(0, 0x100).within(&ports(0, 0x100)));
        assert!(!ports(0xff, 2).within(&ports(0, 0x100)));
        assert!(!ports(u64::MAX, 2).within(&ports(0, u64::MAX)));
        let irq = Object { kind: ObjectKind::Irq, target: Target::Range { base: 0x60, len: 5 } };
        assert!(!irq.within(&ports(0, 0x100)));
        assert_eq!(ports(0x60, 5).to_string(), "I/O ports 0x0060-0x0064");
    }
}
