//! The Radix capability core.
//!
//! Every resource in Radix is reached through a capability (spec 2.3), and
//! this crate is where capabilities live:
//!
//! - [`Rights`], the twelve rights a capability can grant, and
//!   [`ObjectKind`], the twelve kinds of object it can designate: memory,
//!   frames, address spaces, threads, processes, endpoints, notifications,
//!   interrupt lines, I/O ports, device registers, DMA buffers and resource
//!   domains;
//! - [`Capability`], a value holding an object, its rights, a badge and
//!   the key it is sealed with, together with the checks that decide what
//!   it may be used for and what may be made from it;
//! - [`Capabilities`], the store: capability spaces of slots, the table of
//!   kernel objects, and the derivation tree through which capabilities
//!   are delegated and revoked.
//!
//! Capabilities cannot be forged: [`Capability`] has no public
//! constructor, the store makes an original capability only for an object
//! it creates, and every other capability is made from an existing one by
//! an operation that refuses to add rights, widen a range, change a badge
//! or use a sealed capability. `docs/design/capabilities.md` argues this
//! in full; the Kani proofs in `proofs.rs` check the rights checks for all
//! inputs, and `tla/Capabilities.tla` models derivation and revocation.
//!
//! The crate is safe code, builds for the host, where its unit and
//! property tests run, and is checked by Kani.

#![cfg_attr(not(any(test, kani)), no_std)]
#![forbid(unsafe_code)]
#![feature(vec_push_within_capacity)]
// Kani's own, older nightly has the allocator API unstable still.
#![cfg_attr(kani, feature(allocator_api))]

extern crate alloc;

mod cap;
mod error;
mod object;
mod rights;
mod store;

#[cfg(kani)]
mod proofs;
#[cfg(test)]
mod tests;

pub use cap::{Capability, Derivation, carve};
pub use error::CapError;
pub use object::{KINDS, Object, ObjectId, ObjectKind, Target};
pub use rights::Rights;
pub use store::{
    Capabilities, Census, Extent, Inconsistency, MAX_DEPTH, MAX_RETYPE, MAX_SLOTS, ObjectInfo,
    Progress, Released, SlotRef, SpaceId,
};
