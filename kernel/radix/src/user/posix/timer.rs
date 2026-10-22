// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Timers (Phase 8): the three interval timers of POSIX's `setitimer`
//! each process has (radix-process(2)'s `set_timer`), real time
//! (SIGALRM), user time (SIGVTALRM) and profiling time (SIGPROF); and the
//! POSIX timers of `timer_create` (`timer_create` to `timer_delete`).
//!
//! A timer holds when it expires, on the kernel's clock or on its
//! process's CPU time, and its interval. One kernel thread runs them all:
//! it sleeps until the next timer on the kernel's clock is due, or for
//! [`CPU_TICK_NS`] while a CPU-time timer is armed, since CPU time is only
//! counted at ticks; then it sends the signal of each timer that has
//! expired. The processes with an armed timer are listed in [`ARMED`], by
//! weak references, so a process that has gone drops out of the list.
//!
//! An interval timer is set again by its interval at once (an expiry
//! missed while the signal was pending counts once). A POSIX timer with
//! an interval, as on Linux, waits until its signal is taken: then the
//! expiries missed meanwhile become the signal's overrun, and it is set
//! for the next one ([`taken`]). A POSIX timer's signal goes when the
//! timer is deleted; one dropped while pending (ignored, or discarded by
//! SIGCONT or a stop) sets its timer again ([`discarded`]).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::errors::Kernel;
use radix_abi::process::{self, ClockKind, TIMERS_MAX, TimerKind, TimerNotify};
use radix_abi::signal::{SIGALRM, SIGNALS, SIGPROF, SIGVTALRM};
use radix_klog::error;
use radix_mm::Kmalloc;
use radix_posix::signal::{Info, SI_KERNEL, SI_TIMER, bit};
use radix_sync::{Mutex, MutexGuard};

use super::signal::Sent;
use super::{PosixState, cpu_ns, find_thread, kmalloc, signal};
use crate::Arch;
use crate::sched::{self, Options, Policy, wait};
use crate::user::methods::{Answer, answer};
use crate::user::process::{ProcessObject, ProcessRef};
use crate::user::{Result, UserRef, error};

/// How often CPU-time timers are looked at while one is armed.
const CPU_TICK_NS: u64 = 10_000_000;

/// A timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timer {
    /// When it expires (0: never): on the kernel's clock for the real
    /// timer, on its process's CPU time for the others.
    pub expires: u64,
    /// How long after expiring it expires again (0: it does not).
    pub interval: u64,
}

impl Timer {
    /// A timer that is not set.
    pub const OFF: Timer = Timer { expires: 0, interval: 0 };

    /// Set to expire next after `now`, by its interval from when it last
    /// expired, but not before `soonest`; off if it has no interval.
    /// Returns how many expiries it missed.
    fn next_after(&mut self, now: u64, soonest: u64) -> u64 {
        let Some(missed) = now.saturating_sub(self.expires).checked_div(self.interval) else {
            *self = Timer::OFF;
            return 0;
        };
        let expires = self.expires.saturating_add((missed + 1).saturating_mul(self.interval));
        self.expires = expires.max(soonest);
        missed
    }
}

/// The least a POSIX timer whose signal was dropped waits to expire
/// again, so that one with a short interval does not keep the timer
/// thread busy.
const DROPPED_NS: u64 = 1_000_000;

/// The largest overrun a signal reports (DELAYTIMER_MAX).
const OVERRUN_MAX: u64 = i32::MAX as u64;

/// A POSIX timer (`timer_create`).
#[derive(Clone, Copy, Debug)]
pub struct PosixTimer {
    /// The clock it was made on.
    clock: ClockKind,
    /// What it does when it expires.
    notify: TimerNotify,
    /// Its signal.
    signal: u32,
    /// What its signal carries.
    value: u64,
    /// The thread it signals, for TimerNotify::Thread.
    thread: u64,
    /// When it expires, on the kernel's clock, or on the process's CPU
    /// time for a process clock.
    timer: Timer,
    /// Its signal is pending; with an interval, it waits for the signal
    /// to be taken.
    fired: bool,
    /// The overrun of its signal last taken.
    overrun: u32,
}

impl PosixTimer {
    /// Whether it counts its process's CPU time.
    fn on_cpu_time(&self) -> bool {
        self.clock == ClockKind::Process
    }

    /// Whether its process must stay in [`ARMED`]: it is set, and sends
    /// a signal when it expires.
    fn set(&self) -> bool {
        self.timer.expires != 0 && self.notify != TimerNotify::None
    }

    /// What its signal carries, before its overrun is known.
    fn info(&self, number: usize) -> Info {
        Info { code: SI_TIMER, pid: number as u64, status: self.value }
    }

    /// For one that does nothing when it expires: set again past `now`,
    /// as if it had expired on time.
    fn catch_up(&mut self, now: u64) {
        if self.notify == TimerNotify::None && self.timer.expires != 0 && now >= self.timer.expires
        {
            self.timer.next_after(now, 0);
        }
    }
}

/// The processes with a timer set, which the thread looks at. Its lock is
/// taken before a process's POSIX state, and never with the tree's held.
static ARMED: Mutex<Option<Vec<Weak<ProcessObject, Kmalloc>, Kmalloc>>> = Mutex::new(None);

/// Moves whenever a timer is set, so that the thread looks again.
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// Whether the thread has been started.
static STARTED: AtomicBool = AtomicBool::new(false);

fn changes_key() -> usize {
    &CHANGES as *const AtomicU64 as usize
}

/// The signal interval timer `index` sends.
const ITIMER_SIGNALS: [u32; 3] = [SIGALRM, SIGVTALRM, SIGPROF];

/// What timer `index` of `process` counts now: the kernel's clock, or the
/// process's CPU time.
fn now_for(process: &ProcessObject, index: usize) -> u64 {
    if index == 0 { sched::now_ns() } else { cpu_ns(process) }
}

/// What POSIX timer `timer` of `process` counts now.
fn posix_now(process: &ProcessObject, timer: &PosixTimer) -> u64 {
    if timer.on_cpu_time() { cpu_ns(process) } else { sched::now_ns() }
}

/// Sets `process`'s timer `kind` to expire in `value_ns` (0: never), then
/// every `interval_ns`, and returns what was left of it and its interval.
pub fn set(
    process: &ProcessRef,
    kind: TimerKind,
    value_ns: u64,
    interval_ns: u64,
) -> Result<(u64, u64)> {
    let index = kind as usize;
    if index >= ITIMER_SIGNALS.len() {
        return Err(Kernel::InvalidArgument.status(0));
    }
    let armed = if value_ns != 0 {
        start()?;
        Some(register(process)?)
    } else {
        None
    };
    let now = now_for(process, index);
    let old = {
        let mut posix = process.posix.lock();
        let timer = &mut posix.timers[index];
        let old = *timer;
        *timer = if value_ns == 0 {
            Timer::OFF
        } else {
            Timer { expires: now.saturating_add(value_ns).max(1), interval: interval_ns }
        };
        old
    };
    drop(armed);
    changed();
    // A timer due now is still a little way off.
    let left = if old.expires == 0 { 0 } else { old.expires.saturating_sub(now).max(1) };
    Ok((left, old.interval))
}

/// Has the timer thread look at the timers again.
fn changed() {
    CHANGES.fetch_add(1, Ordering::AcqRel);
    wait::wake_all(changes_key());
}

/// The POSIX timer methods of radix-process(2), called by `user`'s
/// thread.
#[inline(never)]
pub fn method(user: &UserRef, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let process = &user.process;
    match label {
        process::TIMER_CREATE => {
            let a = process::TimerCreateArgs::decode(args)?;
            let timer = create(process, a.clock, a.notify, a.signal, a.value, a.thread)?;
            let results = process::TimerCreateResults { timer };
            answer(label, process::TimerCreateResults::WORDS, results.encode())
        }
        process::TIMER_SET => {
            let a = process::TimerSetArgs::decode(args)?;
            let (value_ns, interval_ns) =
                set_posix(process, a.timer, a.absolute, a.value_ns, a.interval_ns)?;
            let results = process::TimerSetResults { value_ns, interval_ns };
            answer(label, process::TimerSetResults::WORDS, results.encode())
        }
        process::TIMER_GET => {
            let a = process::TimerGetArgs::decode(args)?;
            let mut posix = process.posix.lock();
            let timer = posix_timer(&mut posix, a.timer)?;
            let now = posix_now(process, timer);
            let (value_ns, interval_ns) = left(timer, now);
            let results =
                process::TimerGetResults { value_ns, interval_ns, overrun: timer.overrun };
            drop(posix);
            answer(label, process::TimerGetResults::WORDS, results.encode())
        }
        process::TIMER_DELETE => {
            let a = process::TimerDeleteArgs::decode(args)?;
            let mut posix = process.posix.lock();
            posix_timer(&mut posix, a.timer)?;
            delete(process, &mut posix, a.timer as usize);
            drop(posix);
            answer(label, 0, [0; 8])
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// POSIX timer `number` of `posix`'s, if it has one.
fn posix_timer(posix: &mut PosixState, number: u32) -> Result<&mut PosixTimer> {
    posix
        .posix_timers
        .get_mut(number as usize)
        .and_then(Option::as_mut)
        .ok_or(Kernel::InvalidArgument.status(0))
}

/// `timer_create`: makes a POSIX timer of `process`'s and returns its
/// number.
fn create(
    process: &ProcessRef,
    clock: ClockKind,
    notify: TimerNotify,
    signal: u32,
    value: u64,
    thread: u64,
) -> Result<u32> {
    // The clocks Linux has timers on, but the thread's CPU time.
    if !matches!(
        clock,
        ClockKind::Realtime | ClockKind::Monotonic | ClockKind::Boottime | ClockKind::Process
    ) {
        return Err(Kernel::InvalidArgument.status(0));
    }
    if matches!(notify, TimerNotify::Signal | TimerNotify::Thread)
        && (signal == 0 || signal > SIGNALS)
    {
        return Err(Kernel::InvalidArgument.status(2));
    }
    if notify == TimerNotify::Thread && find_thread(process, thread).is_none() {
        return Err(Kernel::InvalidArgument.status(4));
    }
    let timer = PosixTimer {
        clock,
        notify,
        signal,
        value,
        thread,
        timer: Timer::OFF,
        fired: false,
        overrun: 0,
    };
    let mut posix = process.posix.lock();
    let timers = &mut posix.posix_timers;
    let number = match timers.iter().position(Option::is_none) {
        Some(free) => free,
        None => {
            if timers.len() >= TIMERS_MAX as usize {
                return Err(error(Kernel::WouldBlock));
            }
            timers.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
            // Room was reserved above.
            let _ = timers.push_within_capacity(None);
            timers.len() - 1
        }
    };
    timers[number] = Some(match notify {
        TimerNotify::Alarm => PosixTimer {
            notify: TimerNotify::Signal,
            signal: SIGALRM,
            value: number as u64,
            ..timer
        },
        _ => timer,
    });
    Ok(number as u32)
}

/// What is left of `timer` at `now` (0: it is not set), and its interval.
fn left(timer: &mut PosixTimer, now: u64) -> (u64, u64) {
    timer.catch_up(now);
    let mut next = timer.timer;
    if timer.fired {
        // Its next expiry, had it not waited for its signal to be taken.
        next.next_after(now, 0);
    }
    // A timer due now is still a little way off.
    let left = if next.expires == 0 { 0 } else { next.expires.saturating_sub(now).max(1) };
    (left, timer.timer.interval)
}

/// `timer_settime`: sets POSIX timer `number` of `process`'s, as
/// radix-process(2) says, and returns what was left of it and its
/// interval.
fn set_posix(
    process: &ProcessRef,
    number: u32,
    absolute: bool,
    value_ns: u64,
    interval_ns: u64,
) -> Result<(u64, u64)> {
    let armed = if value_ns != 0 {
        start()?;
        Some(register(process)?)
    } else {
        None
    };
    let wall = <Arch as radix_hal::Timer>::wall_clock_ns().unwrap_or(0);
    let old = {
        let mut posix = process.posix.lock();
        let timer = posix_timer(&mut posix, number)?;
        let now = posix_now(process, timer);
        let old = left(timer, now);
        let expires = match (absolute, timer.clock) {
            (false, _) => now.saturating_add(value_ns),
            // The wall clock's time, on the kernel's clock as it reads now.
            (true, ClockKind::Realtime) => now.saturating_add(value_ns.saturating_sub(wall)),
            (true, _) => value_ns,
        };
        timer.timer = if value_ns == 0 {
            Timer::OFF
        } else {
            Timer { expires: expires.max(1), interval: interval_ns }
        };
        timer.fired = false;
        timer.overrun = 0;
        old
    };
    drop(armed);
    changed();
    Ok(old)
}

/// Deletes POSIX timer `number` of `process`'s, whose POSIX state is
/// `posix`, and its signals that are pending (more than one if it was set
/// again while one was).
fn delete(process: &ProcessObject, posix: &mut PosixState, number: usize) {
    let Some(timer) = posix.posix_timers.get_mut(number).and_then(Option::take) else { return };
    let info = timer.info(number);
    match timer.notify {
        TimerNotify::Signal => while posix.pending.remove(timer.signal, &info) {},
        TimerNotify::Thread => {
            if let Some((_, user)) = find_thread(process, timer.thread) {
                let mut signals = user.signals.lock();
                while signals.pending.remove(timer.signal, &info) {}
            }
        }
        // A timer with no signal, or one made as Signal.
        TimerNotify::None | TimerNotify::Alarm => {}
    }
}

/// Deletes every POSIX timer of `process`'s, whose POSIX state is
/// `posix` (`exec`).
pub fn delete_all(process: &ProcessObject, posix: &mut PosixState) {
    for number in 0..posix.posix_timers.len() {
        delete(process, posix, number);
    }
    posix.posix_timers.clear();
}

/// `signal`, carrying `info`, was taken from `process`, whose POSIX state
/// is `posix`, or from one of its threads: if it is a POSIX timer's, its
/// overrun goes in it, and the timer is set again for the expiry after
/// now. Returns what it carries now. Called with the process's lock held
/// (and no thread's).
pub fn taken(process: &ProcessObject, posix: &mut PosixState, signal: u32, mut info: Info) -> Info {
    if info.code != SI_TIMER {
        return info;
    }
    let number = info.pid as usize;
    let Some(Some(timer)) = posix.posix_timers.get_mut(number) else { return info };
    if !timer.fired || timer.signal != signal || timer.info(number) != info {
        return info;
    }
    timer.fired = false;
    let mut overrun = 0;
    if timer.timer.expires != 0 {
        let now = posix_now(process, timer);
        overrun = timer.timer.next_after(now, 0).min(OVERRUN_MAX);
        changed();
    }
    timer.overrun = overrun as u32;
    info.pid |= overrun << 32;
    info
}

/// The signals of `set` were dropped where they were pending in
/// `process`, whose POSIX state is `posix`, or in one of its threads: a
/// POSIX timer whose signal went is set again, as if it had been taken
/// with nothing missed. Called with the process's lock held (and no
/// thread's).
pub fn discarded(process: &ProcessObject, posix: &mut PosixState, set: u64) {
    if set == 0 || posix.posix_timers.is_empty() {
        return;
    }
    let mut again = false;
    for number in 0..posix.posix_timers.len() {
        let Some(timer) = posix.posix_timers[number] else { continue };
        if !timer.fired || bit(timer.signal) & set == 0 {
            continue;
        }
        let info = timer.info(number);
        let pending = match timer.notify {
            TimerNotify::Thread => find_thread(process, timer.thread)
                .is_some_and(|(_, user)| user.signals.lock().pending.holds(timer.signal, &info)),
            _ => posix.pending.holds(timer.signal, &info),
        };
        if pending {
            continue;
        }
        let now = posix_now(process, &timer);
        if let Some(timer) = posix.posix_timers[number].as_mut() {
            timer.fired = false;
            if timer.timer.expires != 0 {
                timer.timer.next_after(now, now.saturating_add(DROPPED_NS));
                again = true;
            }
        }
    }
    if again {
        changed();
    }
}

/// The list of [`ARMED`], locked.
type ArmedGuard = MutexGuard<'static, Option<Vec<Weak<ProcessObject, Kmalloc>, Kmalloc>>>;

/// Puts `process` in [`ARMED`], unless it is there, and returns the list
/// still locked: the caller sets the timer before letting go of it.
/// Otherwise the timer thread, which takes a process whose timers are
/// all off out of the list under the same lock ([`unregister`]), could
/// look at its timers before the new one is set and take it out after it
/// was put in, and the timer would never expire.
fn register(process: &ProcessRef) -> Result<ArmedGuard> {
    let mut armed = ARMED.lock();
    let list = armed.get_or_insert_with(|| Vec::new_in(kmalloc()));
    if list.iter().any(|weak| core::ptr::eq(weak.as_ptr(), Arc::as_ptr(process))) {
        return Ok(armed);
    }
    list.retain(|weak| weak.strong_count() > 0);
    list.try_reserve(1).map_err(|_| Kernel::NoMemory.status(0))?;
    // Room was reserved above.
    let _ = list.push_within_capacity(Arc::downgrade(process));
    Ok(armed)
}

/// Starts the timer thread, once.
fn start() -> Result<()> {
    if STARTED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    match sched::spawn("timers", run, 0, Options::new().policy(Policy::Fair { nice: -5 })) {
        Ok(_) => Ok(()),
        Err(err) => {
            STARTED.store(false, Ordering::Release);
            error!("User: could not start the timer thread: {err}");
            Err(Kernel::NoMemory.status(0))
        }
    }
}

/// The timer thread.
fn run(_: usize) {
    loop {
        let seen = CHANGES.load(Ordering::Acquire);
        let deadline = expire_due();
        let changed = || CHANGES.load(Ordering::Acquire) != seen;
        match deadline {
            Some(deadline) => {
                wait::wait_until_or_timeout(changes_key(), "timers", deadline, changed);
            }
            None => wait::wait_until(changes_key(), "timers", changed),
        }
    }
}

/// A POSIX timer's signal to send.
#[derive(Clone, Copy)]
struct Fire {
    /// The timer's number.
    number: usize,
    /// Its signal.
    signal: u32,
    /// The thread to send it to, for TimerNotify::Thread.
    thread: Option<u64>,
    /// What the signal carries.
    info: Info,
}

/// How many POSIX timers of one process the thread sends signals for in
/// one look (more are sent at once after).
const FIRES: usize = 8;

/// Sends the signal of every timer that has expired, sets it again, and
/// returns when the thread must look next (`None`: when a timer is set).
fn expire_due() -> Option<u64> {
    let mut processes = Vec::new_in(kmalloc());
    {
        let mut armed = ARMED.lock();
        let list = armed.as_mut()?;
        list.retain(|weak| weak.strong_count() > 0);
        if processes.try_reserve(list.len()).is_err() {
            // Look again soon.
            return Some(sched::now_ns().saturating_add(CPU_TICK_NS));
        }
        for process in list.iter().filter_map(Weak::upgrade) {
            // Room was reserved above.
            let _ = processes.push_within_capacity(process);
        }
    }
    let mut next = u64::MAX;
    let mut keep_looking = false;
    for process in &processes {
        let real = sched::now_ns();
        let cpu = cpu_ns(process);
        let mut fired = [false; 3];
        let mut fires = [None; FIRES];
        let mut count = 0;
        let mut armed = false;
        {
            let mut posix = process.posix.lock();
            for (index, timer) in posix.timers.iter_mut().enumerate() {
                if timer.expires == 0 {
                    continue;
                }
                let now = if index == 0 { real } else { cpu };
                if now >= timer.expires {
                    fired[index] = true;
                    // Missed expiries count once.
                    timer.next_after(now, 0);
                }
                if timer.expires != 0 {
                    armed = true;
                    if index == 0 {
                        next = next.min(timer.expires);
                    } else {
                        keep_looking = true;
                    }
                }
            }
            for (number, slot) in posix.posix_timers.iter_mut().enumerate() {
                let Some(timer) = slot.as_mut().filter(|timer| timer.set()) else { continue };
                // One that waits for its signal to be taken is set again
                // then, and watched again.
                armed = true;
                if timer.fired {
                    continue;
                }
                let now = if timer.on_cpu_time() { cpu } else { real };
                if now >= timer.timer.expires && count < FIRES {
                    timer.fired = true;
                    if timer.timer.interval == 0 {
                        timer.timer = Timer::OFF;
                    }
                    let thread = (timer.notify == TimerNotify::Thread).then_some(timer.thread);
                    fires[count] = Some(Fire {
                        number,
                        signal: timer.signal,
                        thread,
                        info: timer.info(number),
                    });
                    count += 1;
                } else if now >= timer.timer.expires {
                    // No room to send it this time: at once, next time.
                    next = real;
                } else if timer.on_cpu_time() {
                    keep_looking = true;
                } else {
                    next = next.min(timer.timer.expires);
                }
            }
        }
        for (index, _) in fired.iter().enumerate().filter(|(_, fired)| **fired) {
            let info = Info { code: SI_KERNEL, pid: 0, status: 0 };
            signal::send_process(process, ITIMER_SIGNALS[index], info);
        }
        for fire in fires.iter().flatten() {
            if send(process, fire) != Sent::Queued {
                dropped(process, fire.number);
                next = next.min(sched::now_ns().saturating_add(DROPPED_NS));
                keep_looking = true;
            }
        }
        if !armed {
            unregister(process);
        }
    }
    if keep_looking {
        next = next.min(sched::now_ns().saturating_add(CPU_TICK_NS));
    }
    (next != u64::MAX).then_some(next)
}

/// Sends the signal of `fire`, a POSIX timer of `process`'s.
fn send(process: &ProcessRef, fire: &Fire) -> Sent {
    match fire.thread {
        None => signal::send_process(process, fire.signal, fire.info),
        Some(number) => match find_thread(process, number) {
            Some((thread, user)) => signal::send_thread(&thread, &user, fire.signal, fire.info),
            None => Sent::Dropped,
        },
    }
}

/// The signal of `process`'s POSIX timer `number` was not queued: the
/// timer is set again, as if it had been taken with nothing missed.
fn dropped(process: &ProcessRef, number: usize) {
    let mut posix = process.posix.lock();
    let Some(Some(timer)) = posix.posix_timers.get(number).copied() else { return };
    if !timer.fired {
        return;
    }
    let now = posix_now(process, &timer);
    if let Some(Some(timer)) = posix.posix_timers.get_mut(number) {
        timer.fired = false;
        if timer.timer.expires != 0 {
            timer.timer.next_after(now, now.saturating_add(DROPPED_NS));
        }
    }
}

/// Takes `process` out of [`ARMED`], unless a timer of its was set again
/// meanwhile.
fn unregister(process: &ProcessRef) {
    let mut armed = ARMED.lock();
    let Some(list) = armed.as_mut() else { return };
    {
        let posix = process.posix.lock();
        if posix.timers.iter().any(|timer| timer.expires != 0)
            || posix.posix_timers.iter().flatten().any(PosixTimer::set)
        {
            return;
        }
    }
    list.retain(|weak| !core::ptr::eq(weak.as_ptr(), Arc::as_ptr(process)));
}
