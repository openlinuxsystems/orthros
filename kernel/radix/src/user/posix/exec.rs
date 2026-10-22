// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `exec` (Phase 8, radix-process(2)): runs a program from a file in a
//! process, in a new address space.
//!
//! The program is read from its file a page at a time, never whole: its
//! headers first (`radix_elf::read_program`), then each loadable segment
//! is mapped as anonymous memory filled from the file when a page is
//! first touched, so a program starts as fast whatever its size, and only
//! the pages it uses take memory. A program that names an interpreter
//! (`PT_INTERP`) gets it mapped too, at a random place in the loader's
//! window, and starts there; a script that starts with `#!` gets its
//! interpreter run with the script's path as an argument (one level: the
//! interpreter must be a program). Both are found from the process's
//! SLOT_ROOT. A position-independent program without an interpreter is
//! placed at random in the image window and its `R_X86_64_RELATIVE`
//! relocations applied here; it may apply any others itself.
//!
//! The stack is laid out as the System V ABI says (`radix_posix::exec`),
//! below a random top, with 8 MiB of room made as it is touched; the
//! start-up page (radix-startup(2)) is mapped read-only at
//! STARTUP_ADDRESS, as for a program the kernel's loader starts, and the
//! auxiliary vector points at it.
//!
//! Everything that can fail is done in the new space, which nothing runs
//! in yet, before the process is touched. Then, on the caller's own
//! process, its other threads are stopped, the new space takes the old
//! one's place (the original capability in the hidden slot 0 is made
//! again, so every capability to the old space goes), the slots with
//! close_on_exec are emptied, handled signals go back to their defaults,
//! a thread starts in the new program, and the calling thread exits. On a
//! process `create` made, which has not started, the process instead
//! joins the tree as the caller's child (`posix_spawn`'s way), and the
//! call returns.

use alloc::boxed::Box;
use alloc::vec::Vec;

use radix_abi::address_space::{ANONYMOUS_TOP, PAGE_SIZE, USER_START};
use radix_abi::capability::SlotFlags;
use radix_abi::errors::Kernel;
use radix_abi::process::{self, EXEC_MAX, EXIT_STOPPED, ProcessState};
use radix_abi::startup::{self, Startup};
use radix_abi::{Status, UserBuffer};
use radix_cap::{Derivation, ObjectKind, Rights, SlotRef};
use radix_elf::{Access, ElfError, Kind, Program};
use radix_hal::{UserAccess, UserMode};
use radix_klog::warn;
use radix_mm::Kmalloc;
use radix_posix::exec::{Block, BlockError, Stack, aux, shebang};
use radix_vfs::mount::{Location, OpenFlags};
use radix_vfs::{NodeRef, SliceMut};

use super::{kmalloc, with_tree};
use crate::fs;
use crate::sched::{self, Name, wait};
use crate::user::memory::{Anon, FileSource, Placement, SpaceObject, SpaceRef};
use crate::user::methods::{Answer, answer};
use crate::user::objects::{self, Object};
use crate::user::process::{ProcessObject, ProcessRef, Start};
use crate::user::{Result, UserRef, cap_error, error};
use crate::{Arch, caps};

/// The room a new program's stack has, made as it is touched: Linux's
/// default limit.
const STACK_BYTES: u64 = 8 << 20;

/// The most bytes of a file read to find its program headers.
const HEAD_MAX: usize = 65536;

/// The first bytes of a file looked at: enough for a `#!` line and, for
/// nearly every program, its program headers.
const HEAD: usize = 4096;

/// A buffer of `len` zero bytes from the kernel's heap.
fn zeroed(len: usize) -> Result<Vec<u8, Kmalloc>> {
    let mut bytes = Vec::new_in(kmalloc());
    bytes.try_reserve_exact(len).map_err(|_| error(Kernel::NoMemory))?;
    while bytes.len() < len {
        // Room was reserved above.
        let _ = bytes.push_within_capacity(0);
    }
    Ok(bytes)
}

/// Copies `bytes` to the kernel's heap.
fn copied(bytes: &[u8]) -> Result<Vec<u8, Kmalloc>> {
    let mut copy = zeroed(bytes.len())?;
    copy.copy_from_slice(bytes);
    Ok(copy)
}

/// What a refused program becomes.
fn not_executable(err: ElfError) -> Status {
    Kernel::NotExecutable.status(err as u32)
}

/// Reads `out.len()` bytes of `node` from `offset`, all of them.
fn read_exact(node: &NodeRef, offset: u64, out: &mut [u8]) -> Result<()> {
    let mut done = 0;
    while done < out.len() {
        let read =
            node.read(offset + done as u64, &mut SliceMut(&mut out[done..])).map_err(fs::status)?;
        if read == 0 {
            return Err(not_executable(ElfError::Truncated));
        }
        done += read;
    }
    Ok(())
}

/// A program's file: where it is, its size and its first bytes.
struct File {
    node: NodeRef,
    size: u64,
    head: Vec<u8, Kmalloc>,
}

impl File {
    /// Reads the first bytes of the file at `at`, which must be a regular
    /// file: as many as hold an ELF file's program headers, up to
    /// [`HEAD_MAX`].
    fn open(at: &Location) -> Result<File> {
        let node = at.node.clone();
        let stat = node.stat();
        match stat.kind {
            radix_vfs::Kind::File => {}
            radix_vfs::Kind::Directory => {
                return Err(radix_abi::errors::File::IsDirectory.status(0));
            }
            _ => return Err(error(Kernel::NotExecutable)),
        }
        let mut len = (stat.size as usize).min(HEAD);
        let mut head = zeroed(len)?;
        read_exact(&node, 0, &mut head)?;
        if head.starts_with(b"\x7fELF") && head.len() >= 64 {
            let phoff = u64::from_le_bytes(head[32..40].try_into().unwrap_or([0; 8]));
            let phnum = u64::from(u16::from_le_bytes([head[56], head[57]]));
            let end = phoff.saturating_add(phnum * Program::HEADER_SIZE);
            if end > len as u64 && end <= (HEAD_MAX as u64).min(stat.size) {
                len = end as usize;
                head = zeroed(len)?;
                read_exact(&node, 0, &mut head)?;
            }
        }
        Ok(File { node, size: stat.size, head })
    }

    /// Reads it as a program, kept on the heap: two of them would not fit
    /// in `exec`'s stack frame.
    #[inline(never)]
    fn program(&self) -> Result<Box<Program, Kmalloc>> {
        let program = radix_elf::read_program(&self.head, self.size).map_err(not_executable)?;
        Box::try_new_in(program, kmalloc()).map_err(|_| error(Kernel::NoMemory))
    }
}

/// The directory a process finds interpreters from: its SLOT_ROOT.
fn root_of(process: &ProcessObject) -> Result<Location> {
    let (_, root) = objects::directory(process.cspace(), startup::SLOT_ROOT, Rights::READ)
        .map_err(|_| radix_abi::errors::File::NotFound.status(0))?;
    Ok(root.at.clone())
}

/// Opens the file at `path` from `process`'s root: an interpreter's path,
/// absolute or not.
fn open_from(process: &ProcessObject, path: &[u8]) -> Result<File> {
    let mut path = path;
    while let [b'/', rest @ ..] = path {
        path = rest;
    }
    let path = radix_vfs::path::parse(path).map_err(fs::status)?;
    let root = root_of(process)?;
    let vfs = fs::vfs().ok_or(error(Kernel::Gone))?;
    let at = vfs.open(&root, path, OpenFlags::default()).map_err(fs::status)?;
    File::open(&at)
}

/// Where a program was mapped.
struct Mapped {
    entry: u64,
    bias: u64,
    headers: Option<u64>,
}

/// Maps `program`, from `file`, into `space` at a place in `window` (its
/// own place, for a fixed one).
#[inline(never)]
fn map_program(
    space: &SpaceObject,
    file: &File,
    program: &Program,
    window: core::ops::Range<u64>,
) -> Result<Mapped> {
    let image = &program.image;
    let bias = image.random_bias(window.clone(), radix_random::u64()).map_err(not_executable)?;
    let placed = image.place(bias, window).map_err(not_executable)?;
    for segment in image.segments() {
        let access = match segment.access {
            Access::Read => UserAccess::Read,
            Access::ReadWrite => UserAccess::ReadWrite,
            Access::ReadExecute => UserAccess::ReadExecute,
        };
        let page_offset = segment.offset % PAGE_SIZE;
        let source = (segment.filesz > 0).then(|| FileSource {
            node: file.node.clone(),
            base: segment.offset - page_offset,
            size: page_offset + segment.filesz,
        });
        let anon = Anon { access: Some(access), shared: false, file: source };
        let at = segment.first_page().wrapping_add(bias);
        space
            .map_anonymous(segment.pages(), anon, Placement::FixedNoReplace(at))
            .map_err(|_| not_executable(ElfError::OutOfRange))?;
    }
    Ok(Mapped {
        entry: placed.entry(),
        bias,
        headers: program.headers.map(|at| at.wrapping_add(bias)),
    })
}

/// Applies the relocations of `program`, a position-independent program
/// without an interpreter mapped into `space` moved by `bias`: those the
/// kernel can (`R_X86_64_RELATIVE`), leaving any others to the program.
#[inline(never)]
fn relocate(space: &SpaceObject, file: &File, program: &Program, bias: u64) -> Result<()> {
    let Some((offset, size)) = program.dynamic else { return Ok(()) };
    let mut table = zeroed(size.min(4096) as usize)?;
    read_exact(&file.node, offset, &mut table)?;
    let (start, count) = match program.relocations(&table) {
        Ok(found) => found,
        // Not something this loader understands: the program's own.
        Err(_) => return Ok(()),
    };
    let placed = program.image.place(bias, 0..u64::MAX).map_err(not_executable)?;
    let entry_size = radix_elf::Placed::RELOCATION_SIZE;
    let mut chunk = zeroed(4096 / entry_size as usize * entry_size as usize)?;
    let per_chunk = chunk.len() as u64 / entry_size;
    let mut done = 0;
    while done < count {
        let n = (count - done).min(per_chunk);
        let bytes = &mut chunk[..(n * entry_size) as usize];
        read_exact(&file.node, start + done * entry_size, bytes)?;
        for entry in bytes.chunks_exact(entry_size as usize) {
            if let Ok(Some((address, value))) = placed.relocation(entry) {
                space.write_new(address, &value.to_le_bytes())?;
            }
        }
        done += n;
    }
    Ok(())
}

/// Maps the start-up page at STARTUP_ADDRESS, read-only, filled in for a
/// program moved by `bias`.
#[inline(never)]
fn map_startup(space: &SpaceObject, bias: u64) -> Result<()> {
    let anon = Anon { access: Some(UserAccess::Read), shared: false, file: None };
    space
        .map_anonymous(1, anon, Placement::FixedNoReplace(startup::STARTUP_ADDRESS))
        .map_err(|_| not_executable(ElfError::OutOfRange))?;
    let mut page = [0u8; Startup::SIZE];
    let mut put =
        |offset: usize, value: &[u8]| page[offset..offset + value.len()].copy_from_slice(value);
    put(Startup::OFFSET_VERSION, &startup::STARTUP_VERSION.to_le_bytes());
    put(Startup::OFFSET_CPU, &startup::ANY_CPU.to_le_bytes());
    put(Startup::OFFSET_TSC_HZ, &Arch::cycle_counter_hz().to_le_bytes());
    put(Startup::OFFSET_IMAGE_BIAS, &bias.to_le_bytes());
    put(Startup::OFFSET_ARGS_OFFSET, &(Startup::SIZE as u32).to_le_bytes());
    space.write_new(startup::STARTUP_ADDRESS, &page)
}

/// The block `exec` was given, copied in.
fn copy_block(buffer: UserBuffer) -> Result<Vec<u8, Kmalloc>> {
    if buffer.len > EXEC_MAX {
        return Err(error(Kernel::ArgumentsTooLong));
    }
    if !radix_ipc::range::user_bytes(buffer.address, buffer.len) {
        return Err(error(Kernel::BadAddress));
    }
    let mut bytes = zeroed(buffer.len as usize)?;
    if !Arch::copy_from_user(&mut bytes, buffer.address) {
        return Err(error(Kernel::BadAddress));
    }
    Ok(bytes)
}

/// The last part of a path: what a process run from it is named.
fn base_name(path: &[u8]) -> &[u8] {
    let path = path.strip_suffix(b"/").unwrap_or(path);
    path.rsplit(|&b| b == b'/').next().unwrap_or(path)
}

/// `exec`, called by `user`'s thread as method `label` on `target` (its
/// own process, or one `create` made that has not started), whose
/// capability is in the caller's slot `slot`: runs the program in the
/// file in the caller's slot `file`, with the strings in `block`.
#[inline(never)]
pub fn exec(
    user: &UserRef,
    target: &ProcessRef,
    slot: u32,
    label: u32,
    file: u32,
    block: UserBuffer,
) -> Result<Answer> {
    let own = ProcessRef::ptr_eq(target, &user.process);
    if !own && target.state().0 != ProcessState::Created {
        return Err(Kernel::InvalidArgument.status(0));
    }
    let (_, file) = objects::file(user.process.cspace(), file, Rights::READ)?;
    let bytes = copy_block(block)?;
    let block = Block::read(&bytes).map_err(|err| match err {
        BlockError::TooLong => error(Kernel::ArgumentsTooLong),
        BlockError::Malformed => Kernel::InvalidArgument.status(1),
    })?;
    let mut extra = block.extra();
    let cwd = extra.next();
    let execfn = extra.next();

    // The program, or a script and its interpreter.
    let mut program_file = File::open(&file.at)?;
    drop(file);
    let mut script = None;
    if let Some(line) = shebang(&program_file.head) {
        let interpreter = copied(line.interpreter)?;
        let argument = line.argument.map(copied).transpose()?;
        program_file = open_from(target, &interpreter)?;
        if program_file.head.starts_with(b"#!") {
            return Err(error(Kernel::NotExecutable));
        }
        script = Some((interpreter, argument));
    }
    let program = program_file.program()?;
    let interpreter = match program.interpreter {
        Some((offset, len)) => {
            let mut path = zeroed(len as usize)?;
            read_exact(&program_file.node, offset, &mut path)?;
            let path = path.split(|&b| b == 0).next().unwrap_or(&[]);
            let file = open_from(target, path)?;
            let found = file.program()?;
            if found.image.kind != Kind::Relocatable || found.interpreter.is_some() {
                return Err(error(Kernel::NotExecutable));
            }
            Some((file, found))
        }
        None => None,
    };

    // The new address space, filled.
    let domain = target.account().domain().clone();
    let space = SpaceObject::new(Some(domain)).ok_or(error(Kernel::NoMemory))?;
    let window = match program.image.kind {
        Kind::Fixed => USER_START + PAGE_SIZE..ANONYMOUS_TOP / 2,
        Kind::Relocatable => startup::IMAGE_BASE..startup::IMAGE_BASE + startup::WINDOW,
    };
    let main = map_program(&space, &program_file, &program, window)?;
    let loader = match &interpreter {
        Some((file, found)) => {
            let window = startup::LOADER_BASE..startup::LOADER_BASE + startup::WINDOW;
            Some(map_program(&space, file, found, window)?)
        }
        None => None,
    };
    if program.image.kind == Kind::Relocatable && interpreter.is_none() {
        relocate(&space, &program_file, &program, main.bias)?;
    }
    map_startup(&space, main.bias)?;

    let sp = build_stack(
        &space,
        &block,
        script.as_ref(),
        (cwd, execfn),
        &program,
        &main,
        loader.as_ref(),
    )?;
    let entry = loader.as_ref().map_or(main.entry, |loader| loader.entry);
    let run_as = execfn.or(block.args().next()).unwrap_or(b"");
    let name = base_name(run_as);
    let name = core::str::from_utf8(&name[..name.len().min(process::NAME_MAX as usize)])
        .unwrap_or("program");
    let name = Name::new(name);

    if own {
        replace_own(user, target, space, [entry, sp, 0], name)
    } else {
        start_created(user, target, slot, space, [entry, sp, 0], name)?;
        answer(label, 0, [0; 8])
    }
}

/// A script's interpreter, and the argument its `#!` line gives it.
type Script = (Vec<u8, Kmalloc>, Option<Vec<u8, Kmalloc>>);

/// Lays out the new program's stack in `space` as the System V ABI says:
/// its arguments (the script's interpreter's first, for a script), its
/// environment and its auxiliary vector, below a random top with
/// [`STACK_BYTES`] of room. Returns the stack pointer it starts with.
#[inline(never)]
fn build_stack(
    space: &SpaceObject,
    block: &Block<'_>,
    script: Option<&Script>,
    (cwd, execfn): (Option<&[u8]>, Option<&[u8]>),
    program: &Program,
    main: &Mapped,
    loader: Option<&Mapped>,
) -> Result<u64> {
    let mut args: Vec<&[u8], Kmalloc> = Vec::new_in(kmalloc());
    let mut env: Vec<&[u8], Kmalloc> = Vec::new_in(kmalloc());
    let count = block.arg_count() as usize + 3;
    args.try_reserve_exact(count).map_err(|_| error(Kernel::NoMemory))?;
    env.try_reserve_exact(block.env_count() as usize).map_err(|_| error(Kernel::NoMemory))?;
    let mut given = block.args();
    let first = given.next();
    let run_as = execfn.or(first).unwrap_or(b"");
    if let Some((interpreter, argument)) = script {
        // `#!interpreter argument`, then the script, then its arguments.
        let _ = args.push_within_capacity(&interpreter[..]);
        if let Some(argument) = argument {
            let _ = args.push_within_capacity(&argument[..]);
        }
        let _ = args.push_within_capacity(run_as);
    } else if let Some(first) = first {
        let _ = args.push_within_capacity(first);
    }
    for arg in given {
        let _ = args.push_within_capacity(arg);
    }
    for var in block.env() {
        let _ = env.push_within_capacity(var);
    }
    let auxv = [
        (aux::AT_PHDR, main.headers.unwrap_or(0)),
        (aux::AT_PHENT, Program::HEADER_SIZE),
        (aux::AT_PHNUM, u64::from(program.header_count)),
        (aux::AT_PAGESZ, PAGE_SIZE),
        (aux::AT_BASE, loader.map_or(0, |loader| loader.bias)),
        (aux::AT_FLAGS, 0),
        (aux::AT_ENTRY, main.entry),
        (aux::AT_UID, 0),
        (aux::AT_EUID, 0),
        (aux::AT_GID, 0),
        (aux::AT_EGID, 0),
        (aux::AT_HWCAP, 0),
        (aux::AT_CLKTCK, 100),
        (aux::AT_SECURE, 0),
        (aux::AT_RADIX_STARTUP, startup::STARTUP_ADDRESS),
    ];
    let mut random = [0u8; 16];
    crate::user::random::fill(&mut random);
    let stack = Stack {
        args: &args,
        env: &env,
        aux: &auxv,
        execfn: run_as,
        cwd,
        platform: b"x86_64",
        random,
    };
    let room = stack.size().next_multiple_of(16);
    if room > STACK_BYTES / 4 {
        return Err(error(Kernel::ArgumentsTooLong));
    }
    let windows = startup::STACK_WINDOW / PAGE_SIZE - STACK_BYTES / PAGE_SIZE;
    let top = startup::STACK_TOP - radix_random::below(windows) * PAGE_SIZE;
    let anon = Anon { access: Some(UserAccess::ReadWrite), shared: false, file: None };
    space
        .map_anonymous(STACK_BYTES / PAGE_SIZE, anon, Placement::FixedNoReplace(top - STACK_BYTES))
        .map_err(|_| error(Kernel::NoMemory))?;
    let mut out = zeroed(room as usize)?;
    let sp = stack.write(top, &mut out).map_err(|_| error(Kernel::ArgumentsTooLong))?;
    space.write_new(top - room, &out)?;
    Ok(sp)
}

/// Makes `space` the address space of `process`: its hidden slot 0 holds
/// a new original capability to it, and SLOT_SPACE one derived from that;
/// the slots with close_on_exec are emptied. With `thread`, that slot is
/// emptied too, for the new program's thread.
fn install_space(process: &ProcessRef, space: &SpaceRef, empty_thread: bool) -> Result<()> {
    let kept = SlotRef::new(process.keep_space(), 0);
    // Every capability to the old space goes with its original.
    caps::revoke(kept).map_err(cap_error)?;
    let cspace = process.cspace();
    caps::with(|store| -> Result<()> {
        objects::reserve(store, 1).map_err(|_| error(Kernel::NoMemory))?;
        store.delete(kept, &mut caps::release).map_err(cap_error)?;
        let id = store.create(kept, ObjectKind::AddressSpace).map_err(cap_error)?;
        objects::insert(store, id, Object::Space(space.clone()))
            .map_err(|_| error(Kernel::NoMemory))?;
        let at = SlotRef::new(cspace, startup::SLOT_SPACE);
        let _ = store.delete(at, &mut caps::release);
        let how = Derivation { rights: ObjectKind::AddressSpace.rights(), ..Derivation::COPY };
        store.derive(kept, at, how).map_err(cap_error)?;
        let mask = SlotFlags::CLOSE_ON_EXEC.0 as u8;
        let mut from = 0;
        while let Some(slot) = store.find_flagged(cspace, mask, from).map_err(cap_error)? {
            let _ = store.delete(SlotRef::new(cspace, slot), &mut caps::release);
            from = slot + 1;
        }
        if empty_thread {
            let _ = store.delete(SlotRef::new(cspace, startup::SLOT_THREAD), &mut caps::release);
        }
        Ok(())
    })
    .map_err(cap_error)
    .and_then(|done| done)?;
    process.set_space(space.clone());
    Ok(())
}

/// The caller's own process runs the new program: its other threads stop,
/// `space` takes the old one's place, and a thread starts at `start`;
/// the calling thread then exits.
fn replace_own(
    user: &UserRef,
    process: &ProcessRef,
    space: SpaceRef,
    start: [u64; 3],
    name: Name,
) -> Result<Answer> {
    let me = sched::current().ok_or(error(Kernel::Gone))?;
    // The other threads stop before anything changes: none of them may see
    // the old program's capabilities after the new program starts.
    process.stop_other_threads(&me);
    wait::wait_until(process.threads_key(), "exec", || {
        process.thread_count() <= 1 || me.should_stop()
    });
    if me.should_stop() {
        return Err(super::signal::interrupted(super::signal::Restart::Never));
    }
    if let Err(err) = install_space(process, &space, true) {
        // Past saving: the old program's capabilities may be gone.
        warn!("User: process {} could not run its new program: {:#x}", process.number(), err.0);
        process.posix.lock().ended_by.get_or_insert(radix_abi::signal::SIGKILL);
        process.exit(EXIT_STOPPED);
        return Ok(Answer::Exit(EXIT_STOPPED));
    }
    let (mask, mut pending) = {
        let mut signals = user.signals.lock();
        let pending = core::mem::replace(
            &mut signals.pending,
            super::new_pending().ok_or(error(Kernel::NoMemory))?,
        );
        (signals.mask, pending)
    };
    process.posix.lock().actions.reset_for_exec();
    with_tree(|tree| {
        if let Some(member) = tree.get_mut(process.number()) {
            member.execed = true;
        }
    });
    process.set_name(name);
    let started = ProcessObject::start(
        process,
        Start::Fresh(start),
        startup::SLOT_THREAD,
        None,
        mask,
        &mut |thread| {
            // The signals pending for the old thread are the new one's.
            if let Some(new) = thread.user() {
                let mut signals = new.signals.lock();
                while let Some((signal, info)) = pending.take(u64::MAX) {
                    signals.pending.add(signal, info);
                }
            }
            Ok(())
        },
    );
    if let Err(err) = started {
        warn!("User: process {} could not start its new program: {:#x}", process.number(), err.0);
        process.posix.lock().ended_by.get_or_insert(radix_abi::signal::SIGKILL);
        process.exit(EXIT_STOPPED);
        return Ok(Answer::Exit(EXIT_STOPPED));
    }
    Ok(Answer::Exit(0))
}

/// A process `create` made, whose capability is in `user`'s slot `slot`,
/// runs the new program: it gets `space`, its own process in
/// SLOT_PROCESS if that slot is empty, a place in the tree as the caller's
/// child, and a thread at `start`.
fn start_created(
    user: &UserRef,
    process: &ProcessRef,
    slot: u32,
    space: SpaceRef,
    start: [u64; 3],
    name: Name,
) -> Result<()> {
    install_space(process, &space, false)?;
    let me = &user.process;
    let own = SlotRef::new(process.cspace(), startup::SLOT_PROCESS);
    if matches!(caps::get(own), Err(radix_cap::CapError::Empty)) {
        let how = Derivation { rights: Rights::READ.union(Rights::CONTROL), ..Derivation::COPY };
        caps::with(|store| store.derive(SlotRef::new(me.cspace(), slot), own, how))
            .and_then(|done| done)
            .map_err(cap_error)?;
    }
    with_tree(|tree| -> Result<()> {
        super::join(tree, me)?;
        tree.insert_child(process.number(), me.number(), process.clone())
            .map_err(super::tree_error)?;
        if let Some(member) = tree.get_mut(process.number()) {
            member.execed = true;
        }
        Ok(())
    })?;
    process.make_posix_child();
    {
        let terminal = me.posix.lock().terminal.clone();
        let mut posix = process.posix.lock();
        posix.in_tree = true;
        posix.terminal = terminal;
    }
    process.set_name(name);
    let mask = user.signals.lock().mask;
    ProcessObject::start(
        process,
        Start::Fresh(start),
        startup::SLOT_THREAD,
        None,
        mask,
        &mut |_| Ok(()),
    )?;
    Ok(())
}
