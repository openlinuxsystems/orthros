//! Allocation context: what every allocation in the kernel must say about
//! the caller.
//!
//! Spec section 3.4 asks kernel allocators to be explicit about context, the
//! way Linux's GFP flags are. Every call into the frame allocator, the heap
//! or the page-table code takes [`Flags`]:
//!
//! - [`Context`]: whether the caller may sleep. `Atomic` callers (interrupt
//!   handlers, code holding a spin lock) never wait, never make other CPUs
//!   give back cached memory, and may dig into the small reserve each zone
//!   keeps for them. `MaySleep` callers leave the reserve alone, and before
//!   failing the allocator asks every CPU to return its cached objects and
//!   tries again. Once the scheduler exists (Phase 4) they may also wait for
//!   reclaim.
//! - [`ZoneLimit`]: `Dma32` for memory a 32-bit device must reach.
//! - [`NodePolicy`]: which NUMA node to take memory from: the calling CPU's
//!   (the default), a preferred one with fallback to the nearest others, or
//!   exactly one.
//! - whether to zero the memory.
//!
//! Failure is always an error value, never a panic and never a silent kill:
//! the caller decides what to do without the memory.

#![forbid(unsafe_code)]

use core::fmt;

/// Whether the caller may sleep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    /// The caller cannot sleep: it runs in an interrupt handler or holds a
    /// spin lock. The allocator never waits for it, and it may use the
    /// zones' reserves.
    Atomic,
    /// The caller may sleep. The allocator keeps the reserves for atomic
    /// callers and drains per-CPU caches before giving up.
    MaySleep,
}

/// Which physical memory is acceptable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneLimit {
    /// Any RAM.
    Any,
    /// RAM below 4 GiB, for devices that address memory with 32 bits.
    Dma32,
}

/// Which NUMA node to take memory from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodePolicy {
    /// The node of the CPU making the call, then the nearest others.
    Local,
    /// This node, then the nearest others.
    Prefer(u8),
    /// This node only.
    Only(u8),
}

/// Everything an allocation says about its caller and its memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flags {
    /// Whether the caller may sleep.
    pub context: Context,
    /// Which memory is acceptable.
    pub zone: ZoneLimit,
    /// Which node to take it from.
    pub node: NodePolicy,
    /// Fill the memory with zeros.
    pub zero: bool,
}

impl Flags {
    /// For callers that cannot sleep: any memory, the local node first.
    pub const ATOMIC: Flags = Flags {
        context: Context::Atomic,
        zone: ZoneLimit::Any,
        node: NodePolicy::Local,
        zero: false,
    };

    /// For callers that may sleep: any memory, the local node first.
    pub const MAY_SLEEP: Flags = Flags {
        context: Context::MaySleep,
        zone: ZoneLimit::Any,
        node: NodePolicy::Local,
        zero: false,
    };

    /// The same flags, asking for zeroed memory.
    #[must_use]
    pub const fn zeroed(self) -> Flags {
        Flags { zero: true, ..self }
    }

    /// The same flags, limited to memory below 4 GiB.
    #[must_use]
    pub const fn dma32(self) -> Flags {
        Flags { zone: ZoneLimit::Dma32, ..self }
    }

    /// The same flags, preferring `node`.
    #[must_use]
    pub const fn prefer_node(self, node: u8) -> Flags {
        Flags { node: NodePolicy::Prefer(node), ..self }
    }

    /// The same flags, allowing only `node`.
    #[must_use]
    pub const fn only_node(self, node: u8) -> Flags {
        Flags { node: NodePolicy::Only(node), ..self }
    }
}

/// An allocation failed: no memory satisfying the flags was free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocError;

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("out of memory")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_change_one_field() {
        let flags = Flags::MAY_SLEEP.zeroed().dma32().only_node(1);
        assert_eq!(flags.context, Context::MaySleep);
        assert_eq!(flags.zone, ZoneLimit::Dma32);
        assert_eq!(flags.node, NodePolicy::Only(1));
        assert!(flags.zero);
        assert_eq!(Flags::ATOMIC.prefer_node(2).node, NodePolicy::Prefer(2));
    }
}
