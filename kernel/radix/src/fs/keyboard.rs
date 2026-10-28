// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/devices/kbd` (Phase 9, radix-input(2)): the keys of the machine's
//! keyboards, as events, for consoled(8).
//!
//! The keyboard's interrupt hands each byte it sends to [`byte`], which
//! decodes it (`radix_input::Set1`) and puts the key event into the ring
//! [`RING`], numbered from boot. A description of `/devices/kbd` keeps in its
//! offset one more than the number of the next event it reads, starting
//! with the first after it was opened, so that nobody reads keys typed
//! before. The interrupt cannot wake the readers itself (waking takes the
//! wait table's lock, which interrupt handlers must not take, `kmsg`), so
//! it wakes the `kbd` thread, which wakes the readers and `poll`. The
//! thread also lights the keyboard's lock lights (SET_LEDS): each byte of
//! that command waits for the keyboard's answer, which comes back through
//! the interrupt.

use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use radix_abi::Status;
use radix_abi::errors::Kernel;
use radix_abi::input::{EVENTS_KEPT, Event};
use radix_hal::Devices;
use radix_input::Set1;
use radix_klog::{error, info};
use radix_sync::{Once, SpinLock};
use radix_vfs::{In, Kind, Node, NodeRef, Out, Stat};

use super::FileObject;
use crate::Arch;
use crate::sched::{self, Options, Policy, ThreadRef, wait};
use crate::user::posix::poll::{self, Polled};
use crate::user::posix::signal::{Restart, interrupted};

/// Events kept.
const KEPT: usize = EVENTS_KEPT as usize;

/// The keyboard's command that sets its lights, and its answers.
const SET_LIGHTS: u8 = 0xed;
const ACK: u8 = 0xfa;
const RESEND: u8 = 0xfe;

/// How long the thread waits for the keyboard to answer a byte, in
/// milliseconds.
const ANSWER_MS: u32 = 50;

/// The events, and the number of the next.
struct Ring {
    events: [(u64, u16, u16); KEPT],
    next: u64,
}

static RING: SpinLock<Ring> = SpinLock::new(Ring { events: [(0, 0, 0); KEPT], next: 0 });
/// Touched only by the interrupt, but locked as `RING` is.
static DECODER: SpinLock<Set1> = SpinLock::new(Set1::new());
/// The thread that wakes the readers.
static THREAD: Once<ThreadRef> = Once::new();
/// The number of the next event when the thread last looked.
static SEEN: AtomicU64 = AtomicU64::new(0);
/// The pollers of `/devices/kbd`.
pub static POLLED: Polled = Polled::new();
/// The lights wanted, and the lights set (`u32::MAX` before any are).
static LIGHTS: AtomicU32 = AtomicU32::new(0);
static LIGHTS_SET: AtomicU32 = AtomicU32::new(u32::MAX);
/// Set while the thread waits for the keyboard's answer, when 0xFA and
/// 0xFE are answers, not keys; and what the answer was.
static WAITING: AtomicBool = AtomicBool::new(false);
static ANSWER: AtomicU32 = AtomicU32::new(0);
/// The keyboard is ready to be sent commands.
static STARTED: AtomicBool = AtomicBool::new(false);

/// The node of `/devices/kbd`, numbered as its id.
pub struct Keyboard(pub u64);

impl Node for Keyboard {
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

    // The descriptions' reads are `read` below; the node itself holds
    // nothing, and takes nothing.
    fn read(&self, _offset: u64, _out: &mut dyn Out) -> radix_vfs::Result<usize> {
        Ok(0)
    }

    fn write(&self, _offset: u64, _data: &dyn In) -> radix_vfs::Result<usize> {
        Err(radix_vfs::Error::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Whether `node` is `/devices/kbd`.
pub fn is_keyboard(node: &NodeRef) -> bool {
    radix_vfs::downcast::<Keyboard>(&**node).is_some()
}

/// The offset a new description starts with: the next event's.
pub(super) fn opened() -> u64 {
    next() + 1
}

fn next() -> u64 {
    RING.lock_irq().next
}

/// What readers wait on.
fn key() -> usize {
    core::ptr::addr_of!(SEEN).addr()
}

/// A byte from the keyboard, in its interrupt.
fn byte(byte: u8) {
    if WAITING.load(Ordering::Acquire) && matches!(byte, ACK | RESEND) {
        ANSWER.store(u32::from(byte), Ordering::Release);
    } else {
        let Some(key) = DECODER.lock_irq().feed(byte) else { return };
        let time = sched::now_ns();
        let mut ring = RING.lock_irq();
        let at = (ring.next % KEPT as u64) as usize;
        ring.events[at] = (time, key.code, key.value);
        ring.next += 1;
    }
    if let Some(thread) = THREAD.get() {
        sched::wake(thread);
    }
}

/// Reads as many of the reader's next events as `out` holds whole, waiting
/// for one unless `non_blocking`. Fails with invalid_argument if `out`
/// cannot hold one, and with gone once if events were lost before the
/// reader read them, moving on to the oldest kept.
pub fn read(file: &FileObject, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
    let room = out.len() / Event::SIZE;
    if room == 0 {
        return Err(Kernel::InvalidArgument.status(0));
    }
    let mut offset = file.offset.lock();
    loop {
        let mut wanted = offset.saturating_sub(1);
        // A batch at a time, copied out with the ring let go of: the copy
        // may wait for a page.
        let mut batch = [(0u64, 0u16, 0u16); 32];
        let (count, lost) = {
            let ring = RING.lock_irq();
            let oldest = ring.next.saturating_sub(KEPT as u64);
            let lost = wanted < oldest;
            wanted = wanted.max(oldest);
            let count = (ring.next - wanted).min(room.min(batch.len()) as u64) as usize;
            for (i, slot) in batch[..count].iter_mut().enumerate() {
                *slot = ring.events[((wanted + i as u64) % KEPT as u64) as usize];
            }
            (count, lost)
        };
        if lost {
            *offset = wanted + 1;
            return Err(Kernel::Gone.status(0));
        }
        if count > 0 {
            for (i, &(time_ns, code, value)) in batch[..count].iter().enumerate() {
                let mut bytes = [0u8; Event::SIZE];
                bytes[Event::OFFSET_TIME_NS..][..8].copy_from_slice(&time_ns.to_le_bytes());
                bytes[Event::OFFSET_CODE..][..2].copy_from_slice(&code.to_le_bytes());
                bytes[Event::OFFSET_VALUE..][..2].copy_from_slice(&value.to_le_bytes());
                out.put(i * Event::SIZE, &bytes).map_err(super::status)?;
            }
            *offset = wanted + count as u64 + 1;
            return Ok(count * Event::SIZE);
        }
        if non_blocking {
            return Err(Kernel::WouldBlock.status(0));
        }
        wait::wait_until(key(), "kbd", || next() > wanted || sched::interrupted());
        if sched::interrupted() && next() <= wanted {
            return Err(interrupted(Restart::Always));
        }
    }
}

/// What `poll` finds on `file`: readable while an event waits for it.
pub fn events(file: &FileObject) -> u16 {
    // A read in progress holds the offset: it has nothing to read yet.
    let Some(offset) = file.offset.try_lock() else { return 0 };
    if next() > offset.saturating_sub(1) { poll::READABLE } else { 0 }
}

/// SET_LEDS: lights the lock lights `lights` names.
pub fn set_lights(lights: u32) {
    LIGHTS.store(lights & 7, Ordering::Release);
    if let Some(thread) = THREAD.get() {
        sched::wake(thread);
    }
}

/// Starts the thread, then the keyboard.
pub fn start() {
    let made = sched::spawn("kbd", run, 0, Options::new().policy(Policy::Fair { nice: -5 }));
    match made {
        Ok(thread) => {
            let _ = THREAD.set(thread);
        }
        Err(err) => {
            error!("Keyboard: could not start the thread that wakes /devices/kbd's readers: {err}");
            return;
        }
    }
    match Arch::start_keyboard(byte) {
        Some(name) => {
            info!("Keyboard: {name} reads into /devices/kbd");
            STARTED.store(true, Ordering::Release);
            if let Some(thread) = THREAD.get() {
                sched::wake(thread);
            }
        }
        None => info!("Keyboard: no built-in keyboard"),
    }
}

/// Sends `byte` to the keyboard and waits for its answer: whether it took
/// it.
fn send(byte: u8) -> bool {
    for _ in 0..3 {
        ANSWER.store(0, Ordering::Release);
        WAITING.store(true, Ordering::Release);
        if !Arch::keyboard_send(byte) {
            break;
        }
        let mut answer = 0;
        for _ in 0..ANSWER_MS {
            answer = ANSWER.load(Ordering::Acquire);
            if answer != 0 {
                break;
            }
            sched::sleep_ns(1_000_000);
        }
        WAITING.store(false, Ordering::Release);
        match answer as u8 {
            ACK => return true,
            // Asked for it again.
            RESEND => {}
            _ => break,
        }
    }
    WAITING.store(false, Ordering::Release);
    false
}

/// The thread: wakes the readers and `poll` when events came, and sets the
/// lights when they are to change.
fn run(_: usize) {
    loop {
        let next = next();
        if SEEN.swap(next, Ordering::AcqRel) != next {
            wait::wake_all(key());
            POLLED.changed();
        }
        let lights = LIGHTS.load(Ordering::Acquire);
        if STARTED.load(Ordering::Acquire) && LIGHTS_SET.load(Ordering::Acquire) != lights {
            // Set even if the keyboard does not answer, so that a keyboard
            // without lights is not asked again and again.
            LIGHTS_SET.store(lights, Ordering::Release);
            if !(send(SET_LIGHTS) && send(lights as u8)) {
                info!("Keyboard: the keyboard did not take the lock lights");
            }
        }
        sched::block("kbd");
    }
}
