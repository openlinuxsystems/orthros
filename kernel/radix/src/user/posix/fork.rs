// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `fork` (Phase 8, radix-process(2)): a child process that is a copy of
//! the caller.
//!
//! The child is a process of its own, charged to the caller's resource
//! domain like one `create` makes, but it does not end with the caller: it
//! holds its own original capability (in SLOT_PROCESS), and the process
//! tree holds it for its parent to wait for. It gets:
//!
//! - a capability space of the caller's size, with a copy of each of the
//!   caller's capabilities that has the copy and transfer rights, in the
//!   same slot and with the slot's flags (`close_on_exec`); a copy of a
//!   file capability is the same open file description, offset and all;
//! - copies of the caller's kept capabilities (the frames of a loaded
//!   image, which the child maps too) and a capability to its own address
//!   space in SLOT_SPACE;
//! - an address space with the caller's memory (`SpaceObject::fork_into`);
//! - the caller's signal actions and controlling terminal, the calling
//!   thread's signal mask and alternate stack, and nothing pending;
//! - one thread, in SLOT_THREAD, which goes on from the call with the
//!   calling thread's registers, extended state and thread pointer, and
//!   gets 0 where the caller gets the child's number.
//!
//! The child joins the tree before its thread starts, so that its end is
//! always reported; a fork that fails after that takes it out again
//! unreported.

use radix_abi::errors::Kernel;
use radix_abi::process::{EXIT_STOPPED, ForkResults, KEEP_SLOTS};
use radix_abi::startup;
use radix_abi::syscalls::CapCallResults;
use radix_cap::{CapError, Derivation, ObjectKind, Rights, SlotRef};
use radix_hal::{SyscallRegs, UserMode};
use radix_ipc::info::Info;

use super::{OWN_SLOTS, join, tree_error, with_tree};
use crate::user::account::Account;
use crate::user::process::{self, ProcessObject, ProcessRef, Start};
use crate::user::{Result, UserRef, cap_error, error};
use crate::{Arch, caps};

/// `fork`, called by `user`'s thread as method `label`: makes the child
/// and returns its number.
#[inline(never)]
pub fn fork(user: &UserRef, label: u32) -> Result<u64> {
    let parent = &user.process;
    // The child's thread comes back from this call with 0. (AArch64's
    // registers are a stub, `()`, until its user mode comes.)
    #[allow(clippy::let_unit_value)]
    let mut regs = Arch::user_registers().ok_or(error(Kernel::Gone))?;
    let words = ForkResults { child: 0 }.encode();
    let reply = CapCallResults { badge: 0, info: Info::reply(label, ForkResults::WORDS), words };
    Arch::set_results(&mut regs, &SyscallRegs { number: 0, args: reply.encode() });
    let thread_pointer = Arch::thread_pointer(&regs);

    let slots = caps::with(|store| store.slots(parent.cspace()))
        .and_then(|slots| slots)
        .map_err(cap_error)?;
    let charge = Account::charge(parent.account(), process::bytes_of(slots))?;
    let account = Account::new(parent.account().domain().clone()).ok_or(error(Kernel::NoMemory))?;
    let name = parent.name();
    let child = process::create(None, name.as_str(), slots, account, Some(charge), None, None)?;

    let made = (|| -> Result<()> {
        copy_capabilities(parent, &child, slots)?;
        let from = parent.space().ok_or(error(Kernel::Gone))?;
        let to = child.space().ok_or(error(Kernel::Gone))?;
        from.fork_into(&to)?;
        {
            let parent = parent.posix.lock();
            // The child is new: nothing else takes its lock yet.
            let mut posix = child.posix.lock_nested(1);
            posix.actions.copy_from(&parent.actions);
            posix.terminal = parent.terminal.clone();
        }
        with_tree(|tree| -> Result<()> {
            join(tree, parent)?;
            tree.insert_child(child.number(), parent.number(), child.clone()).map_err(tree_error)
        })?;
        child.posix.lock().in_tree = true;
        Ok(())
    })();
    if let Err(err) = made {
        abandon(&child);
        return Err(err);
    }

    let (mask, stack) = {
        let signals = user.signals.lock();
        (signals.mask, signals.stack)
    };
    let started = ProcessObject::start(
        &child,
        Start::Registers(regs),
        startup::SLOT_THREAD,
        None,
        mask,
        &mut |thread| {
            thread.copy_extended_state();
            thread.set_thread_pointer(thread_pointer);
            if let Some(user) = thread.user() {
                user.signals.lock().stack = stack;
            }
            Ok(())
        },
    );
    if let Err(err) = started {
        abandon(&child);
        return Err(err);
    }
    Ok(child.number())
}

/// Ends a child a fork could not finish, unreported: out of the tree
/// first, so that its end tells nobody.
fn abandon(child: &ProcessRef) {
    let mut gone = None;
    let in_tree = core::mem::replace(&mut child.posix.lock().in_tree, false);
    if in_tree {
        with_tree(|tree| tree.remove(child.number(), &mut |handle| gone = Some(handle)));
    }
    drop(gone);
    child.exit(EXIT_STOPPED);
}

/// Gives `child` copies of `parent`'s capabilities, as [`fork`] says.
fn copy_capabilities(parent: &ProcessObject, child: &ProcessObject, slots: u32) -> Result<()> {
    let (from_space, to_space) = (parent.cspace(), child.cspace());
    caps::with(|store| -> core::result::Result<(), CapError> {
        let mut first = 0;
        while first < slots {
            let mut used = store.used_slots(from_space, first)?;
            while used != 0 {
                let slot = first + used.trailing_zeros();
                used &= used - 1;
                if slot >= slots || OWN_SLOTS.contains(&slot) {
                    continue;
                }
                let from = SlotRef::new(from_space, slot);
                if !store.get(from)?.rights().contains(Rights::COPY.union(Rights::TRANSFER)) {
                    continue;
                }
                let to = SlotRef::new(to_space, slot);
                store.derive(from, to, Derivation::COPY)?;
                let flags = store.slot_flags(from)?;
                if flags != 0 {
                    store.set_slot_flags(to, flags)?;
                }
            }
            first += 64;
        }
        for slot in 1..=KEEP_SLOTS {
            let from = SlotRef::new(parent.keep_space(), slot);
            match store.get(from) {
                Ok(_) => {
                    store.derive(from, SlotRef::new(child.keep_space(), slot), Derivation::COPY)?
                }
                Err(CapError::Empty) => {}
                Err(err) => return Err(err),
            }
        }
        let how = Derivation { rights: ObjectKind::AddressSpace.rights(), ..Derivation::COPY };
        store.derive(
            SlotRef::new(child.keep_space(), 0),
            SlotRef::new(to_space, startup::SLOT_SPACE),
            how,
        )
    })
    .and_then(|done| done)
    .map_err(cap_error)
}
