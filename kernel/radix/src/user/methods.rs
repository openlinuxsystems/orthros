// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods `cap_call` calls on kernel objects.
//!
//! A label is an interface number and a method number (`(interface << 16)
//! | method`, radix-ipc(7)). The methods of radix-capability(2), with
//! interface 0xffff, work on every capability; the others only on the kind
//! their interface is for. Arguments and results travel in the message's
//! data words, packed and checked by the code ridlc generates from
//! `abi/*.ridl`, so the kernel and every program agree on them.

use radix_abi::capability::{self as abi_cap, ObjectKind as AbiKind, Rights as AbiRights};
use radix_abi::errors::Kernel;
use radix_abi::{address_space, memory, random, ring, thread};
use radix_cap::{CapError, Capability, Derivation, ObjectKind, Rights, SlotRef};
use radix_ipc::info::Info;
use radix_klog::info;

use super::memory::{cap_rights, frames_made};
use super::objects;
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
}

fn answer(label: u32, words: usize, values: [u64; 8]) -> Result<Answer> {
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
        ObjectKind::Endpoint => radix_abi::endpoint::VERSION,
        ObjectKind::Notification => radix_abi::notification::VERSION,
        ObjectKind::Random => random::VERSION,
        ObjectKind::Ring => ring::VERSION,
        ObjectKind::KernelLog => radix_abi::kernel_log::VERSION,
        _ => 0,
    }
}

/// Calls the method `info`'s label names on the capability in `slot`,
/// which does not designate an endpoint: `cap`, if the caller has just
/// looked it up.
pub fn call(
    me: &ThreadRef,
    user: &UserRef,
    slot: u32,
    cap: Option<Capability>,
    info: Info,
    words: [u64; 8],
) -> Result<Answer> {
    // The data words past the count are not the caller's arguments. A
    // copy of a fixed size compiles to moves; one of `info.words` words
    // would call `memcpy`, whose string moves cost more than the words.
    let args: [u64; 8] = core::array::from_fn(|i| if i < info.words { words[i] } else { 0 });
    let at = SlotRef::new(user.cspace, slot);
    if info.is_reserved() {
        return capability(at, info.label, &args);
    }
    let cap = match cap {
        Some(cap) => cap,
        None => objects::capability(user.cspace, slot)?,
    };
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
        (ObjectKind::Thread, _) => thread_method(me, user, slot, info.label, &args),
        (ObjectKind::Random, random::READ) => {
            let a = random::ReadArgs::decode(&args)?;
            cap.check(ObjectKind::Random, Rights::READ).map_err(cap_error)?;
            read_random(a.buffer.address, a.buffer.len)?;
            answer(info.label, 0, [0; 8])
        }
        (ObjectKind::Random, random::WORDS) => {
            random::WordsArgs::decode(&args)?;
            cap.check(ObjectKind::Random, Rights::READ).map_err(cap_error)?;
            let mut bytes = [0u8; 64];
            super::random::fill(&mut bytes);
            let mut values = [0u64; 8];
            for (value, chunk) in values.iter_mut().zip(bytes.as_chunks::<8>().0) {
                *value = u64::from_le_bytes(*chunk);
            }
            answer(info.label, random::WordsResults::WORDS, values)
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

/// The methods every capability answers.
fn capability(at: SlotRef, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let space = at.space;
    match label {
        abi_cap::INFO => {
            abi_cap::InfoArgs::decode(args)?;
            let cap: Capability = caps::get(at).map_err(cap_error)?;
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
            caps::delete(at).map_err(cap_error)?;
            answer(label, 0, [0; 8])
        }
        abi_cap::REVOKE => {
            abi_cap::RevokeArgs::decode(args)?;
            caps::revoke(at).map_err(cap_error)?;
            answer(label, 0, [0; 8])
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
    if kind == ObjectKind::Frame {
        objects::reserve(count as usize).map_err(|_| error(Kernel::NoMemory))?;
    }
    caps::with(|store| -> Result<()> {
        store.retype(memory, kind, order, to, count).map_err(cap_error)?;
        if kind != ObjectKind::Frame {
            return Ok(());
        }
        let mut made = [(radix_cap::ObjectId { index: 0, generation: 0 }, 0, 0); 1];
        for i in 0..count {
            let at = SlotRef::new(to.space, to.slot + i);
            let found = store.get(at).ok().and_then(|cap| {
                let id = cap.id()?;
                let extent = store.object(id)?.extent?;
                Some((id, extent.base, extent.pages))
            });
            let made_one = match found {
                Some(frame) => {
                    made[0] = frame;
                    frames_made(&made)
                }
                None => Err(error(Kernel::NoMemory)),
            };
            if let Err(err) = made_one {
                // All or nothing: the frames made so far go again.
                for j in 0..count {
                    let _ = store.delete(SlotRef::new(to.space, to.slot + j), &mut caps::release);
                }
                return Err(err);
            }
        }
        Ok(())
    })
    .map_err(cap_error)
    .and_then(|done| done)
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
