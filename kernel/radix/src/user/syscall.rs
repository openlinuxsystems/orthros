// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The twelve system calls (radix-ipc(7)).
//!
//! The architecture saves the caller's registers and calls [`handle`]
//! with interrupts off, on the thread's kernel stack. The call runs with
//! interrupts on, like any kernel code, and may block. Its status goes
//! back in the number register and its results in the argument registers;
//! on an error every result register is 0, so nothing the kernel had in
//! hand leaks out.
//!
//! A call a signal interrupted (Phase 8) fails with `interrupted`, and its
//! arguments are kept (`posix::signal::call_failed`): on the way back to
//! user mode it runs again with them, unless a handler that does not
//! restart calls runs, as radix-signal(2) says.
//!
//! The calls of a process a trace follows (radix-trace(2)) take a slow
//! way, which writes each one's record as it returns (`fs::trace`); the
//! others pay one look at a flag of the process.

use radix_abi::endpoint::{MessageFlags, NO_SLOT};
use radix_abi::errors::Kernel;
use radix_abi::kernel_log::WRITE_MAX;
use radix_abi::notification::WaitFlags;
use radix_abi::syscalls as sys;
use radix_cap::{CapError, ObjectKind, Rights, SpaceId};
use radix_hal::{Cpu, SyscallRegs, UserMode};
use radix_ipc::info::Info;
use radix_klog::info;

use super::ipc::{self, Delivery, Message};
use super::memory::{FrameObject, access_of, cap_rights};
use super::methods::{self, Answer};
use super::objects::{self, Object};
use super::process::Ending;
use super::{EXIT_STOPPED, Result, UserRef, cap_error, error, exit_current};
use crate::Arch;
use crate::fs::kmsg::Shown;
use crate::sched::{self, ThreadRef};

/// What a system call did.
enum Done {
    /// It returned, with its results in the argument registers.
    Return,
    /// The caller exits.
    Exit(u64),
    /// It returned from a signal handler (`signal_return`): the number
    /// register gets this, and the argument registers what the frame
    /// saved.
    Restore(u64),
}

/// Handles a system call from user mode.
pub fn handle(regs: &mut SyscallRegs) {
    Arch::enable_interrupts();
    // Borrowed, not counted: the call runs as this thread throughout.
    let running = sched::running();
    let Some((me, user)) = running.as_deref().and_then(|me| Some((me, me.user()?))) else {
        panic!("User mode: a system call from a thread that is not a program's");
    };
    // Kept for the log line of a thread stopped in the call.
    let (number, info) = (regs.number, regs.args[1]);
    // One call site, so that `dispatch` stays inlined in this fast path.
    let mut begun = if user.process.traced() { begin(user, regs) } else { None };
    // What `begin` noted holds the trace, which must go with the last
    // thread that follows it, so that the tracer reads the end of it.
    let status = call(me, user, regs, || drop(begun.take()));
    if let Some(begun) = begun {
        end(user, begun, status, regs);
    }
    if me.should_stop() {
        // A signal that ends a POSIX process is its parent's to report.
        if user.process.posix.lock().ended_by.is_none() {
            stopped(me, number, info);
        }
        super::note_domain_stop(me, user);
        exit_current(user.clone(), EXIT_STOPPED, Ending::Stop);
    }
}

/// Runs the system call in `regs` and puts what it returns there; returns
/// its status: 0, or the error's. A call that ends the thread does not
/// return: `exiting` runs first, to let go of what the caller holds.
#[inline(always)]
fn call(me: &ThreadRef, user: &UserRef, regs: &mut SyscallRegs, exiting: impl FnOnce()) -> u64 {
    let number = regs.number;
    match dispatch(me, user, number, &mut regs.args) {
        Ok(Done::Return) => regs.number = 0,
        Ok(Done::Exit(code)) => {
            exiting();
            exit_current(user.clone(), code, Ending::Exit)
        }
        Ok(Done::Restore(saved)) => {
            regs.number = saved;
            return 0;
        }
        Err(status) => {
            // A failed call has not touched its arguments.
            if status.code() == Kernel::Interrupted as u16 && status.domain() == Kernel::DOMAIN {
                interrupted(me, user, number, &regs.args, status);
            }
            regs.number = status.0;
            regs.args = [0; 10];
        }
    }
    regs.number
}

/// Notes the start of the call in `regs` of a process a trace follows
/// (radix-trace(2)), whose record is written as it returns ([`end`]).
#[cold]
#[inline(never)]
fn begin(user: &UserRef, regs: &SyscallRegs) -> Option<crate::fs::trace::Begun> {
    crate::fs::trace::begin(user, regs.number, &regs.args)
}

/// Writes the record of a traced call that returned `status`, with its
/// results in `regs`.
#[cold]
#[inline(never)]
fn end(user: &UserRef, begun: crate::fs::trace::Begun, status: u64, regs: &SyscallRegs) {
    crate::fs::trace::end(user, begun, status, &regs.args);
}

/// System call `number` with `args` was interrupted: it is kept to run
/// again. What runs again of a `reply_recv` is its receive, the reply
/// having gone.
#[cold]
#[inline(never)]
fn interrupted(
    me: &ThreadRef,
    user: &UserRef,
    number: u64,
    args: &[u64; 10],
    status: radix_abi::Status,
) {
    let (number, args) = match sys::ReplyRecvArgs::decode(args) {
        Ok(a) if number == sys::REPLY_RECV => {
            (sys::RECV, sys::RecvArgs { cap: a.cap, flags: 0 }.encode())
        }
        _ => (number, *args),
    };
    super::posix::signal::call_failed(me, user, number, &args, status);
}

/// Logs that thread `me` was stopped in system call `number`, with info
/// word `info` (which names a method `cap_call` called).
#[cold]
fn stopped(me: &ThreadRef, number: u64, info: u64) {
    let name = sys::name(number).unwrap_or("an unknown system call");
    if number == sys::CAP_CALL {
        // The label is the info word's low half.
        let label = info as u32;
        info!("User: thread {} ({}) stopped in {name} (label {label:#x})", me.id(), me.name());
    } else {
        info!("User: thread {} ({}) stopped in {name}", me.id(), me.name());
    }
}

/// Puts a system call's results in the argument registers, whose
/// arguments it has read, and returns.
fn results(registers: &mut [u64; 10], words: [u64; 10]) -> Result<Done> {
    // Without SSE, a copy of more than 64 bytes compiles to a string move,
    // whose start-up costs more than the whole system call's work on some
    // CPUs; two smaller copies are plain moves.
    let (low, high) = words.split_at(8);
    registers[..8].copy_from_slice(low);
    registers[8..].copy_from_slice(high);
    Ok(Done::Return)
}

/// Puts a method's answer in the argument registers, or says how the
/// thread goes on.
// It takes the answer as the method gave it; inlined, a move costs nothing.
#[allow(clippy::needless_pass_by_value)]
#[inline(always)]
fn answered(registers: &mut [u64; 10], answer: Result<Answer>) -> Result<Done> {
    // Matched in place rather than through `?`, which would copy the
    // answer first.
    match answer {
        Ok(Answer::Reply(info, words)) => {
            results(registers, sys::CapCallResults { badge: 0, info, words }.encode())
        }
        Ok(Answer::Exit(code)) => Ok(Done::Exit(code)),
        Ok(Answer::Restore(saved)) => {
            *registers = saved.args;
            Ok(Done::Restore(saved.number))
        }
        Err(status) => Err(status),
    }
}

/// The info word of a message a program sends.
fn message_info(word: u64) -> Result<Info> {
    Info::decode(word).map_err(|_| Kernel::InvalidArgument.status(1))
}

/// A received message's results.
// As `answered`, it takes what `ipc::recv` gave.
#[allow(clippy::needless_pass_by_value)]
fn delivered(delivery: Delivery) -> [u64; 10] {
    sys::RecvResults { badge: delivery.badge, info: delivery.info, words: delivery.words }.encode()
}

/// Runs system call `number` on the arguments in `args`, and leaves its
/// results there.
fn dispatch(me: &ThreadRef, user: &UserRef, number: u64, args: &mut [u64; 10]) -> Result<Done> {
    let cspace = user.cspace;
    match number {
        sys::CAP_CALL => {
            let a = sys::CapCallArgs::decode(args)?;
            let info = message_info(a.info)?;
            if info.is_reserved() {
                // radix-capability(2)'s methods, on a capability of any
                // kind.
                if info.caps > 0 || info.move_caps || info.no_block {
                    return Err(Kernel::InvalidArgument.status(1));
                }
                return answered(args, methods::call_capability(user, a.cap, info, a.words));
            }
            // One look at this CPU's cache finds the capability and, for
            // an endpoint, a file, a directory or another process, its
            // object, which the call borrows where the lookup put it.
            let mut place = None;
            let process = Some(&user.process);
            let found =
                objects::find_holding(cspace, a.cap, methods::takes_object, process, &mut place)?;
            let cap = &found.cap;
            match cap.check(ObjectKind::Endpoint, Rights::SEND) {
                Ok(()) => {
                    let Some(Object::Endpoint(endpoint)) = &found.object else {
                        return Err(error(Kernel::Gone));
                    };
                    let message = Message {
                        info,
                        words: a.words,
                        badge: cap.badge(),
                        call: true,
                        grant: cap.rights().contains(Rights::GRANT),
                        cspace,
                    };
                    let reply = ipc::send(me, user, endpoint, message)?;
                    let reply = reply.expect("a call ends with a reply or an error");
                    return results(args, delivered(reply));
                }
                Err(CapError::WrongKind { .. }) => {}
                Err(err) => return Err(cap_error(err)),
            }
            if info.caps > 0 || info.move_caps || info.no_block {
                return Err(Kernel::InvalidArgument.status(1));
            }
            answered(args, methods::call(me, user, a.cap, found, info, a.words))
        }
        sys::SEND => {
            let a = sys::SendArgs::decode(args)?;
            let info = message_info(a.info)?;
            if info.is_reserved() {
                return Err(Kernel::InvalidArgument.status(1));
            }
            let (cap, endpoint) = objects::endpoint(cspace, a.cap, Rights::SEND)?;
            let message = Message {
                info,
                words: a.words,
                badge: cap.badge(),
                call: false,
                grant: cap.rights().contains(Rights::GRANT),
                cspace,
            };
            ipc::send(me, user, &endpoint, message)?;
            results(args, [0; 10])
        }
        sys::RECV => {
            let a = sys::RecvArgs::decode(args)?;
            if a.flags & !MessageFlags::NO_BLOCK.bits() != 0 {
                return Err(Kernel::InvalidArgument.status(1));
            }
            let (_, endpoint) = objects::endpoint(cspace, a.cap, Rights::RECEIVE)?;
            let no_block = a.flags & MessageFlags::NO_BLOCK.bits() != 0;
            results(args, delivered(ipc::recv(me, user, &endpoint, no_block)?))
        }
        sys::REPLY_RECV => {
            let a = sys::ReplyRecvArgs::decode(args)?;
            let info = message_info(a.info)?;
            let endpoint = if a.cap == NO_SLOT {
                None
            } else {
                Some(objects::endpoint(cspace, a.cap, Rights::RECEIVE)?.1)
            };
            ipc::reply(user, info, a.words)?;
            match endpoint {
                None => results(args, [0; 10]),
                Some(endpoint) => {
                    results(args, delivered(ipc::recv(me, user, &endpoint, info.no_block)?))
                }
            }
        }
        sys::NOTIFY => {
            let a = sys::NotifyArgs::decode(args)?;
            let (cap, notification) = objects::notification(cspace, a.cap, Rights::SEND)?;
            notification.notify(cap.badge());
            results(args, [0; 10])
        }
        sys::WAIT => {
            let a = sys::WaitArgs::decode(args)?;
            let (_, notification) = objects::notification(cspace, a.cap, Rights::RECEIVE)?;
            let deadline =
                (a.timeout_ns != 0).then(|| sched::now_ns().saturating_add(a.timeout_ns));
            let bits = notification.wait(a.flags.contains(WaitFlags::POLL), deadline)?;
            results(args, sys::WaitResults { bits }.encode())
        }
        sys::YIELD => {
            sys::YieldArgs::decode(args)?;
            sched::yield_now();
            results(args, [0; 10])
        }
        sys::MAP => {
            let a = sys::MapArgs::decode(args)?;
            let access = access_of(a.rights).ok_or(Kernel::InvalidArgument.status(3))?;
            let (_, space) = objects::space(cspace, a.space, Rights::CONTROL)?;
            let (_, frame) = objects::frame(cspace, a.frame, cap_rights(a.rights))?;
            FrameObject::map_into(&frame, &space, a.address, a.offset, a.pages, access)?;
            results(args, [0; 10])
        }
        sys::UNMAP => {
            let a = sys::UnmapArgs::decode(args)?;
            let (_, space) = objects::space(cspace, a.space, Rights::CONTROL)?;
            space.unmap(a.address, a.pages)?;
            results(args, [0; 10])
        }
        sys::RING_SETUP => {
            let a = sys::RingSetupArgs::decode(args)?;
            let size = super::ring::setup(user, a.space, a.address, a.entries, a.to)?;
            results(args, sys::RingSetupResults { size }.encode())
        }
        sys::RING_ENTER => {
            let a = sys::RingEnterArgs::decode(args)?;
            let (submitted, ready) =
                super::ring::enter(user, a.ring, a.submit, a.min_complete, a.timeout_ns)?;
            results(args, sys::RingEnterResults { submitted, ready }.encode())
        }
        sys::DEBUG_LOG => {
            debug_log(me, cspace, sys::DebugLogArgs::decode(args)?)?;
            results(args, [0; 10])
        }
        _ => Err(error(Kernel::NoSuchSyscall)),
    }
}

/// `debug_log`: writes the program's text to the kernel log. Kept out of
/// [`dispatch`], whose frame every system call pays for: the text is
/// copied to the stack first.
#[inline(never)]
fn debug_log(me: &ThreadRef, cspace: SpaceId, a: sys::DebugLogArgs) -> Result<()> {
    objects::lookup(cspace, a.log, ObjectKind::KernelLog, Rights::WRITE)?;
    let len = a.text.len.min(WRITE_MAX) as usize;
    let mut text = [0u8; WRITE_MAX as usize];
    if !radix_ipc::range::user_bytes(a.text.address, len as u64)
        || !Arch::copy_from_user(&mut text[..len], a.text.address)
    {
        return Err(error(Kernel::BadAddress));
    }
    let mut text = &text[..len];
    while let [rest @ .., b'\n' | b'\r'] = text {
        text = rest;
    }
    let (level, facility, text) = crate::fs::kmsg::priority_of(text);
    let process = me.user().map_or(0, |user| user.process.number());
    let name = me.name();
    // Saturating, as `/devices/kmsg`'s writes (ADR 0210).
    radix_klog::log_from(
        level,
        radix_klog::Origin { process: u32::try_from(process).unwrap_or(u32::MAX), facility },
        format_args!("{}[{}]: {}", Shown(name.as_str().as_bytes()), me.id(), Shown(text)),
    );
    Ok(())
}
