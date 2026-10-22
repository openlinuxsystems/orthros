// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Loading programs in the kernel: Spark (`init`) and the self-tests' test
//! programs (`launch`). Every other program is loaded by radix-rt in user
//! space, the same way (radix-startup(2)).
//!
//! [`start`] makes a process and gives it what radix-startup(2) promises:
//!
//! - its thread, address space, a memory object to make frames from, the
//!   random number generator, the kernel log and its own process (read
//!   and control rights) in the slots radix-startup(2) lists, and from
//!   `SLOT_FIRST_EXTRA` on the capabilities its starter passes it;
//! - its image's segments, placed at random in the image window when the
//!   program is position-independent; a stack whose top is a random page
//!   below `STACK_TOP` for such a program, at `STACK_TOP` for a fixed one;
//!   and the read-only start-up page at `STARTUP_ADDRESS`, with its
//!   arguments;
//! - for Spark, the boot archive mapped read-only at a random place in the
//!   loader's window;
//! - one thread, at the image's entry point with the start-up page's
//!   address as its argument.
//!
//! The frames for the image, the stack, the start-up page and the boot
//! archive are kept in the process's hidden capability space, so the
//! program cannot pull its own code from under itself by accident. The
//! image, stack and start-up page come from the program's own memory
//! object, which says how much of it they took; the archive's from memory
//! of its own.

use radix_abi::Status;
use radix_abi::address_space::{PAGE_SIZE, USER_START};
use radix_abi::errors::Kernel;
use radix_abi::startup::{self, Startup};
use radix_cap::{Derivation, ObjectKind, Rights, SlotRef};
use radix_elf::{Access, Kind};
use radix_hal::{UserAccess, UserMode};
use radix_klog::warn;
use radix_mm::MAX_ORDER;

use super::account::Account;
use super::memory::{FrameObject, FrameRef, SpaceRef};
use super::methods::retype;
use super::objects::{self, Object};
use super::process::{self, ProcessObject, ProcessRef};
use super::{Result, cap_error, error};
use crate::domains::DomainRef;
use crate::{Arch, caps};

/// The most capabilities a program gets from its starter: the start-up
/// page says what each is.
pub const EXTRA_MAX: usize = 16;

/// The hidden slots of the image, stack and start-up page frames; the boot
/// archive's memory and frames follow.
const KEPT_STACK: u32 = 1;
const KEPT_IMAGE: u32 = 2;
const KEPT_PAGE: u32 = 3;
const KEPT_ARCHIVE: u32 = 4;

/// The largest frame the kernel makes: the boot archive is copied into as
/// many as it takes.
const ARCHIVE_CHUNK: u64 = PAGE_SIZE << MAX_ORDER;

/// A program to start.
pub struct Program<'a> {
    /// Its name, which its process and thread take.
    pub name: &'a str,
    /// Its image, an ELF64 executable.
    pub image: &'a [u8],
    /// The CPU its thread is pinned to, if it is.
    pub cpu: Option<usize>,
    /// Its memory object's size: 2^order pages.
    pub memory_order: u32,
    /// What it is started to do, for test programs with many roles.
    pub role: u64,
    /// A word for it.
    pub argument: u64,
    /// Its arguments, the first its path; they must fit in the start-up
    /// page.
    pub args: &'a [&'a str],
    /// The capabilities it gets, from `SLOT_FIRST_EXTRA` on: where each
    /// is, the rights to keep, and a badge (0 for none).
    pub extra: &'a [(SlotRef, Rights, u64)],
    /// The boot archive, for Spark.
    pub archive: Option<&'a [u8]>,
    /// The domain its threads run in and its objects are charged to.
    pub domain: DomainRef,
    /// Called once it has exited.
    pub on_exit: Option<fn(&ProcessObject, u64)>,
}

/// Why starting failed, logged.
fn failed(name: &str, what: &str, status: Status) -> Status {
    warn!("User: could not start {name}: {what} failed with {:#x}", status.0);
    status
}

/// Starts `program` in a new process whose original capability goes in
/// `at`. On a failure, the process made so far is ended.
pub fn start(at: SlotRef, program: &Program<'_>) -> Result<ProcessRef> {
    let name = program.name;
    if program.extra.len() > EXTRA_MAX {
        return Err(failed(name, "checking its capabilities", error(Kernel::InvalidArgument)));
    }
    let account = Account::new(program.domain.clone()).ok_or(error(Kernel::NoMemory))?;
    let process = process::create(at, name, startup::SLOTS, account, None, None, program.on_exit)
        .map_err(|err| failed(name, "making its process", err))?;
    match build(at, &process, program) {
        Ok(()) => Ok(process),
        Err(err) => {
            process.exit(radix_abi::process::EXIT_STOPPED);
            Err(err)
        }
    }
}

/// Fills `process`, whose original capability is in `at`, and starts it.
fn build(at: SlotRef, process: &ProcessRef, program: &Program<'_>) -> Result<()> {
    let name = program.name;
    let cspace = process.cspace();
    let keep = process.keep_space();
    let mine = |slot| SlotRef::new(cspace, slot);
    let kept = |slot| SlotRef::new(keep, slot);
    let space = process.space().ok_or(error(Kernel::Gone))?;

    let image = radix_elf::read(program.image).map_err(|err| {
        warn!("User: {name} cannot run: {err}");
        error(Kernel::InvalidArgument)
    })?;
    // A fixed program runs where it was linked, below its stack; a
    // position-independent one at a random place in the image window.
    let (range, stack_top) = match image.kind {
        Kind::Fixed => {
            (USER_START + PAGE_SIZE..startup::STACK_TOP - startup::STACK_SIZE, startup::STACK_TOP)
        }
        Kind::Relocatable => {
            let pages = startup::STACK_WINDOW / PAGE_SIZE;
            let top = startup::STACK_TOP - radix_random::below(pages + 1) * PAGE_SIZE;
            (startup::IMAGE_BASE..startup::IMAGE_BASE + startup::WINDOW, top)
        }
    };
    let placed = image
        .random_bias(range.clone(), radix_random::u64())
        .and_then(|bias| image.place(bias, range))
        .map_err(|err| {
            warn!("User: {name} does not fit: {err}");
            error(Kernel::InvalidArgument)
        })?;

    // The capabilities radix-startup(2) lists, but its thread.
    let derive = |from: SlotRef, to: SlotRef, rights: Rights, badge: u64| {
        let how = Derivation { rights, badge: (badge != 0).then_some(badge), range: None };
        caps::with(|store| store.derive(from, to, how)).and_then(|done| done).map_err(cap_error)
    };
    let all = |kind: ObjectKind| kind.rights();
    derive(kept(0), mine(startup::SLOT_SPACE), all(ObjectKind::AddressSpace), 0)
        .map_err(|err| failed(name, "giving it its address space", err))?;
    derive(at, mine(startup::SLOT_PROCESS), Rights::READ.union(Rights::CONTROL), 0)
        .map_err(|err| failed(name, "giving it its process", err))?;
    caps::create_memory(mine(startup::SLOT_MEMORY), program.memory_order)
        .map_err(|err| failed(name, "making its memory", cap_error(err)))?;
    process::install(mine(startup::SLOT_RANDOM), ObjectKind::Random, Object::Random)?;
    process::install(mine(startup::SLOT_LOG), ObjectKind::KernelLog, Object::KernelLog)?;
    let mut kinds = [0u8; EXTRA_MAX];
    for (i, &(from, rights, badge)) in program.extra.iter().enumerate() {
        derive(from, mine(startup::SLOT_FIRST_EXTRA + i as u32), rights, badge)
            .map_err(|err| failed(name, "passing it capabilities", err))?;
        let cap = caps::get(from).map_err(cap_error)?;
        kinds[i] = cap.kind() as u8;
    }

    // Frames from its memory, kept hidden: the stack, the image and the
    // start-up page, largest first so that none wastes memory on
    // alignment.
    let stack_pages = startup::STACK_SIZE / PAGE_SIZE;
    let image_order = image.pages().next_power_of_two().trailing_zeros();
    let mut frames =
        [(stack_pages.trailing_zeros(), KEPT_STACK), (image_order, KEPT_IMAGE), (0, KEPT_PAGE)];
    frames.sort_unstable_by_key(|frame| core::cmp::Reverse(frame.0));
    for (order, slot) in frames {
        retype(mine(startup::SLOT_MEMORY), ObjectKind::Frame, order, kept(slot), 1)
            .map_err(|err| failed(name, "making its frames", err))?;
    }
    let frame = |slot| objects::frame(keep, slot, Rights::NONE).map(|(_, frame)| frame);
    let (stack, code, page) = (frame(KEPT_STACK)?, frame(KEPT_IMAGE)?, frame(KEPT_PAGE)?);

    load(&placed, program.image, &code, &space)
        .map_err(|err| failed(name, "loading its image", err))?;
    let stack_bottom = stack_top - startup::STACK_SIZE;
    FrameObject::map_into(&stack, &space, stack_bottom, 0, stack_pages, UserAccess::ReadWrite)
        .map_err(|err| failed(name, "mapping its stack", err))?;
    FrameObject::map_into(&page, &space, startup::STARTUP_ADDRESS, 0, 1, UserAccess::Read)
        .map_err(|err| failed(name, "mapping its start-up page", err))?;
    let archive = match program.archive {
        Some(archive) => Some(map_archive(process, &space, archive)?),
        None => None,
    };

    let used = caps::with(|store| {
        let cap = store.get(mine(startup::SLOT_MEMORY)).ok()?;
        Some(store.object(cap.id()?)?.watermark)
    })
    .ok()
    .flatten()
    .unwrap_or(0);
    let mut bytes = [0u8; PAGE_SIZE as usize];
    let mut put = |offset: usize, value: &[u8]| {
        bytes[offset..offset + value.len()].copy_from_slice(value);
    };
    put(Startup::OFFSET_VERSION, &startup::STARTUP_VERSION.to_le_bytes());
    let cpu = program.cpu.map_or(startup::ANY_CPU, |cpu| cpu as u32);
    put(Startup::OFFSET_CPU, &cpu.to_le_bytes());
    put(Startup::OFFSET_ROLE, &program.role.to_le_bytes());
    put(Startup::OFFSET_ARGUMENT, &program.argument.to_le_bytes());
    put(Startup::OFFSET_TSC_HZ, &Arch::cycle_counter_hz().to_le_bytes());
    put(Startup::OFFSET_EXTRA, &(program.extra.len() as u32).to_le_bytes());
    put(Startup::OFFSET_SLOTS, &kinds);
    let free = ((1u64 << program.memory_order) - used) * PAGE_SIZE;
    put(Startup::OFFSET_MEMORY_BYTES, &free.to_le_bytes());
    put(Startup::OFFSET_IMAGE_BIAS, &placed.bias().to_le_bytes());
    let (archive_at, archive_size) = archive.unwrap_or((0, 0));
    put(Startup::OFFSET_BOOTFS_ADDRESS, &archive_at.to_le_bytes());
    put(Startup::OFFSET_BOOTFS_SIZE, &archive_size.to_le_bytes());
    // The arguments, each ending with a NUL, after the fields.
    let mut at_byte = Startup::SIZE;
    for arg in program.args {
        let end = at_byte + arg.len() + 1;
        if arg.contains('\0') || end > PAGE_SIZE as usize {
            return Err(failed(name, "passing it its arguments", error(Kernel::InvalidArgument)));
        }
        put(at_byte, arg.as_bytes());
        at_byte = end;
    }
    put(Startup::OFFSET_ARGS_OFFSET, &(Startup::SIZE as u32).to_le_bytes());
    put(Startup::OFFSET_ARGS_SIZE, &((at_byte - Startup::SIZE) as u32).to_le_bytes());

    let start = [placed.entry(), stack_top, startup::STARTUP_ADDRESS];
    ProcessObject::start(process, start, startup::SLOT_THREAD, program.cpu, &mut |thread| {
        bytes[Startup::OFFSET_THREAD_ID..][..4]
            .copy_from_slice(&(thread.id() as u32).to_le_bytes());
        page.write(0, &bytes)
    })
    .map_err(|err| failed(name, "starting its thread", err))?;
    Ok(())
}

/// Copies the image into `code`, one segment after another, relocated, and
/// maps it into `space`.
fn load(
    placed: &radix_elf::Placed<'_>,
    bytes: &[u8],
    code: &FrameRef,
    space: &SpaceRef,
) -> Result<()> {
    let mut written = Ok(());
    placed
        .load(bytes, &mut |offset, data| {
            if written.is_ok() {
                written = code.write(offset, data);
            }
        })
        .map_err(|err| {
            warn!("User: an image cannot be loaded: {err}");
            error(Kernel::InvalidArgument)
        })?;
    written?;
    for mapping in placed.mappings().into_iter().flatten() {
        let access = match mapping.access {
            Access::Read => UserAccess::Read,
            Access::ReadWrite => UserAccess::ReadWrite,
            Access::ReadExecute => UserAccess::ReadExecute,
        };
        FrameObject::map_into(code, space, mapping.address, mapping.page, mapping.pages, access)?;
    }
    Ok(())
}

/// Copies the boot archive into frames kept for `process` and maps them
/// read-only at a random place in the loader's window. Returns where, and
/// the archive's size.
fn map_archive(process: &ProcessObject, space: &SpaceRef, archive: &[u8]) -> Result<(u64, u64)> {
    let size = archive.len() as u64;
    let pages = size.div_ceil(PAGE_SIZE).max(1);
    let chunks = (pages * PAGE_SIZE).div_ceil(ARCHIVE_CHUNK);
    // A memory object and its frame for each chunk.
    if KEPT_ARCHIVE as u64 + 2 * chunks > u64::from(radix_abi::process::KEEP_SLOTS) + 1 {
        warn!("User: the boot archive is too large: {} KiB", size / 1024);
        return Err(error(Kernel::NoRoom));
    }
    let room = startup::WINDOW / PAGE_SIZE - pages;
    let base = startup::LOADER_BASE + radix_random::below(room + 1) * PAGE_SIZE;
    let keep = process.keep_space();
    for chunk in 0..chunks {
        let first = chunk * ARCHIVE_CHUNK / PAGE_SIZE;
        let count = (pages - first).min(ARCHIVE_CHUNK / PAGE_SIZE);
        let order = count.next_power_of_two().trailing_zeros();
        let memory = SlotRef::new(keep, KEPT_ARCHIVE + 2 * chunk as u32);
        let to = SlotRef::new(keep, memory.slot + 1);
        caps::create_memory(memory, order).map_err(cap_error)?;
        retype(memory, ObjectKind::Frame, order, to, 1)?;
        let (_, frame) = objects::frame(keep, to.slot, Rights::NONE)?;
        let start = (first * PAGE_SIZE) as usize;
        let end = (start + (count * PAGE_SIZE) as usize).min(archive.len());
        frame.write(0, &archive[start..end])?;
        let address = base + first * PAGE_SIZE;
        FrameObject::map_into(&frame, space, address, 0, count, UserAccess::Read)?;
    }
    Ok((base, size))
}
