// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Starting the self-tests' programs from the kernel.
//!
//! The boot loader hands the kernel a module, `usertests`: one statically
//! linked ELF64 executable with every user-mode test program in it, each
//! a *role* the program picks from its start-up page. A [`Group`] starts
//! programs from it, each in a process of its own (`loader`), with its
//! thread pinned to the CPU asked for, and waits for them. A group keeps
//! the objects its programs share and their processes' original
//! capabilities in a capability space of its own; when the group ends,
//! its programs are stopped and that space goes, and with it everything
//! the programs had.

use alloc::vec::Vec;

use radix_abi::errors::Kernel;
use radix_cap::{ObjectKind, Rights, SlotRef, SpaceId};
use radix_hal::BootModule;
use radix_klog::{info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::Once;

use super::loader::{self, EXTRA_MAX, Program};
use super::objects::Object;
use super::process::{self, ProcessRef};
use super::{EXIT_STOPPED, Result, cap_error, error, ipc};
use crate::sched::{self, Name};
use crate::{caps, domains};

/// The word on the boot loader's module line that marks the test image.
pub const MODULE_WORD: &str = "usertests";

/// Each program's memory object: 2^order pages.
const MEMORY_ORDER: u32 = 9;

/// Slots of a group's own space, for the objects it shares and its
/// programs' processes.
const LAUNCHER_SLOTS: u32 = 64;

static IMAGE: Once<&'static [u8]> = Once::new();

/// Finds the test image among the boot modules.
pub fn set_modules(modules: &'static [BootModule]) {
    if let Some(module) = modules.iter().find(|m| m.has_word(MODULE_WORD)) {
        let _ = IMAGE.set(module.bytes);
        info!("User: the {MODULE_WORD} image is {} KiB", module.bytes.len() / 1024);
    }
}

/// The test image, if the boot loader loaded one.
pub fn image() -> Option<&'static [u8]> {
    IMAGE.get().copied()
}

/// What to start.
pub struct Spec<'a> {
    /// The program's name.
    pub name: &'a str,
    /// Which program of the image runs.
    pub role: u32,
    /// A word for the program, in its start-up page.
    pub argument: u64,
    /// The CPU it runs on.
    pub cpu: usize,
    /// The group's objects it gets, from `SLOT_FIRST_EXTRA` on: the group's
    /// slot, the rights to keep, and a badge (0 for none).
    pub extra: &'a [(u32, Rights, u64)],
}

/// Programs started together, and the objects they share.
pub struct Group {
    launcher: SpaceId,
    next: u32,
    started: Vec<(Name, ProcessRef), Kmalloc>,
}

impl Group {
    /// A new group, with nothing in it.
    pub fn new() -> Result<Group> {
        let launcher = caps::with(|store| store.create_space(LAUNCHER_SLOTS))
            .and_then(|made| made)
            .map_err(cap_error)?;
        let started = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
        Ok(Group { launcher, next: 0, started })
    }

    /// The group's next slot, taken.
    fn take_slot(&mut self) -> Result<SlotRef> {
        if self.next == LAUNCHER_SLOTS {
            return Err(error(Kernel::NoRoom));
        }
        self.next += 1;
        Ok(SlotRef::new(self.launcher, self.next - 1))
    }

    /// Makes an endpoint for the group's programs to share.
    pub fn endpoint(&mut self) -> Result<u32> {
        let endpoint = ipc::Endpoint::new().ok_or(error(Kernel::NoMemory))?;
        let at = self.take_slot()?;
        process::install(at, ObjectKind::Endpoint, Object::Endpoint(endpoint))?;
        Ok(at.slot)
    }

    /// Makes a notification for the group's programs to share.
    pub fn notification(&mut self) -> Result<u32> {
        let notification = super::notify::Notification::new().ok_or(error(Kernel::NoMemory))?;
        let at = self.take_slot()?;
        process::install(at, ObjectKind::Notification, Object::Notification(notification))?;
        Ok(at.slot)
    }

    /// Makes a memory object of 2^`order` pages for the group's programs.
    pub fn memory(&mut self, order: u32) -> Result<u32> {
        let at = self.take_slot()?;
        caps::create_memory(at, order).map_err(cap_error)?;
        Ok(at.slot)
    }

    /// Gives the group's programs the directory at `at` to share.
    pub fn directory(&mut self, at: radix_vfs::mount::Location) -> Result<u32> {
        let object = crate::fs::FileObject::new(at).ok_or(error(Kernel::NoMemory))?;
        let slot = self.take_slot()?;
        process::install(slot, ObjectKind::Directory, Object::Directory(object))?;
        Ok(slot.slot)
    }

    /// Starts a program. Returns its number in the group.
    pub fn start(&mut self, spec: &Spec<'_>) -> Result<usize> {
        if spec.extra.len() > EXTRA_MAX {
            return Err(error(Kernel::InvalidArgument));
        }
        let mut extra = [(SlotRef::new(self.launcher, 0), Rights::NONE, 0); EXTRA_MAX];
        for (to, &(slot, rights, badge)) in extra.iter_mut().zip(spec.extra) {
            *to = (SlotRef::new(self.launcher, slot), rights, badge);
        }
        self.started.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        let at = self.take_slot()?;
        let program = Program {
            name: spec.name,
            image: image().ok_or(error(Kernel::Gone))?,
            cpu: Some(spec.cpu),
            memory_order: MEMORY_ORDER,
            role: u64::from(spec.role),
            argument: spec.argument,
            args: &[spec.name],
            extra: &extra[..spec.extra.len()],
            archive: None,
            root: false,
            domain: domains::root(),
            on_exit: None,
        };
        let process = loader::start(at, &program)?;
        // Room was reserved above.
        let _ = self.started.push_within_capacity((Name::new(spec.name), process));
        Ok(self.started.len() - 1)
    }

    /// Waits until every program has exited, or `timeout_ns` has passed.
    /// Returns whether they all exited.
    pub fn wait(&self, timeout_ns: u64) -> bool {
        let deadline = sched::now_ns().saturating_add(timeout_ns);
        self.started.iter().all(|(_, process)| process.wait_until(deadline))
    }

    /// Program `index`'s exit code, once it has exited.
    pub fn status(&self, index: usize) -> Option<u64> {
        self.started.get(index)?.1.state().1
    }

    /// Program `index`'s name.
    pub fn name(&self, index: usize) -> &str {
        self.started.get(index).map_or("?", |(name, _)| name.as_str())
    }

    /// How many programs were started.
    pub fn len(&self) -> usize {
        self.started.len()
    }

    /// Stops every program still running, and waits for them.
    pub fn stop(&self) {
        for (_, process) in &self.started {
            process.exit(EXIT_STOPPED);
        }
        if !self.wait(2_000_000_000) {
            warn!("User: a program did not stop within 2 s");
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.stop();
        if let Err(err) = caps::destroy_space(self.launcher) {
            warn!("User: could not destroy a group's capability space: {err}");
        }
    }
}
