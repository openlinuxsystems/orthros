// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Starting programs from the kernel, until Phase 6 brings processes and
//! init.
//!
//! The boot loader hands the kernel a module, `usertests`: one statically
//! linked ELF64 executable with every user-mode test program in it, each
//! a *role* the program picks from its start-up page. A [`Group`] starts
//! programs from it and waits for them. Each program gets:
//!
//! - its own capability space of `startup::SLOTS` slots, holding its
//!   thread, its address space, a memory object to make frames from, the
//!   random number generator and the kernel log (the slots radix-startup(2)
//!   lists), and from `SLOT_FIRST_EXTRA` on the objects the group shares
//!   with it;
//! - its own address space, with the image's segments, a stack below
//!   `STACK_TOP` and the read-only start-up page at `STARTUP_ADDRESS`;
//! - one thread, pinned to the CPU asked for, which starts at the image's
//!   entry point with the start-up page's address as its argument.
//!
//! The frames for the image, stack and start-up page come from the
//! program's own memory object, but their capabilities are kept in a
//! second, hidden space, so the program cannot pull its own code from
//! under itself by accident. When the group ends, every space goes, and
//! with it everything the programs had.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::Status;
use radix_abi::address_space::{PAGE_SIZE, USER_START};
use radix_abi::errors::Kernel;
use radix_abi::startup::{self, Startup};
use radix_cap::{Derivation, ObjectKind, Rights, SlotRef, SpaceId};
use radix_elf::Access;
use radix_hal::{BootModule, UserAccess, UserMode};
use radix_klog::{info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::{Once, SpinLock};

use super::memory::{FrameObject, FrameRef, SpaceObject, SpaceRef};
use super::methods::retype;
use super::objects::{self, Object};
use super::{Result, UserRef, UserThread, cap_error, default_window, error, ipc, thread_main};
use crate::sched::{self, Options, ThreadRef, wait};
use crate::{Arch, caps};

/// The word on the boot loader's module line that marks the test image.
pub const MODULE_WORD: &str = "usertests";

/// Each program's memory object: 2^order pages.
const MEMORY_ORDER: u32 = 9;

/// Slots of a program's hidden space: the image, stack and start-up page.
const KEEPER_SLOTS: u32 = 4;

/// The most objects a program gets from its group: the start-up page
/// says what each is.
const EXTRA_MAX: usize = 16;

/// Slots of a group's own space, for the objects it shares.
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

fn kmalloc() -> Kmalloc {
    HEAP.with(Flags::MAY_SLEEP)
}

/// A program the kernel started, as its thread sees it.
pub struct Program {
    exited: AtomicBool,
}

impl Program {
    fn key(&self) -> usize {
        self as *const Program as usize
    }

    /// Its thread has exited.
    pub fn thread_exited(&self) {
        self.exited.store(true, Ordering::Release);
        wait::wake_all(self.key());
    }
}

/// What to start.
pub struct Spec<'a> {
    /// The thread's name.
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

struct Started {
    name: sched::Name,
    program: Arc<Program, Kmalloc>,
    user: Option<UserRef>,
    thread: Option<ThreadRef>,
    cspace: Option<SpaceId>,
    keeper: Option<SpaceId>,
}

/// Programs started together, and the objects they share.
pub struct Group {
    launcher: SpaceId,
    next: u32,
    started: Vec<Started, Kmalloc>,
}

/// Why starting failed.
fn failed(what: &str, status: Status) -> Status {
    warn!("User: could not start a program: {what} failed with {:#x}", status.0);
    status
}

impl Group {
    /// A new group, with nothing in it.
    pub fn new() -> Result<Group> {
        let launcher = caps::with(|store| store.create_space(LAUNCHER_SLOTS))
            .and_then(|made| made)
            .map_err(cap_error)?;
        Ok(Group { launcher, next: 0, started: Vec::new_in(kmalloc()) })
    }

    /// Makes an object of `kind` in the group's next slot, and returns
    /// the slot.
    fn make(&mut self, kind: ObjectKind, object: Object) -> Result<u32> {
        if self.next == LAUNCHER_SLOTS {
            return Err(error(Kernel::NoRoom));
        }
        let slot = self.next;
        caps::with(|store| -> Result<()> {
            objects::reserve(store, 1).map_err(|_| error(Kernel::NoMemory))?;
            let id = store.create(SlotRef::new(self.launcher, slot), kind).map_err(cap_error)?;
            objects::insert(store, id, object).map_err(|_| error(Kernel::NoMemory))
        })
        .map_err(cap_error)
        .and_then(|made| made)?;
        self.next += 1;
        Ok(slot)
    }

    /// Makes an endpoint for the group's programs to share.
    pub fn endpoint(&mut self) -> Result<u32> {
        let endpoint = ipc::Endpoint::new().ok_or(error(Kernel::NoMemory))?;
        self.make(ObjectKind::Endpoint, Object::Endpoint(endpoint))
    }

    /// Makes a notification for the group's programs to share.
    pub fn notification(&mut self) -> Result<u32> {
        let notification = super::notify::Notification::new().ok_or(error(Kernel::NoMemory))?;
        self.make(ObjectKind::Notification, Object::Notification(notification))
    }

    /// Makes a memory object of 2^`order` pages for the group's programs.
    pub fn memory(&mut self, order: u32) -> Result<u32> {
        if self.next == LAUNCHER_SLOTS {
            return Err(error(Kernel::NoRoom));
        }
        let slot = self.next;
        caps::create_memory(SlotRef::new(self.launcher, slot), order).map_err(cap_error)?;
        self.next += 1;
        Ok(slot)
    }

    /// Starts a program. Returns its number in the group.
    pub fn start(&mut self, spec: &Spec<'_>) -> Result<usize> {
        if spec.extra.len() > EXTRA_MAX {
            return Err(error(Kernel::InvalidArgument));
        }
        let bytes = image().ok_or(error(Kernel::Gone))?;
        let top = startup::STACK_TOP - startup::STACK_SIZE;
        let image = radix_elf::read(bytes, USER_START + PAGE_SIZE..top).map_err(|err| {
            warn!("User: the {MODULE_WORD} image cannot run: {err}");
            error(Kernel::InvalidArgument)
        })?;
        self.started.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
        let program = Arc::try_new_in(Program { exited: AtomicBool::new(false) }, kmalloc())
            .map_err(|_| error(Kernel::NoMemory))?;
        let _ = self.started.push_within_capacity(Started {
            name: sched::Name::new(spec.name),
            program: program.clone(),
            user: None,
            thread: None,
            cspace: None,
            keeper: None,
        });
        let index = self.started.len() - 1;
        // What is made is recorded as it is made, so that the group's end
        // undoes it all, whatever step fails.
        let (cspace, keeper) = caps::with(|store| {
            let cspace = store.create_space(startup::SLOTS)?;
            match store.create_space(KEEPER_SLOTS) {
                Ok(keeper) => Ok((cspace, keeper)),
                Err(err) => {
                    let _ = store.destroy_space(cspace, &mut caps::release);
                    Err(err)
                }
            }
        })
        .and_then(|made| made)
        .map_err(|err| failed("making its capability spaces", cap_error(err)))?;
        self.started[index].cspace = Some(cspace);
        self.started[index].keeper = Some(keeper);
        let at = |slot| SlotRef::new(cspace, slot);

        let space = SpaceObject::new().ok_or(error(Kernel::NoMemory))?;
        self.install(
            at(startup::SLOT_SPACE),
            ObjectKind::AddressSpace,
            Object::Space(space.clone()),
        )
        .map_err(|err| failed("making its address space", err))?;
        caps::create_memory(at(startup::SLOT_MEMORY), MEMORY_ORDER)
            .map_err(|err| failed("making its memory", cap_error(err)))?;

        // Frames from its memory, kept in the hidden space: the stack, the
        // image and the start-up page, largest first so that none wastes
        // memory on alignment.
        let stack_pages = startup::STACK_SIZE / PAGE_SIZE;
        let image_order = image.pages().next_power_of_two().trailing_zeros();
        let mut frames = [(stack_pages.trailing_zeros(), 0), (image_order, 1), (0, 2)];
        frames.sort_unstable_by_key(|frame| core::cmp::Reverse(frame.0));
        for (order, slot) in frames {
            retype(
                at(startup::SLOT_MEMORY),
                ObjectKind::Frame,
                order,
                SlotRef::new(keeper, slot),
                1,
            )
            .map_err(|err| failed("making its frames", err))?;
        }
        let frame = |slot| objects::frame(keeper, slot, Rights::NONE).map(|(_, frame)| frame);
        let (stack, code, page) = (frame(0)?, frame(1)?, frame(2)?);

        load(&image, bytes, &code, &space).map_err(|err| failed("loading its image", err))?;
        FrameObject::map_into(&stack, &space, top, 0, stack_pages, UserAccess::ReadWrite)
            .map_err(|err| failed("mapping its stack", err))?;
        FrameObject::map_into(&page, &space, startup::STARTUP_ADDRESS, 0, 1, UserAccess::Read)
            .map_err(|err| failed("mapping its start-up page", err))?;

        self.install(at(startup::SLOT_RANDOM), ObjectKind::Random, Object::Random)?;
        self.install(at(startup::SLOT_LOG), ObjectKind::KernelLog, Object::KernelLog)?;
        for (i, &(slot, rights, badge)) in spec.extra.iter().enumerate() {
            let how = Derivation { rights, badge: (badge != 0).then_some(badge), range: None };
            let to = at(startup::SLOT_FIRST_EXTRA + i as u32);
            caps::with(|store| store.derive(SlotRef::new(self.launcher, slot), to, how))
                .and_then(|done| done)
                .map_err(|err| failed("passing it the group's objects", cap_error(err)))?;
        }

        // Its thread: it waits in the kernel until everything is ready.
        let user = UserThread {
            cspace,
            space: space.clone(),
            start: [image.entry, startup::STACK_TOP, startup::STARTUP_ADDRESS],
            ready: AtomicBool::new(false),
            ipc: SpinLock::new(ipc::IpcState::Idle),
            reply: SpinLock::new(None),
            window: AtomicU64::new(default_window()),
            exit: AtomicU64::new(0),
            exited: AtomicBool::new(false),
            program,
        };
        let user: UserRef =
            Arc::try_new_in(user, kmalloc()).map_err(|_| error(Kernel::NoMemory))?;
        let options = Options::new().pinned(spec.cpu).user(user.clone());
        let thread = sched::spawn(spec.name, thread_main, 0, options)
            .map_err(|_| failed("starting its thread", error(Kernel::NoMemory)))?;
        self.started[index].user = Some(user.clone());
        self.started[index].thread = Some(thread.clone());
        let installed = self.install(
            at(startup::SLOT_THREAD),
            ObjectKind::Thread,
            Object::Thread(thread.clone()),
        );
        if let Err(err) = installed {
            thread.request_stop();
            set_ready(&user);
            return Err(failed("giving it its thread", err));
        }

        let used = caps::with(|store| {
            let cap = store.get(at(startup::SLOT_MEMORY)).ok()?;
            Some(store.object(cap.id()?)?.watermark)
        })
        .ok()
        .flatten()
        .unwrap_or(0);
        let mut bytes = [0u8; Startup::SIZE];
        let mut put = |offset: usize, value: &[u8]| {
            bytes[offset..offset + value.len()].copy_from_slice(value);
        };
        put(Startup::OFFSET_VERSION, &startup::STARTUP_VERSION.to_le_bytes());
        put(Startup::OFFSET_CPU, &(spec.cpu as u32).to_le_bytes());
        put(Startup::OFFSET_ROLE, &u64::from(spec.role).to_le_bytes());
        put(Startup::OFFSET_ARGUMENT, &spec.argument.to_le_bytes());
        put(Startup::OFFSET_TSC_HZ, &Arch::cycle_counter_hz().to_le_bytes());
        put(Startup::OFFSET_EXTRA, &(spec.extra.len() as u32).to_le_bytes());
        put(Startup::OFFSET_THREAD_ID, &(thread.id() as u32).to_le_bytes());
        let mut kinds = [0u8; EXTRA_MAX];
        for (kind, &(slot, ..)) in kinds.iter_mut().zip(spec.extra) {
            let cap = caps::with(|store| store.get(SlotRef::new(self.launcher, slot)));
            *kind = cap.ok().and_then(|cap| cap.ok()).map_or(0, |cap| cap.kind() as u8);
        }
        put(Startup::OFFSET_SLOTS, &kinds);
        let free = ((1u64 << MEMORY_ORDER) - used) * PAGE_SIZE;
        put(Startup::OFFSET_MEMORY_BYTES, &free.to_le_bytes());
        Arch::write_frames(page.phys(), &bytes);

        set_ready(&user);
        Ok(index)
    }

    /// Makes an object of `kind` with its original capability in `at`.
    fn install(&self, at: SlotRef, kind: ObjectKind, object: Object) -> Result<()> {
        caps::with(|store| -> Result<()> {
            objects::reserve(store, 1).map_err(|_| error(Kernel::NoMemory))?;
            let id = store.create(at, kind).map_err(cap_error)?;
            objects::insert(store, id, object).map_err(|_| error(Kernel::NoMemory))
        })
        .map_err(cap_error)
        .and_then(|made| made)
    }

    /// Waits until every program has exited, or `timeout_ns` has passed.
    /// Returns whether they all exited.
    pub fn wait(&self, timeout_ns: u64) -> bool {
        let deadline = sched::now_ns().saturating_add(timeout_ns);
        self.started.iter().all(|started| {
            let program = &started.program;
            started.thread.is_none()
                || wait::wait_until_or_timeout(program.key(), "program", deadline, || {
                    program.exited.load(Ordering::Acquire)
                })
        })
    }

    /// Program `index`'s exit status, once it has exited.
    pub fn status(&self, index: usize) -> Option<u64> {
        self.started.get(index)?.user.as_ref()?.exit_status()
    }

    /// Program `index`'s name.
    pub fn name(&self, index: usize) -> &str {
        self.started.get(index).map_or("?", |started| started.name.as_str())
    }

    /// How many programs were started.
    pub fn len(&self) -> usize {
        self.started.len()
    }

    /// Stops every program still running, and waits for them.
    pub fn stop(&self) {
        for thread in self.started.iter().filter_map(|started| started.thread.as_ref()) {
            thread.request_stop();
            sched::wake(thread);
        }
        if !self.wait(2_000_000_000) {
            warn!("User: a program did not stop within 2 s");
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.stop();
        let spaces = self.started.iter().flat_map(|started| [started.cspace, started.keeper]);
        for space in spaces.flatten().chain([self.launcher]) {
            if let Err(err) = caps::destroy_space(space) {
                warn!("User: could not destroy a program's capability space: {err}");
            }
        }
    }
}

fn set_ready(user: &UserThread) {
    user.ready.store(true, Ordering::Release);
    wait::wake_all(user as *const UserThread as usize);
}

/// Copies the image's segments into `code`, one after another, and maps
/// them into `space`.
fn load(image: &radix_elf::Image, bytes: &[u8], code: &FrameRef, space: &SpaceRef) -> Result<()> {
    let mut page = 0;
    for segment in image.segments() {
        let file = &bytes[segment.offset as usize..][..segment.filesz as usize];
        let start = code.phys() + page * PAGE_SIZE + segment.vaddr % PAGE_SIZE;
        Arch::write_frames(start, file);
        let access = match segment.access {
            Access::Read => UserAccess::Read,
            Access::ReadWrite => UserAccess::ReadWrite,
            Access::ReadExecute => UserAccess::ReadExecute,
        };
        FrameObject::map_into(code, space, segment.first_page(), page, segment.pages(), access)?;
        page += segment.pages();
    }
    Ok(())
}
