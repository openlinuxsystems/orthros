// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Radix scheduler's policy.
//!
//! The kernel's scheduler (`kernel/radix/src/sched`) owns threads, run
//! queues, context switches and locking. The decisions it makes along the
//! way are here, as plain data structures with no locks and no hardware,
//! so they can be tested on the host and checked by Kani:
//!
//! - [`weight`]: nice levels and their weights, real-time priorities and
//!   resource-domain weights;
//! - [`CpuSet`], for affinity masks and topology spans;
//! - [`FairQueue`], the fair class: an EEVDF run queue (one per CPU, and
//!   one per resource domain per CPU for the domains' CPU weights);
//! - [`RtQueue`], the FIFO and round-robin real-time classes;
//! - [`Topology`]: the CPUs' cores, packages and NUMA nodes, and from them
//!   where a waking thread runs and what load balancing moves.
//!
//! The queues never allocate on their own: the kernel grows them ahead of
//! time, outside its locks. `docs/design/scheduler.md` describes the
//! scheduler as a whole and argues that it is correct.
//!
//! The crate is safe code, builds for the host, where its unit and
//! property tests run, and is checked by Kani.

#![cfg_attr(not(any(test, kani)), no_std)]
#![forbid(unsafe_code)]
#![feature(vec_push_within_capacity)]
// Kani's own, older nightly has the allocator API unstable still.
#![cfg_attr(kani, feature(allocator_api))]

extern crate alloc;

pub mod cpuset;
pub mod fair;
pub mod rt;
pub mod topology;
pub mod weight;

#[cfg(kani)]
mod proofs;

pub use cpuset::{CpuSet, MAX_CPUS};
pub use fair::{BASE_SLICE_NS, Entity, FairQueue, Placement, virtual_ns};
pub use rt::{RR_QUANTUM_NS, RtEntry, RtQueue};
pub use topology::{
    CpuLoad, Level, MAX_NODES, Place, Topology, interval_ticks, is_cache_hot, should_pull,
};
pub use weight::{
    DOMAIN_WEIGHT_DEFAULT, DOMAIN_WEIGHT_MAX, NICE_0_WEIGHT, NICE_MAX, NICE_MIN, RT_PRIO_MAX,
    RT_PRIO_MIN, domain_weight, nice_valid, rt_prio_valid, weight,
};
