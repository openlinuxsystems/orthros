// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Radix memory manager.
//!
//! This crate is the architecture-independent half of memory management:
//!
//! - [`Frames`], the frame allocator: [buddy](buddy) allocators per zone
//!   (`DMA32` and `Normal`) and per NUMA node, with reserves for atomic
//!   callers and fallback to the nearest node;
//! - [`Heap`], the kernel heap: slab caches per size class with per-CPU
//!   magazines in front ([`slab`], [`magazine`], [`class`]);
//! - [`Ranges`], the allocator of the kernel's mapping window, where
//!   stacks and device memory are mapped with guard gaps between them;
//! - [`Pcids`], the allocator of address-space identifiers;
//! - [`bytes`], byte vectors zeroed or filled with one string store;
//! - [`lease`], memory lent as a `&'static mut` slice that comes back
//!   when the slice is handed back, as smoltcp's socket buffers do;
//! - the heap [stress test](stress) the kernel runs on every CPU.
//!
//! Every allocation takes [`Flags`] that say whether the caller may sleep,
//! which memory it needs and from which node (spec 3.4), and every failure
//! is an [`AllocError`]: nothing here panics because memory ran out.
//!
//! The page tables themselves are architecture code (`radix-arch-x86_64`),
//! which hands physical memory to [`Frames`] at boot and implements
//! [`Platform`] for the heap.
//!
//! Most of the crate is safe code. The exceptions are the functions that
//! touch memory the allocators manage, each `unsafe` with its contract
//! written down: the `raw` module, the frame allocator's zeroing of the
//! blocks it hands out, the slab layer's free-object pattern, the heap's
//! `free`, the [`Platform`] trait, the stress test, the lengths
//! [`bytes`] sets and the memory a [`lease`] takes back.
//! The crate builds for the host too, where its unit and property tests
//! run, and for Kani, which proves the allocators' invariants
//! (`docs/design/memory.md`).

#![cfg_attr(not(any(test, kani)), no_std)]
#![deny(unsafe_code)]
// `AllocatorClone`, which `Arc`s in the heap need to be cloned.
#![cfg_attr(not(kani), feature(allocator_ext))]

extern crate alloc;

pub mod asid;
pub mod buddy;
#[cfg(not(kani))]
pub mod bytes;
pub mod class;
pub mod context;
pub mod frames;
pub mod heap;
pub mod lease;
pub mod magazine;
pub mod page;
pub mod platform;
mod raw;
pub mod slab;
pub mod stress;
pub mod vrange;

#[cfg(kani)]
mod proofs;

pub use asid::Pcids;
pub use buddy::{FreeError, MAX_ORDER};
pub use context::{AllocError, Context, Flags, NodePolicy, ZoneLimit};
pub use frames::{
    Frames, MAX_NODES, MAX_RETIRED, OOM_MAX_ORDER, RUN_FRAMES, Retired, SetupError, ZoneKind,
    ZoneStats,
};
pub use heap::{ClassStats, ExplicitOnly, Heap, HeapStats, Kmalloc};
pub use page::{Owner, PAGE_SHIFT, PAGE_SIZE, PageInfo, Region, State};
pub use platform::{MAX_CPUS, Platform};
pub use slab::Corruption;
pub use vrange::{RangeError, Ranges};

/// The kernel's frame allocator.
pub static FRAMES: Frames = Frames::new();

/// The kernel heap.
pub static HEAP: Heap = Heap::new(&FRAMES);

#[cfg(test)]
pub(crate) mod tests {
    //! Helpers for the host tests: a platform whose "physical" memory is
    //! host memory (the direct map starts at 0), with the CPU number kept
    //! per thread.

    use std::alloc::{Layout, alloc_zeroed};
    use std::cell::Cell;

    use super::*;

    thread_local! {
        static CPU: Cell<usize> = const { Cell::new(0) };
    }

    /// Makes the calling thread CPU `cpu`.
    pub fn become_cpu(cpu: usize) {
        CPU.with(|c| c.set(cpu));
    }

    pub struct HostPlatform {
        nodes: usize,
        direct_map: usize,
    }

    impl HostPlatform {
        /// A platform with `nodes` nodes (CPU `n` is on node `n % nodes`)
        /// whose direct map starts at `direct_map`.
        pub fn leak(nodes: usize, direct_map: usize) -> &'static HostPlatform {
            Box::leak(Box::new(HostPlatform { nodes: nodes.max(1), direct_map }))
        }
    }

    // SAFETY: tests only give the allocators regions of host memory they
    // allocated and leaked, at `direct_map` + their physical address, or
    // regions they never touch through the direct map.
    #[allow(unsafe_code)]
    unsafe impl Platform for HostPlatform {
        fn current_cpu(&self) -> usize {
            CPU.with(Cell::get)
        }

        fn cpu_node(&self, cpu: usize) -> usize {
            cpu % self.nodes
        }

        fn disable_interrupts(&self) -> bool {
            false
        }

        fn restore_interrupts(&self, _were_enabled: bool) {}

        fn direct_map(&self) -> usize {
            self.direct_map
        }
    }

    /// `n` fresh frame records.
    pub fn records(n: usize) -> &'static [PageInfo] {
        Box::leak((0..n).map(|_| PageInfo::new()).collect::<Vec<_>>().into_boxed_slice())
    }

    /// Where the tests' "physical" memory starts: 4 GiB, so it is all in
    /// `Normal` zones.
    pub const PHYS_BASE: u64 = 1 << 32;

    /// A frame allocator and heap over `pages` frames of host memory, split
    /// evenly across `nodes` nodes.
    pub fn heap_with(pages: usize, nodes: usize) -> &'static Heap {
        let layout = Layout::from_size_align(pages * PAGE_SIZE, 4 << 20).unwrap();
        // SAFETY: the layout has a non-zero size.
        #[allow(unsafe_code)]
        let host = unsafe { alloc_zeroed(layout) } as usize;
        assert!(host != 0);
        let frames: &'static Frames = Box::leak(Box::new(Frames::new()));
        frames.set_platform(HostPlatform::leak(nodes, host.wrapping_sub(PHYS_BASE as usize)));
        frames.set_topology(nodes, |a, b| if a == b { 10 } else { 20 });
        let per_node = pages / nodes;
        for node in 0..nodes {
            let base = (PHYS_BASE >> PAGE_SHIFT) + (node * per_node) as u64;
            frames.add_region(node, Region { base, pages: records(per_node) }).unwrap();
            frames.release(base << PAGE_SHIFT, (base + per_node as u64) << PAGE_SHIFT);
        }
        frames.start();
        Box::leak(Box::new(Heap::new(frames)))
    }
}
