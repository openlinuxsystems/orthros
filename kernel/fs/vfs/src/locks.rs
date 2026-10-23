// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A file's advisory locks (Phase 8): what its list of locks becomes when
//! a lock is taken, changed or let go of, and which lock stands in
//! another's way. The kernel keeps the lists, and has locks wait
//! (radix's `fs::locks`); radix-file(2)'s `lock` says what they mean.
//!
//! A lock belongs to an [`Owner`]: an open file description or a process.
//! Locks on the whole file, as BSD's `flock` takes, meet only each other;
//! locks on ranges, POSIX's and Linux's open file description locks, meet
//! each other ([`Space`]). One owner's locks in one space never overlap:
//! a new lock takes the place of the old ones on its range, and joins
//! those of its kind that it overlaps or borders, as POSIX's do.

use alloc::vec::Vec;
use core::alloc::Allocator;

use crate::NoMemory;

/// Whose a lock is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// An open file description, by its object's address.
    Description(usize),
    /// A process, by its number.
    Process(u64),
}

/// Which locks a lock meets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// Locks on the whole file, as `flock` takes.
    Whole,
    /// Locks on ranges of bytes: processes' and descriptions'.
    Range,
}

/// What a lock lets others do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Others may hold shared locks on its range too.
    Shared,
    /// Nobody else may lock its range.
    Exclusive,
}

/// A lock, or what a lock is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lock {
    /// Whose it is.
    pub owner: Owner,
    /// Which locks it meets.
    pub space: Space,
    /// What it lets others do.
    pub kind: Kind,
    /// Its first byte.
    pub start: u64,
    /// Its last byte: `u64::MAX` for however far the file grows.
    pub end: u64,
}

impl Lock {
    /// Whether it covers a byte from `start` to `end`.
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start <= end && start <= self.end
    }

    /// Whether it covers or borders a byte from `start` to `end`: a lock
    /// of the same kind there joins it.
    fn touches(&self, start: u64, end: u64) -> bool {
        self.overlaps(start.saturating_sub(1), end.saturating_add(1))
    }

    /// Whether it stands in `other`'s way.
    pub fn blocks(&self, other: &Lock) -> bool {
        self.space == other.space
            && self.owner != other.owner
            && self.overlaps(other.start, other.end)
            && (self.kind == Kind::Exclusive || other.kind == Kind::Exclusive)
    }
}

/// The first of `locks` that stands in `lock`'s way.
pub fn blocker(locks: &[Lock], lock: &Lock) -> Option<Lock> {
    locks.iter().find(|held| held.blocks(lock)).copied()
}

/// Gives `owner`'s locks in `space` on the bytes from `start` to `end` to
/// `kind`, or with `None` lets go of them, whatever stands in the way
/// (the caller has looked with [`blocker`]). Returns whether a lock went
/// or weakened, which can let a waiting lock in; fails only for want of
/// memory, with nothing changed.
pub fn set<A: Allocator>(
    locks: &mut Vec<Lock, A>,
    owner: Owner,
    space: Space,
    kind: Option<Kind>,
    (start, end): (u64, u64),
) -> Result<bool, NoMemory> {
    // At most two pieces of old locks are left beside the new one: those
    // of the one lock reaching past each of its ends.
    locks.try_reserve(3).map_err(|_| NoMemory)?;
    let (mut start, mut end) = (start, end);
    let mut left = [None; 2];
    let mut freed = false;
    let mut i = 0;
    while i < locks.len() {
        let old = locks[i];
        if old.owner != owner || old.space != space {
            i += 1;
        } else if Some(old.kind) == kind && old.touches(start, end) {
            // It joins the new lock.
            (start, end) = (start.min(old.start), end.max(old.end));
            locks.swap_remove(i);
        } else if old.overlaps(start, end) {
            // The new lock takes its place on the bytes they share.
            if old.start < start {
                left[0] = Some(Lock { end: start - 1, ..old });
            }
            if old.end > end {
                left[1] = Some(Lock { start: end + 1, ..old });
            }
            freed |= kind != Some(Kind::Exclusive);
            locks.swap_remove(i);
        } else {
            i += 1;
        }
    }
    if let Some(kind) = kind {
        let _ = locks.push_within_capacity(Lock { owner, space, kind, start, end });
    }
    for piece in left.into_iter().flatten() {
        let _ = locks.push_within_capacity(piece);
    }
    Ok(freed)
}
