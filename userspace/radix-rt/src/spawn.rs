// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Starting programs: a new process, built as radix-startup(2) says, and
//! waiting for it to end.
//!
//! [`spawn`] makes the process with the caller's own process capability
//! (`process.create`, so it is charged to the caller's domain) and gives
//! it:
//!
//! - a memory object of its own, made from a memory object of the
//!   caller's, from which its image, stack and start-up page are made too;
//! - its image, at a random place in the image window if it is
//!   position-independent, relocated; a stack whose top is a random page
//!   below `STACK_TOP`; the start-up page with its arguments;
//! - its thread, address space, memory, process, random number generator
//!   and kernel log in the slots radix-startup(2) lists, and the
//!   capabilities the caller shares from `SLOT_FIRST_EXTRA` on.
//!
//! The image, stack and start-up page are written through mappings in the
//! caller's own address space, mapped into the child's, and their frames
//! kept with the child's process (`process.keep`), out of its reach. A
//! notification the caller holds is signalled when the child exits
//! ([`Child::wait`]); deleting the caller's capabilities to it then reaps
//! it ([`Child::reap`]), and everything it had goes back: its memory
//! object, made from the caller's, leaves nothing behind in it.

use core::fmt;

use libradix::SYS;
use radix_abi::address_space::{PAGE_SIZE, USER_START};
use radix_abi::capability::{self, ObjectKind, Rights};
use radix_abi::errors::Kernel;
use radix_abi::notification::WaitFlags;
use radix_abi::process::{self, NAME_MAX, ProcessState};
use radix_abi::startup::{self, Startup};
use radix_abi::{Status, memory, random, syscalls};
use radix_elf::{Access, Kind, Placed};

use crate::{slots, vm};

/// What to start.
pub struct Spawn<'a> {
    /// The path it was found at: its first argument, and its name.
    pub path: &'a str,
    /// Its image, an ELF64 executable.
    pub image: &'a [u8],
    /// Its arguments after the path.
    pub args: &'a [&'a str],
    /// The memory object of the caller's its memory is made from.
    pub pool: u32,
    /// Its memory object's size: 2^order pages, which hold its image,
    /// stack and start-up page too.
    pub memory_order: u32,
    /// Capabilities it gets, from `SLOT_FIRST_EXTRA` on: the caller's
    /// slot, the rights to give, and a badge (0 for none).
    pub share: &'a [(u32, Rights, u64)],
}

/// Why a program could not be started.
#[derive(Clone, Copy, Debug)]
pub enum Error {
    /// The image is not one Radix runs.
    Image(radix_elf::ElfError),
    /// The arguments do not fit in the start-up page.
    TooLong,
    /// Too many capabilities to share.
    TooMany,
    /// The caller has no free slot left.
    NoSlot,
    /// The caller's address space has no room left.
    NoRoom,
    /// The kernel refused a step.
    Kernel(&'static str, Status),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Image(err) => write!(f, "the image cannot run: {err}"),
            Error::TooLong => f.write_str("the arguments do not fit in the start-up page"),
            Error::TooMany => f.write_str("too many capabilities to share"),
            Error::NoSlot => f.write_str("no free capability slot"),
            Error::NoRoom => f.write_str("no room left in the address space"),
            Error::Kernel(what, status) => match Kernel::from_status(*status) {
                Some(kind) => write!(f, "{what} failed: {}", kind.name()),
                None => write!(f, "{what} failed: {:#x}", status.0),
            },
        }
    }
}

/// A step that may fail in the kernel, named for the error.
fn step(what: &'static str, result: Result<(), Status>) -> Result<(), Error> {
    result.map_err(|status| Error::Kernel(what, status))
}

/// A program that was started.
pub struct Child {
    process: u32,
    exited: u32,
    number: u64,
}

/// The most capabilities a child can be given.
pub const SHARE_MAX: usize = 16;

/// The slots a spawn takes, given back if it fails.
struct Taken {
    slots: [u32; 8],
    count: usize,
}

impl Taken {
    fn take(&mut self) -> Result<u32, Error> {
        let slot = slots::take().ok_or(Error::NoSlot)?;
        self.slots[self.count] = slot;
        self.count += 1;
        Ok(slot)
    }

    /// Lets go of every slot it took but `keep`, deleting what they hold.
    fn release(&mut self, keep: &[u32]) {
        for &slot in &self.slots[..self.count] {
            if !keep.contains(&slot) {
                let _ = capability::delete(&SYS, slot);
                slots::give_back(slot);
            }
        }
        self.count = 0;
    }
}

/// Starts the program `spawn` describes.
pub fn spawn(spawn: &Spawn<'_>) -> Result<Child, Error> {
    if spawn.share.len() > SHARE_MAX {
        return Err(Error::TooMany);
    }
    let image = radix_elf::read(spawn.image).map_err(Error::Image)?;
    let mut taken = Taken { slots: [0; 8], count: 0 };
    let process = taken.take()?;
    let made = step(
        "making its process",
        process::create(&SYS, startup::SLOT_PROCESS, ObjectKind::Process, startup::SLOTS, process),
    );
    let built = made.and_then(|()| build(spawn, &image, process, &mut taken));
    match built {
        Ok(child) => {
            taken.release(&[child.process, child.exited]);
            Ok(child)
        }
        Err(err) => {
            let _ = process::exit(&SYS, process, process::EXIT_STOPPED);
            taken.release(&[]);
            Err(err)
        }
    }
}

/// Builds and starts the child, whose capability is in `process`.
fn build(
    spawn: &Spawn<'_>,
    image: &radix_elf::Image,
    process: u32,
    taken: &mut Taken,
) -> Result<Child, Error> {
    let name = file_name(spawn.path);
    step("naming it", process::set_name(&SYS, process, name.as_bytes()))?;
    let space = taken.take()?;
    step("reaching its address space", process::address_space(&SYS, process, space))?;

    let random = random::words(&SYS, startup::SLOT_RANDOM).map_or([0; 8], |r| r.words);
    let (range, stack_top) = match image.kind {
        Kind::Fixed => {
            (USER_START + PAGE_SIZE..startup::STACK_TOP - startup::STACK_SIZE, startup::STACK_TOP)
        }
        Kind::Relocatable => {
            let pages = startup::STACK_WINDOW / PAGE_SIZE;
            let top = startup::STACK_TOP - (random[1] % (pages + 1)) * PAGE_SIZE;
            (startup::IMAGE_BASE..startup::IMAGE_BASE + startup::WINDOW, top)
        }
    };
    let placed = image
        .random_bias(range.clone(), random[0])
        .and_then(|bias| image.place(bias, range))
        .map_err(Error::Image)?;

    // Its memory, and from it the frames of its stack, image and start-up
    // page, largest first so that none wastes memory on alignment.
    let memory = taken.take()?;
    step(
        "making its memory",
        memory::retype(&SYS, spawn.pool, ObjectKind::Memory, spawn.memory_order, memory, 1),
    )?;
    let stack_pages = startup::STACK_SIZE / PAGE_SIZE;
    let image_pages = image.pages().next_power_of_two();
    let (stack, code, page) = (taken.take()?, taken.take()?, taken.take()?);
    let mut frames = [(stack_pages, stack), (image_pages, code), (1, page)];
    frames.sort_unstable_by_key(|frame| core::cmp::Reverse(frame.0));
    for (pages, slot) in frames {
        let order = pages.trailing_zeros();
        step("making its frames", memory::retype(&SYS, memory, ObjectKind::Frame, order, slot, 1))?;
    }

    load(&placed, spawn.image, code, image_pages)?;
    for mapping in placed.mappings().into_iter().flatten() {
        let rights = match mapping.access {
            Access::Read => Rights::READ,
            Access::ReadWrite => Rights::READ.union(Rights::WRITE),
            Access::ReadExecute => Rights::READ.union(Rights::EXECUTE),
        };
        let mapped =
            syscalls::map(&SYS, space, code, mapping.address, rights, mapping.page, mapping.pages);
        step("mapping its image", mapped)?;
    }
    let rw = Rights::READ.union(Rights::WRITE);
    let stack_bottom = stack_top - startup::STACK_SIZE;
    step("mapping its stack", syscalls::map(&SYS, space, stack, stack_bottom, rw, 0, stack_pages))?;

    // Its capabilities, but its thread.
    let all = Rights::READ.union(Rights::CONTROL).union(Rights::COPY).union(Rights::TRANSFER);
    let passed = Rights::COPY.union(Rights::TRANSFER);
    let gives = [
        (space, startup::SLOT_SPACE, all),
        (startup::SLOT_RANDOM, startup::SLOT_RANDOM, Rights::READ.union(passed)),
        (startup::SLOT_LOG, startup::SLOT_LOG, Rights::WRITE.union(passed)),
        (process, startup::SLOT_PROCESS, Rights::READ.union(Rights::CONTROL)),
    ];
    for (from, to, rights) in gives {
        step("giving it its capabilities", process::share(&SYS, process, from, to, rights, 0))?;
    }
    let mut kinds = [0u8; SHARE_MAX];
    for (i, &(from, rights, badge)) in spawn.share.iter().enumerate() {
        let to = startup::SLOT_FIRST_EXTRA + i as u32;
        step("sharing capabilities", process::share(&SYS, process, from, to, rights, badge))?;
        let info = capability::info(&SYS, from).map_err(|s| Error::Kernel("sharing", s))?;
        kinds[i] = info.kind.raw();
    }

    // The start-up page, once its memory is known.
    let used = memory::info(&SYS, memory).map_err(|s| Error::Kernel("reading its memory", s))?;
    let free = (used.pages - used.used) * PAGE_SIZE;
    let fields = Fields { spawn, placed: &placed, kinds, free };
    write_page(page, &fields)?;
    let ro = Rights::READ;
    step(
        "mapping its start-up page",
        syscalls::map(&SYS, space, page, startup::STARTUP_ADDRESS, ro, 0, 1),
    )?;
    step("giving it its memory", process::give(&SYS, process, memory, startup::SLOT_MEMORY))?;
    for frame in [stack, code, page] {
        step("keeping its frames", process::keep(&SYS, process, frame))?;
    }

    // Told when it exits, then off it goes.
    let exited = taken.take()?;
    let made = process::create(&SYS, startup::SLOT_PROCESS, ObjectKind::Notification, 0, exited);
    step("making its exit notification", made)?;
    step("asking to hear of its end", process::notify_exit(&SYS, process, exited))?;
    let entry = placed.entry();
    let started = process::start(
        &SYS,
        process,
        entry,
        stack_top,
        startup::STARTUP_ADDRESS,
        startup::SLOT_THREAD,
    );
    started.map_err(|status| Error::Kernel("starting its thread", status))?;
    let number = process::info(&SYS, process).map_or(0, |info| info.number);
    Ok(Child { process, exited, number })
}

/// The last name of `path`, cut to a process name's length.
fn file_name(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut len = name.len().min(NAME_MAX as usize);
    while !name.is_char_boundary(len) {
        len -= 1;
    }
    &name[..len]
}

/// Maps the frame in `frame`, `pages` pages, read-write into the caller's
/// own address space for `f` to fill, then unmaps it.
fn fill(frame: u32, pages: u64, f: impl FnOnce(&mut [u8])) -> Result<(), Error> {
    let address = vm::take(pages, PAGE_SIZE).ok_or(Error::NoRoom)?;
    let rw = Rights::READ.union(Rights::WRITE);
    step(
        "mapping a frame to fill",
        syscalls::map(&SYS, startup::SLOT_SPACE, frame, address, rw, 0, pages),
    )?;
    // SAFETY: the frame was just mapped read-write at `address`, `pages`
    // pages long, at addresses nothing else uses (`vm::take`), and stays
    // mapped until after `f` returns.
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(address as *mut u8, (pages * PAGE_SIZE) as usize)
    };
    f(bytes);
    step("unmapping a frame", syscalls::unmap(&SYS, startup::SLOT_SPACE, address, pages))
}

/// Copies the image into the frame `code` of `pages` pages, relocated.
fn load(placed: &Placed<'_>, image: &[u8], code: u32, pages: u64) -> Result<(), Error> {
    let mut loaded = Ok(());
    fill(code, pages, |bytes| {
        loaded = placed.load(image, &mut |offset, data| {
            // `Placed::load` stays inside the image's pages.
            if let Some(to) = bytes.get_mut(offset as usize..offset as usize + data.len()) {
                to.copy_from_slice(data);
            }
        });
    })?;
    loaded.map_err(Error::Image)
}

/// What goes in a child's start-up page.
struct Fields<'a> {
    spawn: &'a Spawn<'a>,
    placed: &'a Placed<'a>,
    kinds: [u8; SHARE_MAX],
    free: u64,
}

/// Writes the start-up page into the frame `page`.
fn write_page(page: u32, fields: &Fields<'_>) -> Result<(), Error> {
    let spawn = fields.spawn;
    let mut fits = true;
    fill(page, 1, |bytes| {
        let mut put = |offset: usize, value: &[u8]| {
            bytes[offset..offset + value.len()].copy_from_slice(value);
        };
        put(Startup::OFFSET_VERSION, &startup::STARTUP_VERSION.to_le_bytes());
        put(Startup::OFFSET_CPU, &startup::ANY_CPU.to_le_bytes());
        let hz = libradix::startup().tsc_hz;
        put(Startup::OFFSET_TSC_HZ, &hz.to_le_bytes());
        put(Startup::OFFSET_EXTRA, &(spawn.share.len() as u32).to_le_bytes());
        put(Startup::OFFSET_SLOTS, &fields.kinds);
        put(Startup::OFFSET_MEMORY_BYTES, &fields.free.to_le_bytes());
        put(Startup::OFFSET_IMAGE_BIAS, &fields.placed.bias().to_le_bytes());
        // The arguments, each ending with a NUL, after the fields.
        let mut at = Startup::SIZE;
        for arg in core::iter::once(&spawn.path).chain(spawn.args) {
            let end = at + arg.len() + 1;
            if arg.contains('\0') || end > PAGE_SIZE as usize {
                fits = false;
                return;
            }
            put(at, arg.as_bytes());
            at = end;
        }
        put(Startup::OFFSET_ARGS_OFFSET, &(Startup::SIZE as u32).to_le_bytes());
        put(Startup::OFFSET_ARGS_SIZE, &((at - Startup::SIZE) as u32).to_le_bytes());
    })?;
    if fits { Ok(()) } else { Err(Error::TooLong) }
}

impl Child {
    /// Its process number.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// Waits until it has exited, and returns its exit code.
    pub fn wait(&self) -> Result<u64, Error> {
        loop {
            let info = process::info(&SYS, self.process).map_err(|s| Error::Kernel("info", s))?;
            if info.state == ProcessState::Exited {
                return Ok(info.code);
            }
            let waited = syscalls::wait(&SYS, self.exited, WaitFlags::NONE, 0);
            waited.map_err(|status| Error::Kernel("waiting for it", status))?;
        }
    }

    /// Reaps it: lets go of the caller's capabilities to it. Once it has
    /// exited, nothing of it is left.
    pub fn reap(self) {
        for slot in [self.process, self.exited] {
            let _ = capability::delete(&SYS, slot);
            slots::give_back(slot);
        }
    }
}
