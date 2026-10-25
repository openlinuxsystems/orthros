// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods `cap_call` calls on kernel objects.
//!
//! A label is an interface number and a method number (`(interface << 16)
//! | method`, radix-ipc(7)). The methods of radix-capability(2), with
//! interface 0xffff, work on every capability; the others only on the kind
//! their interface is for. Arguments and results travel in the message's
//! data words, packed and checked by the code ridlc generates from
//! `abi/*.ridl`, so the kernel and every program agree on them.

use alloc::vec::Vec;

use radix_abi::capability::{self as abi_cap, ObjectKind as AbiKind, Rights as AbiRights};
use radix_abi::errors::Kernel;
use radix_abi::{address_space, memory, random, ring, thread};
use radix_cap::{CapError, Derivation, ObjectKind, Rights, SlotRef};
use radix_ipc::info::Info;
use radix_klog::info;
use radix_mm::{Flags, HEAP};

use super::memory::{Anon, FileSource, Placement, cap_rights, frame_made, futex};
use super::objects::{self, Found, Object};
use super::{Result, UserRef, cap_error, error};
use crate::caps;
use crate::sched::ThreadRef;

/// What a method did.
pub enum Answer {
    /// It answered, with an info word and data words.
    Reply(u64, [u64; 8]),
    /// The caller exits, with this code: the system call ends it once it
    /// has let go of what it holds.
    Exit(u64),
    /// The caller returns from a signal handler (`signal_return`, Phase
    /// 8): the system call returns these registers in place of results.
    Restore(radix_hal::SyscallRegs),
}

/// A reply to the method `label`, with `words` of `values`.
pub(super) fn answer(label: u32, words: usize, values: [u64; 8]) -> Result<Answer> {
    Ok(Answer::Reply(Info::reply(label, words), values))
}

/// The version of the interface a kind answers, 0 for kinds whose
/// interface is still to come.
fn version(kind: ObjectKind) -> u32 {
    match kind {
        ObjectKind::Memory => memory::VERSION,
        ObjectKind::Frame => memory::frame::VERSION,
        ObjectKind::AddressSpace => address_space::VERSION,
        ObjectKind::Thread => thread::VERSION,
        ObjectKind::Process => radix_abi::process::VERSION,
        ObjectKind::Endpoint => radix_abi::endpoint::VERSION,
        ObjectKind::Notification => radix_abi::notification::VERSION,
        ObjectKind::Random => random::VERSION,
        ObjectKind::Ring => ring::VERSION,
        ObjectKind::KernelLog => radix_abi::kernel_log::VERSION,
        ObjectKind::File => radix_abi::file::VERSION,
        ObjectKind::Directory => radix_abi::directory::VERSION,
        ObjectKind::ResourceDomain => radix_abi::resource_domain::VERSION,
        ObjectKind::Power => radix_abi::power::VERSION,
        _ => 0,
    }
}

/// Whether the methods of a capability of `kind` take the object it
/// designates from the lookup that found it ([`objects::find`]),
/// rather than looking it up themselves: messages to an endpoint, and the
/// file, directory and process methods.
pub fn takes_object(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Endpoint | ObjectKind::File | ObjectKind::Directory | ObjectKind::Process
    )
}

/// The arguments of a method called with `info` and the data words
/// `words`.
fn arguments(info: Info, words: &[u64; 8]) -> [u64; 8] {
    // The data words past the count are not the caller's arguments. A
    // copy of a fixed size compiles to moves; one of `info.words` words
    // would call `memcpy`, whose string moves cost more than the words.
    core::array::from_fn(|i| if i < info.words { words[i] } else { 0 })
}

/// Calls the method of radix-capability(2) that `info`'s label (a reserved
/// one) names, on the capability in `slot`, whatever its kind.
pub fn call_capability(user: &UserRef, slot: u32, info: Info, words: [u64; 8]) -> Result<Answer> {
    let args = arguments(info, &words);
    capability(user, SlotRef::new(user.cspace, slot), info.label, &args)
}

/// Calls the method `info`'s label names on `found`, what the caller has
/// just found in `slot`, with the object [`takes_object`] wants. Messages
/// to an endpoint do not come here (`syscall`), nor do the methods of
/// radix-capability(2) ([`call_capability`]).
pub fn call(
    me: &ThreadRef,
    user: &UserRef,
    slot: u32,
    found: &Found,
    info: Info,
    words: [u64; 8],
) -> Result<Answer> {
    let args = arguments(info, &words);
    let at = SlotRef::new(user.cspace, slot);
    let cap = &found.cap;
    let kind = cap.kind();
    match (kind, info.label) {
        (ObjectKind::Memory, memory::RETYPE) => {
            let a = memory::RetypeArgs::decode(&args)?;
            let kind = match a.kind {
                AbiKind::Memory => ObjectKind::Memory,
                AbiKind::Frame => ObjectKind::Frame,
                _ => return Err(Kernel::InvalidArgument.status(0)),
            };
            retype(at, kind, a.order, SlotRef::new(user.cspace, a.to), a.count)?;
            answer(info.label, 0, [0; 8])
        }
        (ObjectKind::Memory, memory::INFO) => {
            memory::InfoArgs::decode(&args)?;
            let found = caps::with(|store| -> core::result::Result<_, CapError> {
                let cap = store.lookup(at, ObjectKind::Memory, Rights::READ)?;
                let object = cap.id().and_then(|id| store.object(id)).ok_or(CapError::Empty)?;
                let extent = object.extent.ok_or(CapError::Empty)?;
                Ok((extent.pages, object.watermark))
            });
            let (pages, used) = found.and_then(|found| found).map_err(cap_error)?;
            let results = memory::InfoResults { pages, used };
            answer(info.label, memory::InfoResults::WORDS, results.encode())
        }
        (ObjectKind::Frame, memory::frame::INFO) => {
            memory::frame::InfoArgs::decode(&args)?;
            let (_, frame) = objects::frame(user.cspace, slot, Rights::NONE)?;
            let results = memory::frame::InfoResults { pages: frame.pages() };
            answer(info.label, memory::frame::InfoResults::WORDS, results.encode())
        }
        (ObjectKind::AddressSpace, address_space::QUERY) => {
            let a = address_space::QueryArgs::decode(&args)?;
            if a.address >= address_space::USER_TOP {
                return Err(Kernel::InvalidArgument.status(0));
            }
            let (_, space) = objects::space(user.cspace, slot, Rights::READ)?;
            let (mapped, rights) = space.query(a.address);
            let results = address_space::QueryResults { mapped, rights };
            answer(info.label, address_space::QueryResults::WORDS, results.encode())
        }
        (ObjectKind::AddressSpace, _) => address_space_method(user, slot, info.label, &args),
        (ObjectKind::Thread, _) => thread_method(me, user, slot, info.label, &args),
        (ObjectKind::Process, _) => super::process::method(user, slot, found, info.label, &args),
        (ObjectKind::ResourceDomain, _) => super::domain::method(user, slot, info.label, &args),
        (ObjectKind::Power, _) => super::power::method(user, cap, info.label, &args),
        (ObjectKind::Directory, _) => {
            super::files::directory_method(user, found, info.label, &args)
        }
        (ObjectKind::File, _) => super::files::file_method(user, found, info.label, &args),
        (ObjectKind::Random, random::READ) => {
            let a = random::ReadArgs::decode(&args)?;
            cap.check(ObjectKind::Random, Rights::READ).map_err(cap_error)?;
            read_random(a.buffer.address, a.buffer.len)?;
            answer(info.label, 0, [0; 8])
        }
        (ObjectKind::Random, random::WORDS) => {
            random::WordsArgs::decode(&args)?;
            cap.check(ObjectKind::Random, Rights::READ).map_err(cap_error)?;
            answer(info.label, random::WordsResults::WORDS, super::random::words())
        }
        (ObjectKind::Ring, ring::INFO) => {
            ring::InfoArgs::decode(&args)?;
            let (_, object) = objects::ring(user.cspace, slot, Rights::NONE)?;
            let (entries, completions) = super::ring::info(&object);
            let results = ring::InfoResults { entries, completions };
            answer(info.label, ring::InfoResults::WORDS, results.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// The methods every capability answers, called by `user`'s thread.
fn capability(user: &UserRef, at: SlotRef, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let space = at.space;
    match label {
        abi_cap::INFO => {
            abi_cap::InfoArgs::decode(args)?;
            let cap = objects::capability(space, at.slot)?;
            let kind = AbiKind::from_raw(cap.kind() as u8).ok_or(error(Kernel::WrongKind))?;
            let results = abi_cap::InfoResults {
                kind,
                rights: AbiRights(u64::from(cap.rights().bits())),
                badge: cap.badge(),
                version: version(cap.kind()),
            };
            answer(label, abi_cap::InfoResults::WORDS, results.encode())
        }
        abi_cap::DERIVE => {
            let a = abi_cap::DeriveArgs::decode(args)?;
            let how = Derivation {
                rights: cap_rights(a.rights),
                badge: (a.badge != 0).then_some(a.badge),
                range: None,
            };
            let to = SlotRef::new(space, a.to);
            caps::with(|store| store.derive(at, to, how))
                .and_then(|done| done)
                .map_err(cap_error)?;
            answer(label, 0, [0; 8])
        }
        abi_cap::MOVE_TO => {
            let a = abi_cap::MoveToArgs::decode(args)?;
            caps::with(|store| store.move_cap(at, SlotRef::new(space, a.to)))
                .and_then(|done| done)
                .map_err(cap_error)?;
            answer(label, 0, [0; 8])
        }
        abi_cap::DELETE => {
            abi_cap::DeleteArgs::decode(args)?;
            // As POSIX has it, a process's record locks on a file go when
            // it closes any descriptor of the file.
            let process = &user.process;
            let file = match process.has_record_locks().then(|| caps::get(at)) {
                Some(Ok(cap)) => match objects::object_of(&cap) {
                    Ok(Object::File(file) | Object::Directory(file)) => Some(file),
                    _ => None,
                },
                _ => None,
            };
            caps::delete(at).map_err(cap_error)?;
            if let Some(file) = file {
                crate::fs::locks::process_gone(process.number(), Some(&file.at.node));
            }
            answer(label, 0, [0; 8])
        }
        abi_cap::REVOKE => {
            abi_cap::RevokeArgs::decode(args)?;
            caps::revoke(at).map_err(cap_error)?;
            answer(label, 0, [0; 8])
        }
        abi_cap::FLAGS => {
            let a = abi_cap::FlagsArgs::decode(args)?;
            let bits = u8::try_from(a.flags.0).map_err(|_| Kernel::InvalidArgument.status(1))?;
            let old = caps::with(|store| {
                if a.set { store.set_slot_flags(at, bits) } else { store.slot_flags(at) }
            })
            .and_then(|done| done)
            .map_err(cap_error)?;
            let results = abi_cap::FlagsResults { flags: abi_cap::SlotFlags(u32::from(old)) };
            answer(label, abi_cap::FlagsResults::WORDS, results.encode())
        }
        abi_cap::SEAL | abi_cap::UNSEAL => {
            let (key, to) = if label == abi_cap::SEAL {
                let a = abi_cap::SealArgs::decode(args)?;
                (a.key, a.to)
            } else {
                let a = abi_cap::UnsealArgs::decode(args)?;
                (a.key, a.to)
            };
            let (key, to) = (SlotRef::new(space, key), SlotRef::new(space, to));
            caps::with(|store| {
                if label == abi_cap::SEAL {
                    store.seal(at, key, to)
                } else {
                    store.unseal(at, key, to)
                }
            })
            .and_then(|done| done)
            .map_err(cap_error)?;
            answer(label, 0, [0; 8])
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// `memory.retype`: makes memory or frame objects. Frames are zeroed and
/// recorded before the store lets go, so no capability to one is used
/// before its object exists.
pub fn retype(
    memory: SlotRef,
    kind: ObjectKind,
    order: u32,
    to: SlotRef,
    count: u32,
) -> Result<()> {
    if !matches!(kind, ObjectKind::Memory | ObjectKind::Frame) {
        return Err(Kernel::InvalidArgument.status(0));
    }
    if order > 20 {
        return Err(Kernel::InvalidArgument.status(1));
    }
    // Checked before the registry makes room for `count` frames, which
    // would otherwise ask the heap for room for any number a program wrote.
    if count == 0 || count > radix_cap::MAX_RETYPE {
        return Err(Kernel::InvalidArgument.status(3));
    }
    // Room to remember the new frames, to zero them once the store's lock
    // has gone.
    let mut frames = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    if kind == ObjectKind::Frame {
        frames.try_reserve_exact(count as usize).map_err(|_| error(Kernel::NoMemory))?;
    }
    caps::with(|store| -> Result<()> {
        if kind == ObjectKind::Frame {
            objects::reserve(store, count as usize).map_err(|_| error(Kernel::NoMemory))?;
        }
        store.retype(memory, kind, order, to, count).map_err(cap_error)?;
        if kind != ObjectKind::Frame {
            return Ok(());
        }
        for i in 0..count {
            let at = SlotRef::new(to.space, to.slot + i);
            let found = store.get(at).ok().and_then(|cap| {
                let id = cap.id()?;
                let extent = store.object(id)?.extent?;
                Some((id, extent.base, extent.pages))
            });
            let made = match found {
                Some((id, base, pages)) => frame_made(store, id, base, pages),
                None => Err(error(Kernel::NoMemory)),
            };
            match made {
                // Room was reserved above.
                Ok(frame) => drop(frames.push_within_capacity(frame)),
                Err(err) => {
                    // All or nothing: the frames made so far go again.
                    for j in 0..count {
                        let _ = store.delete(SlotRef::new(to.space, to.slot + j));
                    }
                    return Err(err);
                }
            }
        }
        Ok(())
    })
    .map_err(cap_error)
    .and_then(|done| done)?;
    // Zeroed outside the store's lock, before the call returns. A frame
    // deleted meanwhile stops being zeroed, and is not mapped any more.
    for frame in &frames {
        frame.zero();
    }
    Ok(())
}

/// `random.read`: fills a buffer of the caller's with random bytes.
fn read_random(address: u64, len: u64) -> Result<()> {
    if len > random::READ_MAX {
        return Err(Kernel::InvalidArgument.status(0));
    }
    if !radix_ipc::range::user_bytes(address, len) {
        return Err(error(Kernel::BadAddress));
    }
    let mut chunk = [0u8; 256];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(chunk.len() as u64) as usize;
        super::random::fill(&mut chunk[..n]);
        let copied =
            <crate::Arch as radix_hal::UserMode>::copy_to_user(address + done, &chunk[..n]);
        chunk.fill(0);
        if !copied {
            return Err(error(Kernel::BadAddress));
        }
        done += n as u64;
    }
    Ok(())
}

/// The access anonymous memory's `rights` ask for (Phase 8): any of read,
/// write and execute, write implying read as on x86-64, write and execute
/// never together; none keeps the memory out of reach.
fn anonymous_access(rights: AbiRights) -> Result<Option<radix_hal::UserAccess>> {
    use radix_hal::UserAccess;
    let known = AbiRights::READ.0 | AbiRights::WRITE.0 | AbiRights::EXECUTE.0;
    if rights.bits() & !known != 0 {
        return Err(Kernel::InvalidArgument.status(2));
    }
    let (write, execute) = (rights.contains(AbiRights::WRITE), rights.contains(AbiRights::EXECUTE));
    Ok(match (rights.contains(AbiRights::READ), write, execute) {
        (_, true, true) => return Err(Kernel::InvalidArgument.status(2)),
        (_, true, false) => Some(UserAccess::ReadWrite),
        (_, false, true) => Some(UserAccess::ReadExecute),
        (true, false, false) => Some(UserAccess::Read),
        (false, false, false) => None,
    })
}

/// Where `map_anonymous` and `map_file` put what they map.
fn placement(address: u64, flags: address_space::MapFlags) -> Placement {
    if flags.contains(address_space::MapFlags::FIXED_NO_REPLACE) {
        Placement::FixedNoReplace(address)
    } else if flags.contains(address_space::MapFlags::FIXED) {
        Placement::Fixed(address)
    } else {
        Placement::Hint(address)
    }
}

/// The pages `size` bytes take.
fn pages_of(size: u64) -> Result<u64> {
    let pages = size.div_ceil(address_space::PAGE_SIZE);
    if pages == 0 || pages > address_space::USER_TOP / address_space::PAGE_SIZE {
        return Err(Kernel::InvalidArgument.status(1));
    }
    Ok(pages)
}

/// The address space interface's methods since Phase 8 (`query` is
/// answered in [`call`]): anonymous memory and futexes.
fn address_space_method(user: &UserRef, slot: u32, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let (_, space) = objects::space(user.cspace, slot, Rights::CONTROL)?;
    let aligned = |address: u64| address.is_multiple_of(address_space::PAGE_SIZE);
    match label {
        address_space::MAP_ANONYMOUS => {
            let a = address_space::MapAnonymousArgs::decode(args)?;
            let access = anonymous_access(a.rights)?;
            let shared = a.flags.contains(address_space::MapFlags::SHARED);
            let placement = placement(a.address, a.flags);
            if !matches!(placement, Placement::Hint(_)) && !aligned(a.address) {
                return Err(Kernel::InvalidArgument.status(0));
            }
            let anon = Anon { access, shared, file: None };
            let address = space.map_anonymous(pages_of(a.size)?, anon, placement)?;
            let results = address_space::MapAnonymousResults { address };
            answer(label, address_space::MapAnonymousResults::WORDS, results.encode())
        }
        address_space::PROTECT => {
            let a = address_space::ProtectArgs::decode(args)?;
            if !aligned(a.address) {
                return Err(Kernel::InvalidArgument.status(0));
            }
            space.protect(a.address, pages_of(a.size)?, anonymous_access(a.rights)?)?;
            answer(label, 0, [0; 8])
        }
        address_space::DISCARD => {
            let a = address_space::DiscardArgs::decode(args)?;
            if !aligned(a.address) {
                return Err(Kernel::InvalidArgument.status(0));
            }
            space.discard(a.address, pages_of(a.size)?)?;
            answer(label, 0, [0; 8])
        }
        address_space::MAP_FILE => {
            let a = address_space::MapFileArgs::decode(args)?;
            let access = anonymous_access(a.rights)?;
            if a.flags.contains(address_space::MapFlags::SHARED) {
                return Err(Kernel::InvalidArgument.status(3));
            }
            let placement = placement(a.address, a.flags);
            if !a.offset.is_multiple_of(address_space::PAGE_SIZE)
                || (!matches!(placement, Placement::Hint(_)) && !aligned(a.address))
            {
                return Err(Kernel::InvalidArgument.status(5));
            }
            let (_, file) = objects::file(user.cspace, a.file, Rights::READ)?;
            let stat = file.at.node.stat();
            match stat.kind {
                radix_vfs::Kind::File => {}
                radix_vfs::Kind::Directory => {
                    return Err(radix_abi::errors::File::IsDirectory.status(0));
                }
                _ => return Err(Kernel::InvalidArgument.status(4)),
            }
            let source = FileSource {
                node: file.at.node.clone(),
                base: a.offset,
                size: stat.size.saturating_sub(a.offset),
            };
            let anon = Anon { access, shared: false, file: Some(source) };
            let address = space.map_anonymous(pages_of(a.size)?, anon, placement)?;
            let results = address_space::MapFileResults { address };
            answer(label, address_space::MapFileResults::WORDS, results.encode())
        }
        address_space::FUTEX_WAIT => {
            let a = address_space::FutexWaitArgs::decode(args)?;
            // The word is read where the caller runs.
            if !core::ptr::eq(&raw const *space, user.space()) {
                return Err(Kernel::InvalidArgument.status(0));
            }
            let deadline =
                (a.timeout_ns != 0).then(|| crate::sched::now_ns().saturating_add(a.timeout_ns));
            futex::wait(&space, a.address, a.expected, deadline, user)?;
            answer(label, 0, [0; 8])
        }
        address_space::FUTEX_WAKE => {
            let a = address_space::FutexWakeArgs::decode(args)?;
            let woken = futex::wake(&space, a.address, a.count)?;
            let results = address_space::FutexWakeResults { woken };
            answer(label, address_space::FutexWakeResults::WORDS, results.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// The thread interface.
fn thread_method(
    me: &ThreadRef,
    user: &UserRef,
    slot: u32,
    label: u32,
    args: &[u64; 8],
) -> Result<Answer> {
    let needs = if label == thread::INFO { Rights::READ } else { Rights::CONTROL };
    let (_, target) = objects::thread(user.cspace, slot, needs)?;
    let mine = ThreadRef::ptr_eq(&target, me);
    match label {
        thread::EXIT => {
            let a = thread::ExitArgs::decode(args)?;
            if !mine {
                // It ends at its next return to user mode.
                target.request_stop();
                crate::sched::wake(&target);
                return answer(label, 0, [0; 8]);
            }
            if a.code != 0 {
                info!("User: thread {} ({}) exited with code {:#x}", me.id(), me.name(), a.code);
            }
            Ok(Answer::Exit(a.code))
        }
        thread::SET_TLS => {
            let a = thread::SetTlsArgs::decode(args)?;
            if a.base >= address_space::USER_TOP {
                return Err(Kernel::InvalidArgument.status(0));
            }
            target.set_thread_pointer(a.base);
            answer(label, 0, [0; 8])
        }
        thread::SET_RECEIVE_WINDOW => {
            let a = thread::SetReceiveWindowArgs::decode(args)?;
            if a.count > 4096 || a.first.checked_add(a.count).is_none() {
                return Err(Kernel::InvalidArgument.status(1));
            }
            let window = target.user().ok_or(error(Kernel::WrongKind))?;
            let word = (u64::from(a.first) << 32) | u64::from(a.count);
            window.window.store(word, core::sync::atomic::Ordering::Relaxed);
            answer(label, 0, [0; 8])
        }
        thread::INFO => {
            thread::InfoArgs::decode(args)?;
            let results = thread::InfoResults { id: target.id(), cpu: target.cpu() as u32 };
            answer(label, thread::InfoResults::WORDS, results.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}
