// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tracing (Phase 9, ADR 0095, radix-trace(2)): what a process asks of
//! the kernel, as records a tracer reads from a pipe the kernel writes.
//!
//! radix-process(2)'s `trace` makes a [`Trace`] and has the process
//! follow it ([`start`]). From then on each system call of the process's
//! threads takes the slow way through `user::syscall`, with [`begin`]
//! and [`end`] around it, and the places that fork, start programs,
//! deliver signals and end processes add their events ([`forked`],
//! [`exec`], [`signal`], [`exited`]). Only a traced process pays: the
//! system call path looks at one flag of the process.
//!
//! A record is written by the thread it tells of, as one write of at most
//! TRACE_RECORD_MAX bytes, which the pipe takes whole ([`pipe::ATOMIC`]).
//! A thread waits for room as a pipe's writer does, but leaves [`KEEP`]
//! bytes free, which the records that never wait use: those of a
//! process's end (written by whatever finishes it, perhaps the tracer
//! itself, which must not wait for its own reading) and of a signal that
//! ends or stops it.
//!
//! A process leaves the trace when it ends, or when one of its records
//! finds that no one reads the pipe. The trace goes once no process
//! follows it, and the pipe's writing end with it, so the tracer reads
//! the end of the file.

use core::sync::atomic::{AtomicBool, Ordering};

use radix_abi::Status;
use radix_abi::catalog;
use radix_abi::errors::Kernel;
use radix_abi::syscalls as sys;
use radix_abi::trace::{
    TRACE_DATA_MAX, TRACE_RECORD_MAX, TraceEvent, TraceFlags, TraceRecord, TraceRecordFlags,
    TraceSignal,
};
use radix_cap::{ObjectKind, Rights, SlotRef};
use radix_hal::UserMode;
use radix_mm::Kmalloc;
use radix_vfs::{KArc, Slice};

use super::pipe::{self, Pipe};
use crate::Arch;
use crate::sched;
use crate::user::UserRef;
use crate::user::process::{ProcessObject, ProcessRef};

/// The bytes a waiting record leaves free in the pipe: room for a record
/// that never waits, with no data.
const KEEP: usize = TraceRecord::SIZE;

/// The object kind of a record of a call with no capability.
const NO_OBJECT: u32 = 0xff;

/// A trace.
pub struct Trace {
    /// The pipe its records go through. Its one writing end is the
    /// trace's, closed when the trace goes.
    pipe: KArc<Pipe>,
    /// What it follows.
    flags: TraceFlags,
    /// Whether a record was lost since the last one written.
    lost: AtomicBool,
}

/// A trace, shared by the processes that follow it.
pub type TraceRef = alloc::sync::Arc<Trace, Kmalloc>;

impl Trace {
    /// Whether anyone can still read it.
    fn live(&self) -> bool {
        self.pipe.readers() > 0
    }

    /// Whether it follows the children of the processes it follows.
    pub fn follows_children(&self) -> bool {
        self.flags.contains(TraceFlags::CHILDREN)
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        self.pipe.close_writer();
    }
}

/// The running thread's number.
fn running_id() -> u64 {
    sched::running().map_or(0, |me| me.id())
}

/// The heap a trace's memory comes from.
fn heap() -> Kmalloc {
    radix_mm::HEAP.with(radix_mm::Flags::MAY_SLEEP)
}

/// radix-process(2)'s `trace`, called by `user`'s thread: `target`
/// follows a new trace with `flags`, whose file goes in `user`'s slot
/// `to`.
pub fn start(
    user: &UserRef,
    target: &ProcessRef,
    flags: TraceFlags,
    to: u32,
) -> Result<(), Status> {
    if ProcessRef::ptr_eq(target, &user.process) {
        return Err(Kernel::InvalidArgument.status(0));
    }
    if target.with_trace(|trace| trace.as_ref().is_some_and(|trace| trace.live())) {
        return Err(Kernel::Busy.status(0));
    }
    let (pipe, reader) = pipe::for_kernel()?;
    let trace =
        alloc::sync::Arc::try_new_in(Trace { pipe, flags, lost: AtomicBool::new(false) }, heap())
            .map_err(|_| Kernel::NoMemory.status(0))?;
    let slot = SlotRef::new(user.process.cspace(), to);
    let rights = Rights::READ.union(Rights::COPY).union(Rights::TRANSFER);
    crate::user::files::install_file(user, slot, ObjectKind::File, rights, reader, false)?;
    // Another tracer may have come first meanwhile.
    let old = target.with_trace(|current| {
        if current.as_ref().is_some_and(|trace| trace.live()) {
            Err(())
        } else {
            Ok(current.replace(trace.clone()))
        }
    });
    let Ok(old) = old else {
        let _ = crate::caps::delete(slot);
        return Err(Kernel::Busy.status(0));
    };
    drop(old);
    let thread = target.number();
    event(target, &trace, thread, TraceEvent::Attach, 0, 0, &[], false);
    Ok(())
}

/// The trace that follows `process`, if one does.
fn trace_of(process: &ProcessObject) -> Option<TraceRef> {
    if !process.traced() {
        return None;
    }
    process.with_trace(|trace| trace.clone())
}

/// `process` leaves `trace`, if it still follows it.
fn leave(process: &ProcessObject, trace: &TraceRef) {
    let gone = process.with_trace(|current| match current {
        Some(now) if TraceRef::ptr_eq(now, trace) => current.take(),
        _ => None,
    });
    drop(gone);
}

/// What [`begin`] noted of a system call for [`end`].
pub struct Begun {
    trace: TraceRef,
    number: u64,
    args: [u64; 10],
    time_ns: u64,
    object: u32,
    flags: TraceRecordFlags,
    /// The record so far: room for its header, then its input data.
    bytes: alloc::vec::Vec<u8, Kmalloc>,
}

/// The byte arguments of system call `number` with `args`: for each, the
/// register its address is in (its length is in the next), and whether
/// the kernel writes it.
fn byte_args(number: u64, args: &[u64; 10]) -> (usize, &'static [(u8, bool)]) {
    if number == sys::CAP_CALL {
        // A method's words start at the third register, and its label is
        // the info word's low half.
        (2, catalog::method_bytes(args[1] as u32))
    } else {
        (0, catalog::syscall_bytes(number))
    }
}

/// Copies `len` bytes of the running program's memory at `address` to
/// the end of `bytes`; `false`, with `bytes` as it was, if they cannot be
/// read.
fn copy_in(bytes: &mut alloc::vec::Vec<u8, Kmalloc>, address: u64, len: usize) -> bool {
    let at = bytes.len();
    if at + len > bytes.capacity() || !radix_ipc::range::user_bytes(address, len as u64) {
        return false;
    }
    while bytes.len() < at + len {
        // Room was checked above.
        let _ = bytes.push_within_capacity(0);
    }
    if !Arch::copy_from_user(&mut bytes[at..], address) {
        bytes.truncate(at);
        return false;
    }
    true
}

/// A record's bytes, with room for its header: `None`, and a record
/// lost, if there is no memory for them.
fn new_record(trace: &Trace) -> Option<alloc::vec::Vec<u8, Kmalloc>> {
    let mut bytes = alloc::vec::Vec::new_in(heap());
    if bytes.try_reserve_exact(TRACE_RECORD_MAX as usize).is_err() {
        trace.lost.store(true, Ordering::Relaxed);
        return None;
    }
    while bytes.len() < TraceRecord::SIZE {
        // Room was reserved above.
        let _ = bytes.push_within_capacity(0);
    }
    Some(bytes)
}

/// A traced process's thread begins system call `number` with `args`:
/// notes what its record needs from now. `None` if the process no longer
/// follows a trace, or there is no memory for the record.
pub fn begin(user: &UserRef, number: u64, args: &[u64; 10]) -> Option<Begun> {
    let trace = trace_of(&user.process)?;
    let mut bytes = new_record(&trace)?;
    let object = if number == sys::YIELD {
        NO_OBJECT
    } else {
        // A slot number that does not fit is refused by the call itself.
        u32::try_from(args[0])
            .ok()
            .and_then(|slot| crate::caps::get(SlotRef::new(user.process.cspace(), slot)).ok())
            .map_or(NO_OBJECT, |cap| cap.kind() as u32)
    };
    let mut flags = TraceRecordFlags::NONE;
    if trace.flags.contains(TraceFlags::DATA) {
        let (base, bytes_args) = byte_args(number, args);
        for &(word, _) in bytes_args.iter().filter(|(_, writes)| !writes) {
            let at = base + usize::from(word);
            let (address, len) = (args[at], args[at + 1]);
            if len > TRACE_DATA_MAX {
                flags = flags.union(TraceRecordFlags::INPUT_CUT);
            }
            if !copy_in(&mut bytes, address, len.min(TRACE_DATA_MAX) as usize) {
                break;
            }
        }
    }
    Some(Begun { trace, number, args: *args, time_ns: sched::now_ns(), object, flags, bytes })
}

/// The call [`begin`] noted returned with `status` (0 for success) and
/// `results`: writes its record, waiting for room.
pub fn end(user: &UserRef, begun: Begun, status: u64, results: &[u64; 10]) {
    let Begun { trace, number, args, time_ns, object, mut flags, mut bytes } = begun;
    let duration_ns = sched::now_ns().saturating_sub(time_ns);
    let data_in = bytes.len() - TraceRecord::SIZE;
    let label = if number == sys::CAP_CALL { args[1] as u32 } else { 0 };
    if status == 0 && number == sys::CAP_CALL && trace.flags.contains(TraceFlags::DATA) {
        // What a method wrote: as much of its first output argument as
        // its count of bytes written says.
        let output = catalog::method_bytes(label).iter().find(|(_, writes)| *writes);
        if let (Some(&(word, _)), Some(count)) = (output, catalog::method_written(label)) {
            let at = 2 + usize::from(word);
            let (address, len) = (args[at], args[at + 1]);
            let written = results[2 + usize::from(count)].min(len);
            if written > TRACE_DATA_MAX {
                flags = flags.union(TraceRecordFlags::OUTPUT_CUT);
            }
            copy_in(&mut bytes, address, written.min(TRACE_DATA_MAX) as usize);
        }
    }
    let data_out = bytes.len() - TraceRecord::SIZE - data_in;
    let thread = running_id();
    let header = Header {
        event: TraceEvent::Call,
        flags,
        process: user.process.number(),
        thread,
        time_ns,
        duration_ns,
        number: number as u32,
        label,
        object,
        data: (data_in, data_out),
        status,
        value: 0,
    };
    header.put(&mut bytes, &args, results);
    write(&user.process, &trace, bytes, true);
}

/// A record's header, but for its arguments and results.
struct Header {
    event: TraceEvent,
    flags: TraceRecordFlags,
    process: u64,
    thread: u64,
    time_ns: u64,
    duration_ns: u64,
    number: u32,
    label: u32,
    object: u32,
    /// The bytes of input and output data.
    data: (usize, usize),
    status: u64,
    value: u64,
}

impl Header {
    /// Writes it, with `args` and `results`, at the start of `bytes`.
    fn put(&self, bytes: &mut [u8], args: &[u64; 10], results: &[u64; 10]) {
        let mut set = |offset: usize, value: &[u8]| {
            bytes[offset..offset + value.len()].copy_from_slice(value);
        };
        set(TraceRecord::OFFSET_EVENT, &self.event.raw().to_le_bytes());
        set(TraceRecord::OFFSET_FLAGS, &(self.flags.bits() as u16).to_le_bytes());
        set(TraceRecord::OFFSET_PROCESS, &self.process.to_le_bytes());
        set(TraceRecord::OFFSET_THREAD, &self.thread.to_le_bytes());
        set(TraceRecord::OFFSET_TIME_NS, &self.time_ns.to_le_bytes());
        set(TraceRecord::OFFSET_DURATION_NS, &self.duration_ns.to_le_bytes());
        set(TraceRecord::OFFSET_NUMBER, &self.number.to_le_bytes());
        set(TraceRecord::OFFSET_LABEL, &self.label.to_le_bytes());
        set(TraceRecord::OFFSET_OBJECT, &self.object.to_le_bytes());
        set(TraceRecord::OFFSET_DATA_IN, &(self.data.0 as u32).to_le_bytes());
        set(TraceRecord::OFFSET_DATA_OUT, &(self.data.1 as u32).to_le_bytes());
        set(TraceRecord::OFFSET_STATUS, &self.status.to_le_bytes());
        set(TraceRecord::OFFSET_VALUE, &self.value.to_le_bytes());
        for (i, (arg, result)) in args.iter().zip(results).enumerate() {
            set(TraceRecord::OFFSET_ARGS + 8 * i, &arg.to_le_bytes());
            set(TraceRecord::OFFSET_RESULTS + 8 * i, &result.to_le_bytes());
        }
    }
}

/// Writes `bytes`, a record with its header, to `trace`'s pipe for
/// `process`: padded to 8 bytes, with its size, and its `lost` flag if a
/// record was lost before it. With `wait`, waits for room, leaving
/// [`KEEP`] bytes free; without, a record that does not fit is lost. A
/// pipe no one reads ends the trace for `process`.
fn write(
    process: &ProcessObject,
    trace: &TraceRef,
    mut bytes: alloc::vec::Vec<u8, Kmalloc>,
    wait: bool,
) {
    while !bytes.len().is_multiple_of(8) {
        // The record's room is a multiple of 8 bytes.
        let _ = bytes.push_within_capacity(0);
    }
    let size = bytes.len() as u32;
    bytes[TraceRecord::OFFSET_SIZE..TraceRecord::OFFSET_SIZE + 4]
        .copy_from_slice(&size.to_le_bytes());
    let lost = trace.lost.swap(false, Ordering::Relaxed);
    if lost {
        let at = TraceRecord::OFFSET_FLAGS;
        let flags =
            u16::from_le_bytes([bytes[at], bytes[at + 1]]) | TraceRecordFlags::LOST.bits() as u16;
        bytes[at..at + 2].copy_from_slice(&flags.to_le_bytes());
    }
    let keep = if wait { KEEP } else { 0 };
    match trace.pipe.write_keeping(&Slice(&bytes), keep, !wait) {
        Ok(_) => {}
        Err(status) if is_gone(status) => leave(process, trace),
        Err(_) => trace.lost.store(true, Ordering::Relaxed),
    }
}

/// Whether `status` is gone: a pipe no one reads.
fn is_gone(status: Status) -> bool {
    status.domain() == Kernel::DOMAIN && status.code() == Kernel::Gone as u16
}

/// Writes an event's record for `process`'s thread `thread`, with
/// `number` and `value`, and `data` (cut to TRACE_DATA_MAX bytes) as its
/// input data; waiting for room if `wait`.
#[allow(clippy::too_many_arguments)]
fn event(
    process: &ProcessObject,
    trace: &TraceRef,
    thread: u64,
    event: TraceEvent,
    number: u32,
    value: u64,
    data: &[u8],
    wait: bool,
) {
    let Some(mut bytes) = new_record(trace) else { return };
    let data = &data[..data.len().min(TRACE_DATA_MAX as usize)];
    for &byte in data {
        // TRACE_RECORD_MAX bytes were reserved.
        let _ = bytes.push_within_capacity(byte);
    }
    let header = Header {
        event,
        flags: TraceRecordFlags::NONE,
        process: process.number(),
        thread,
        time_ns: sched::now_ns(),
        duration_ns: 0,
        number,
        label: 0,
        object: NO_OBJECT,
        data: (data.len(), 0),
        status: 0,
        value,
    };
    header.put(&mut bytes, &[0; 10], &[0; 10]);
    write(process, trace, bytes, wait);
}

/// `parent`'s running thread made `child` (`fork`, or `exec` of a process
/// `create` made), which has not started: if a trace follows `parent`,
/// it hears of the child, and if it follows children, the child follows
/// it too.
pub fn forked(parent: &ProcessObject, child: &ProcessObject) {
    let Some(trace) = trace_of(parent) else { return };
    if trace.follows_children() {
        let old = child.with_trace(|current| current.replace(trace.clone()));
        drop(old);
        event(child, &trace, child.number(), TraceEvent::Attach, 0, 0, &[], true);
    }
    let thread = running_id();
    event(parent, &trace, thread, TraceEvent::Fork, 0, child.number(), &[], true);
}

/// `process`'s thread `thread` started the program at `path`, as the
/// C library named it.
pub fn exec(process: &ProcessObject, thread: u64, path: &[u8]) {
    if let Some(trace) = trace_of(process) {
        event(process, &trace, thread, TraceEvent::Exec, 0, 0, path, true);
    }
}

/// `process`'s thread `thread` took `signal`, and `what` was done with
/// it. A signal that ends or stops the process does not wait for room.
pub fn signal(process: &ProcessObject, thread: u64, signal: u32, what: TraceSignal) {
    if let Some(trace) = trace_of(process) {
        let wait = !matches!(what, TraceSignal::Ended | TraceSignal::Stopped);
        event(process, &trace, thread, TraceEvent::Signal, signal, what.raw(), &[], wait);
    }
}

/// `process` ended with wait status `status`: its last record, which does
/// not wait for room, and it leaves its trace.
pub fn exited(process: &ProcessObject, status: u32) {
    let Some(trace) = trace_of(process) else { return };
    let thread = process.number();
    event(process, &trace, thread, TraceEvent::Exit, 0, u64::from(status), &[], false);
    leave(process, &trace);
}
