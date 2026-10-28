// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/devices/kbd` (Phase 9, radix-input(2)): the keys of the machine's
//! keyboards, as events, for consoled(8).
//!
//! The PS/2 keyboard's interrupt hands each byte it sends to [`byte`],
//! which decodes it (`radix_input::Set1`) and puts the key event into the
//! ring [`EVENTS`] (`events`). The interrupt cannot wake the readers itself
//! (waking takes the wait table's lock, which interrupt handlers must not
//! take, `kmsg`), so it wakes the `kbd` thread, which wakes the readers and
//! `poll`. The thread also lights the keyboard's lock lights (SET_LEDS):
//! each byte of that command waits for the keyboard's answer, which comes
//! back through the interrupt.
//!
//! USB keyboards (Phase 11a, ADR 0148) hand their keys to [`usb_key`]
//! from their controller's thread. A PS/2 keyboard repeats a key held down
//! itself, a USB one does not, so the `kbd` thread repeats the last key a
//! USB keyboard pressed, as Linux's input core does: 250 ms after it went
//! down, then every 33 ms, until a key goes up. The controllers' threads
//! light their keyboards' lights from [`lights`].

use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use radix_abi::Status;
use radix_abi::input::{EVENTS_KEPT, Event, PRESSED, RELEASED, REPEATED};
use radix_hal::Devices;
use radix_input::Set1;
use radix_klog::{error, info};
use radix_sync::{Once, SpinLock};
use radix_vfs::{In, Kind, Node, NodeRef, Out, Stat};

use super::FileObject;
use super::events::{Events, Record};
use crate::Arch;
use crate::sched::{self, Options, Policy, ThreadRef, wait};
use crate::user::posix::poll::{self, Polled};

/// Events kept.
const KEPT: usize = EVENTS_KEPT as usize;

/// The keyboard's command that sets its lights, and its answers.
const SET_LIGHTS: u8 = 0xed;
const ACK: u8 = 0xfa;
const RESEND: u8 = 0xfe;

/// How long the thread waits for the keyboard to answer a byte, in
/// milliseconds.
const ANSWER_MS: u32 = 50;

/// How long a USB keyboard's key is held before it repeats, and how often
/// it repeats then: Linux's defaults.
const REPEAT_DELAY_NS: u64 = 250_000_000;
const REPEAT_PERIOD_NS: u64 = 33_000_000;

/// A key event: when, the key, and what it did.
#[derive(Clone, Copy)]
struct Key {
    time_ns: u64,
    code: u16,
    value: u16,
}

impl Record for Key {
    const SIZE: usize = Event::SIZE;

    fn encode(&self, out: &mut [u8]) {
        out.fill(0);
        out[Event::OFFSET_TIME_NS..][..8].copy_from_slice(&self.time_ns.to_le_bytes());
        out[Event::OFFSET_CODE..][..2].copy_from_slice(&self.code.to_le_bytes());
        out[Event::OFFSET_VALUE..][..2].copy_from_slice(&self.value.to_le_bytes());
    }
}

static EVENTS: Events<Key, KEPT> = Events::new(Key { time_ns: 0, code: 0, value: 0 });
/// Touched only by the interrupt, but locked: it may come on any CPU.
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
/// The key a USB keyboard holds down, which the thread repeats, and when
/// it next does.
static REPEAT: SpinLock<Option<(u16, u64)>> = SpinLock::new(None);

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
    EVENTS.opened()
}

/// What readers wait on.
fn key() -> usize {
    core::ptr::addr_of!(SEEN).addr()
}

/// Wakes the thread, which wakes the readers.
fn wake_thread() {
    if let Some(thread) = THREAD.get() {
        sched::wake(thread);
    }
}

/// A byte from the keyboard, in its interrupt.
fn byte(byte: u8) {
    if WAITING.load(Ordering::Acquire) && matches!(byte, ACK | RESEND) {
        ANSWER.store(u32::from(byte), Ordering::Release);
    } else {
        let Some(key) = DECODER.lock_irq().feed(byte) else { return };
        EVENTS.push(Key { time_ns: sched::now_ns(), code: key.code, value: key.value });
    }
    wake_thread();
}

/// A USB keyboard's key `code` went down, or up: from its controller's
/// thread.
pub fn usb_key(code: u16, down: bool) {
    let now = sched::now_ns();
    EVENTS.push(Key { time_ns: now, code, value: if down { PRESSED } else { RELEASED } });
    // A key going down repeats; any going up stops it, as on Linux.
    *REPEAT.lock_irq() = down.then_some((code, now + REPEAT_DELAY_NS));
    wake_thread();
}

/// The lock lights consoled(8) last asked for: LED_* bits.
pub fn lights() -> u32 {
    LIGHTS.load(Ordering::Acquire)
}

/// Reads as many of the reader's next events as `out` holds whole, waiting
/// for one unless `non_blocking`. Fails with invalid_argument if `out`
/// cannot hold one, and with gone once if events were lost before the
/// reader read them, moving on to the oldest kept.
pub fn read(file: &FileObject, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
    EVENTS.read(file, out, non_blocking, key(), "kbd")
}

/// What `poll` finds on `file`: readable while an event waits for it.
pub fn events(file: &FileObject) -> u16 {
    if EVENTS.readable(file) { poll::READABLE } else { 0 }
}

/// SET_LEDS: lights the lock lights `lights` names.
pub fn set_lights(lights: u32) {
    LIGHTS.store(lights & 7, Ordering::Release);
    wake_thread();
    crate::usb::lights_changed();
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
            wake_thread();
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

/// Repeats the key a USB keyboard holds, if it is time; returns when it
/// is next to, if one is held.
fn repeat() -> Option<u64> {
    let now = sched::now_ns();
    let mut repeat = REPEAT.lock_irq();
    let (code, at) = (*repeat)?;
    if now < at {
        return Some(at);
    }
    EVENTS.push(Key { time_ns: now, code, value: REPEATED });
    // Late, it goes on from now: no burst to catch up.
    let next = (at + REPEAT_PERIOD_NS).max(now + 1);
    *repeat = Some((code, next));
    Some(next)
}

/// The thread: wakes the readers and `poll` when events came, sets the
/// lights when they are to change, and repeats a USB keyboard's key.
fn run(_: usize) {
    loop {
        let repeat_at = repeat();
        let next = EVENTS.next();
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
        match repeat_at {
            Some(at) => sched::block_until(at, "kbd"),
            None => sched::block("kbd"),
        }
    }
}
