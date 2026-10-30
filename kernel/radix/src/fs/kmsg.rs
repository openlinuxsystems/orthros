// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/devices/kmsg` (Phase 9, radix-kernel-log(2)): the kernel log's records,
//! one per read, as Linux's `/dev/kmsg` gives them, for dmesg(1).
//!
//! A description of it keeps where its reader is in the log in its
//! offset: one more than the sequence number of the next record it
//! reads, or 0 before its first read, which starts at the oldest record
//! kept since the log was last cleared ([`CLEARED`]). A read waits for a
//! record; the kernel log never wakes anyone itself (a record may be
//! logged with any lock held, even in an interrupt), so a thread of its
//! own looks for new records every tick's time while a description is
//! open ([`watch`]), and wakes the readers and `poll`. The tick cannot:
//! waking takes the wait table's lock, which interrupt handlers must not
//! take while threads take it around other locks.

use core::any::Any;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use radix_abi::Status;
use radix_abi::errors::Kernel;
use radix_abi::file::Whence;
use radix_abi::kernel_log::WRITE_MAX;
use radix_klog::{Level, TextBuf, error};
use radix_vfs::{In, Kind, Node, NodeRef, Out, Stat};

use super::FileObject;
use crate::sched::{self, Options, Policy, wait};
use crate::user::posix::poll::{self, Polled};
use crate::user::posix::signal::{Restart, interrupted};

/// The first record a new reader reads: none before the last clear.
static CLEARED: AtomicU64 = AtomicU64::new(0);
/// How many descriptions of `/devices/kmsg` are open: while none is, the
/// watcher does not look.
static OPEN: AtomicU32 = AtomicU32::new(0);
/// The next record's sequence number when the watcher last looked.
static SEEN: AtomicU64 = AtomicU64::new(0);
/// The pollers of `/devices/kmsg`.
pub static POLLED: Polled = Polled::new();

/// How long the watcher waits between looks: the timer's tick.
const LOOK_NS: u64 = 10_000_000;

/// The node of `/devices/kmsg`, numbered as its id.
pub struct Kmsg(pub u64);

impl Node for Kmsg {
    fn stat(&self) -> Stat {
        Stat {
            kind: Kind::CharDevice,
            size: 0,
            allocated: 0,
            id: self.0,
            created: 0,
            modified: 0,
            accessed: 0,
            changed: 0,
            links: 1,
            device: 0,
        }
    }

    fn kind(&self) -> Kind {
        Kind::CharDevice
    }

    // The kernel's own reads and writes through the node find nothing,
    // and take nothing: the descriptions' are `read` and `write` below.
    fn read(&self, _offset: u64, _out: &mut dyn Out) -> radix_vfs::Result<usize> {
        Ok(0)
    }

    fn write(&self, _offset: u64, data: &dyn In) -> radix_vfs::Result<usize> {
        Ok(data.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Whether `node` is `/devices/kmsg`.
pub fn is_kmsg(node: &NodeRef) -> bool {
    radix_vfs::downcast::<Kmsg>(&**node).is_some()
}

/// A description of it was made: the first wakes the watcher.
pub(super) fn opened() {
    if OPEN.fetch_add(1, Ordering::AcqRel) == 0 {
        wait::wake_all(open_key());
    }
}

/// A description of it has gone.
pub(super) fn closed() {
    OPEN.fetch_sub(1, Ordering::AcqRel);
}

/// What readers wait on.
fn key() -> usize {
    core::ptr::addr_of!(SEEN).addr()
}

/// What the watcher waits on while no description is open.
fn open_key() -> usize {
    core::ptr::addr_of!(OPEN).addr()
}

/// The sequence number the next record logged will have.
fn next_seq() -> u64 {
    radix_klog::record_range().end
}

/// The next record a reader at `offset` reads: where the offset says, or
/// before its first read, the oldest kept since the last clear.
fn cursor(offset: u64) -> u64 {
    match offset {
        0 => radix_klog::record_range().start.max(CLEARED.load(Ordering::Acquire)),
        at => at - 1,
    }
}

/// A record's syslog(3) level, which leads its line.
fn priority(level: Level) -> u8 {
    match level {
        Level::Error => 3,
        Level::Warn => 4,
        Level::Info => 6,
        Level::Debug => 7,
    }
}

/// Reads the reader's next record into `out` as a line, waiting for one
/// unless `non_blocking`. Fails with invalid_argument if `out` cannot
/// hold it, and with gone once if the record was lost from the ring,
/// moving on to the oldest one kept.
pub fn read(file: &FileObject, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
    let mut offset = file.offset.lock();
    let mut seq = cursor(*offset);
    loop {
        let kept = radix_klog::record_range();
        if seq < kept.start {
            *offset = kept.start + 1;
            return Err(Kernel::Gone.status(0));
        }
        if let Some(record) = radix_klog::record(seq) {
            let mut line = TextBuf::<{ radix_klog::RECORD_TEXT + 48 }>::new();
            // A program's record is of the user facility, 1, as Linux
            // marks the lines written to its _/dev/kmsg_, and names its
            // process after the flags, as Linux's `caller=` field does.
            let _ = if record.process == 0 {
                writeln!(
                    line,
                    "{},{},{},-;{}",
                    priority(record.level),
                    record.seq,
                    record.time_ns / 1000,
                    record.text()
                )
            } else {
                writeln!(
                    line,
                    "{},{},{},-,process={};{}",
                    8 + priority(record.level),
                    record.seq,
                    record.time_ns / 1000,
                    record.process,
                    record.text()
                )
            };
            let bytes = line.as_str().as_bytes();
            if out.len() < bytes.len() {
                return Err(Kernel::InvalidArgument.status(0));
            }
            out.put(0, bytes).map_err(super::status)?;
            *offset = seq + 2;
            return Ok(bytes.len());
        }
        if seq < kept.end {
            // Overwritten as it was read: it is lost.
            seq = kept.start;
            continue;
        }
        if non_blocking {
            return Err(Kernel::WouldBlock.status(0));
        }
        wait::wait_until(key(), "kmsg", || next_seq() > seq || sched::interrupted());
        if sched::interrupted() && next_seq() <= seq {
            return Err(interrupted(Restart::Always));
        }
    }
}

/// The level a program's line asks for with `<priority>` first, as Linux
/// takes it: the facility times eight and the level (systemd writes `<30>`
/// for a daemon's information), of which only the level counts; and the
/// text after it. Information without one.
pub fn level_of(text: &[u8]) -> (Level, &[u8]) {
    if let [b'<', after @ ..] = text
        && let Some(end) = after.iter().position(|&b| b == b'>')
        && (1..=9).contains(&end)
        && after[..end].iter().all(u8::is_ascii_digit)
    {
        let priority = after[..end].iter().fold(0u32, |n, &d| n * 10 + u32::from(d - b'0'));
        let level = match priority & 7 {
            0..=3 => Level::Error,
            4 => Level::Warn,
            7 => Level::Debug,
            _ => Level::Info,
        };
        return (level, &after[end + 1..]);
    }
    (Level::Info, text)
}

/// Logs `data`, a line, as a program's: with a `<N>` before it, at
/// syslog(3) level N & 7 (N is a facility times eight plus a level).
pub fn write(data: &dyn In) -> Result<usize, Status> {
    let len = data.len();
    let mut bytes = [0u8; WRITE_MAX as usize];
    let kept = len.min(bytes.len());
    data.get(0, &mut bytes[..kept]).map_err(super::status)?;
    let mut text = &bytes[..kept];
    while let [rest @ .., b'\n' | b'\r'] = text {
        text = rest;
    }
    let (level, text) = level_of(text);
    let text = core::str::from_utf8(text).unwrap_or("<not UTF-8>");
    let me = sched::running();
    let process = me.as_ref().and_then(|me| me.user()).map_or(0, |user| user.process.number());
    let name = me.as_ref().map(|me| me.name());
    let name = name.as_ref().map_or("?", |name| name.as_str());
    // Saturating, so that no process passes for another (logd trusts
    // Spark's, ADR 0210).
    let process = u32::try_from(process).unwrap_or(u32::MAX);
    radix_klog::log_from(level, process, format_args!("{name}: {text}"));
    Ok(len)
}

/// Moves the reader: to the oldest record kept since the last clear for
/// the start, past every record for the end. Returns the new offset.
pub fn seek(file: &FileObject, offset: i64, whence: Whence) -> Result<u64, Status> {
    let mut at = file.offset.lock();
    *at = match (whence, offset) {
        (Whence::Start, 0) => 0,
        (Whence::End, 0) => next_seq() + 1,
        _ => return Err(Kernel::InvalidArgument.status(1)),
    };
    Ok(*at)
}

/// Clears the log, as `dmesg -C` does: readers that start or seek to the
/// start later skip what it holds now.
pub fn clear() {
    CLEARED.store(next_seq(), Ordering::Release);
}

/// What `poll` finds on `file`: readable while a record waits for its
/// reader.
pub fn events(file: &FileObject) -> u16 {
    // A read in progress holds the offset: it has nothing to read yet.
    let Some(offset) = file.offset.try_lock() else { return 0 };
    if next_seq() > cursor(*offset) { poll::READABLE } else { 0 }
}

/// Starts the watcher, which wakes the readers and `poll`.
pub fn start() {
    if let Err(err) =
        sched::spawn("kmsg", watch, 0, Options::new().policy(Policy::Fair { nice: 0 }))
    {
        error!("Kernel log: could not start the thread that wakes /devices/kmsg's readers: {err}");
    }
}

/// The watcher: while a description is open, looks every tick's time for
/// records logged since it last looked, and wakes the readers and `poll`
/// if there are any; while none is, it sleeps.
fn watch(_: usize) {
    loop {
        wait::wait_until(open_key(), "kmsg", || OPEN.load(Ordering::Acquire) > 0);
        let next = next_seq();
        if SEEN.swap(next, Ordering::AcqRel) != next {
            wait::wake_all(key());
            POLLED.changed();
        }
        sched::sleep_ns(LOOK_NS);
    }
}
