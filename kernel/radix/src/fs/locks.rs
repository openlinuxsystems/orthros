// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Advisory file locks (Phase 8): BSD's `flock`, POSIX's record locks and
//! Linux's open file description locks, which radix-file(2)'s `lock`
//! takes and `lock_holder` tests (docs/design/posix.md).
//!
//! A lock belongs to an [`Owner`]: an open file description (a
//! [`FileObject`](super::FileObject), by its address) or a process (by its
//! number). What a file's list of locks does is `radix_vfs::locks`'s,
//! which the host tests; this module keeps the lists, and has locks wait.
//! The locks of every file that has any are kept in one table, by the
//! address of the file's node, which a weak reference keeps from being
//! reused while they last. The table's lock is a leaf, taken
//! after every other, so that locks can be let go of anywhere: when a
//! description's last reference goes, which may be with the capability
//! store locked, and when a process ends. What it allocates under that
//! lock it allocates without sleeping.
//!
//! A lock in another's way waits until a lock of the file is let go of
//! (or a signal comes) and tries again. Before a process lock waits, the
//! chain of processes waiting for each other's locks is followed from
//! the holder in its way: if it leads back to the caller's process, the
//! wait would never end, and the lock fails with deadlock instead, as
//! Linux's do (following ten links at most).

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::sync::Weak;
use alloc::vec::Vec;
use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::SpinLock;
pub use radix_vfs::locks::{Kind, Lock, Owner, Space};
use radix_vfs::mem::Alloc;
use radix_vfs::{KArc, Node, NodeRef};

use crate::sched::{self, wait};
use crate::user::posix::signal::{Restart, interrupted};

/// A file's locks.
struct File {
    /// Its node's address.
    node: usize,
    /// Keeps that address from going to another node while the file has
    /// locks: a process's can outlive its capabilities of the file, if it
    /// gave them away.
    _pin: Weak<dyn Node, Alloc>,
    /// Its locks; one owner's in one space never overlap.
    locks: Vec<Lock, Kmalloc>,
}

/// Every file's locks, and who waits for whom.
struct Table {
    files: Vec<File, Kmalloc>,
    /// The processes waiting for a process lock, each with the owner of
    /// the lock in its way: the links [`Table::deadlocks`] follows.
    waiting: Vec<(u64, Owner), Kmalloc>,
}

/// The table, made by the first lock.
static TABLE: SpinLock<Option<Table>> = SpinLock::new(None);

/// Moves whenever a lock goes or gives way, under the table's lock:
/// waiting locks wait for it to move.
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// The longest chain of waiting processes a deadlock is looked for in, as
/// Linux's MAX_DEADLK_ITERATIONS.
const CHAIN_MAX: usize = 10;

/// What the table allocates with, its lock held.
fn atomic() -> Kmalloc {
    HEAP.with(Flags::ATOMIC)
}

/// The key the locks of the file whose node is at `node` wait on.
fn key(node: usize) -> usize {
    // Nodes are word-aligned: the low bit keeps the key apart from the
    // node's own.
    node | 1
}

impl Table {
    fn new() -> Table {
        Table { files: Vec::new_in(atomic()), waiting: Vec::new_in(atomic()) }
    }

    fn file(&mut self, node: usize) -> Option<&mut File> {
        self.files.iter_mut().find(|file| file.node == node)
    }

    /// The first lock of the file at `node` in `lock`'s way.
    fn blocker(&self, node: usize, lock: &Lock) -> Option<Lock> {
        let file = self.files.iter().find(|file| file.node == node)?;
        radix_vfs::locks::blocker(&file.locks, lock)
    }

    /// Whether process `me` waiting for `holder`'s lock would wait
    /// forever: `holder` waits, in a chain, for `me`.
    fn deadlocks(&self, me: u64, holder: Owner) -> bool {
        let mut next = holder;
        for _ in 0..CHAIN_MAX {
            let Owner::Process(process) = next else { return false };
            if process == me {
                return true;
            }
            match self.waiting.iter().find(|(waiter, _)| *waiter == process) {
                Some(&(_, holder)) => next = holder,
                None => return false,
            }
        }
        false
    }

    /// Gives `owner`'s locks in `space` on `range` (first and last byte)
    /// of the file at `node` to `kind`, or with `None` lets go of them, as
    /// `radix_vfs::locks::set` does, and wakes the file's waiting locks if
    /// that can let one in. Fails only for want of memory, with nothing
    /// changed.
    fn set(
        &mut self,
        node: &NodeRef,
        owner: Owner,
        space: Space,
        kind: Option<Kind>,
        range: (u64, u64),
    ) -> Result<(), Status> {
        let address = radix_vfs::address(node);
        let no_memory = || Kernel::NoMemory.status(0);
        if self.file(address).is_none() {
            if kind.is_none() {
                return Ok(());
            }
            self.files.try_reserve(1).map_err(|_| no_memory())?;
            let file =
                File { node: address, _pin: KArc::downgrade(node), locks: Vec::new_in(atomic()) };
            let _ = self.files.push_within_capacity(file);
        }
        let index = self.files.iter().position(|file| file.node == address).unwrap_or_default();
        let locks = &mut self.files[index].locks;
        let freed = radix_vfs::locks::set(locks, owner, space, kind, range);
        if locks.is_empty() {
            self.files.swap_remove(index);
        }
        if freed.map_err(|_| no_memory())? {
            changed(address);
        }
        Ok(())
    }

    /// Lets go of the locks of `owner` (in every space) on the file at
    /// `node`, or with `None` on every file, and wakes the waiting locks
    /// of the files that had any.
    fn release(&mut self, owner: Owner, node: Option<usize>) {
        self.files.retain_mut(|file| {
            if node.is_some_and(|node| node != file.node) {
                return true;
            }
            let before = file.locks.len();
            file.locks.retain(|lock| lock.owner != owner);
            if file.locks.len() != before {
                changed(file.node);
            }
            !file.locks.is_empty()
        });
    }
}

/// A lock of the file at `node` went or gave way: its waiting locks try
/// again. Called with the table locked, so that none misses it.
fn changed(node: usize) {
    CHANGES.fetch_add(1, Ordering::Release);
    wait::wake_all(key(node));
}

/// Takes `lock` on the file of `node` for `me`, the calling process, or
/// with `kind` `None` lets go of its owner's locks on its range; waits,
/// with `wait`, for the locks in its way to go. radix-file(2)'s `lock`.
pub fn lock(
    node: &NodeRef,
    lock: Lock,
    kind: Option<Kind>,
    wait: bool,
    me: u64,
) -> Result<(), Status> {
    let address = radix_vfs::address(node);
    let wants = Lock { kind: kind.unwrap_or(Kind::Shared), ..lock };
    // A description's lock on the whole file changes kind as Linux's
    // `flock` does: the old lock goes first, so that two holders of
    // shared locks both asking for an exclusive one do not wait for
    // each other forever.
    let mut converting = lock.space == Space::Whole && kind.is_some();
    loop {
        let seen = CHANGES.load(Ordering::Acquire);
        let holder = {
            let mut guard = TABLE.lock();
            let table = guard.get_or_insert_with(Table::new);
            if converting {
                converting = false;
                let held = table.file(address).and_then(|file| {
                    file.locks
                        .iter()
                        .find(|held| held.owner == lock.owner && held.space == lock.space)
                });
                match held {
                    Some(held) if Some(held.kind) == kind => return Ok(()),
                    Some(_) => table.set(node, lock.owner, lock.space, None, (0, u64::MAX))?,
                    None => {}
                }
            }
            let Some(holder) = kind.and_then(|_| table.blocker(address, &wants)) else {
                return table.set(node, lock.owner, lock.space, kind, (lock.start, lock.end));
            };
            if !wait {
                return Err(Kernel::WouldBlock.status(0));
            }
            if matches!(lock.owner, Owner::Process(_)) {
                if table.deadlocks(me, holder.owner) {
                    return Err(FileError::Deadlock.status(0));
                }
                table.waiting.try_reserve(1).map_err(|_| Kernel::NoMemory.status(0))?;
                let _ = table.waiting.push_within_capacity((me, holder.owner));
            }
            holder.owner
        };
        wait::wait_until(key(address), "file lock", || {
            CHANGES.load(Ordering::Acquire) != seen || sched::interrupted()
        });
        if matches!(lock.owner, Owner::Process(_)) {
            let mut guard = TABLE.lock();
            if let Some(table) = guard.as_mut() {
                let link = table.waiting.iter().position(|&link| link == (me, holder));
                if let Some(link) = link {
                    table.waiting.swap_remove(link);
                }
            }
        }
        if sched::interrupted() {
            return Err(interrupted(Restart::Always));
        }
    }
}

/// The first lock of the file of `node` that stands in `lock`'s way.
/// radix-file(2)'s `lock_holder`.
pub fn holder(node: &NodeRef, lock: &Lock) -> Option<Lock> {
    let guard = TABLE.lock();
    guard.as_ref()?.blocker(radix_vfs::address(node), lock)
}

/// Lets go of the locks of the open file description at `description` on
/// the file of `node`: its last reference has gone.
pub fn description_gone(node: &NodeRef, description: usize) {
    if let Some(table) = TABLE.lock().as_mut() {
        table.release(Owner::Description(description), Some(radix_vfs::address(node)));
    }
}

/// Lets go of the locks of process `process` on the file of `node`, or
/// with `None` on every file: it deleted a capability of the file, or
/// ended.
pub fn process_gone(process: u64, node: Option<&NodeRef>) {
    if let Some(table) = TABLE.lock().as_mut() {
        table.release(Owner::Process(process), node.map(radix_vfs::address));
    }
}
