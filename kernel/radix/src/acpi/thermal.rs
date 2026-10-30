// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The firmware's thermal zones, through uACPI (Phase 11c, ADR 0203).
//!
//! Each zone's trip points are read when it is found and again when the
//! firmware notifies 0x81; its temperature (`_TMP`) by the `thermal`
//! thread, every `_TZP` where the zone has one, every `_TSP` while it
//! cools passively, and at once when the firmware notifies 0x80. As in
//! Linux:
//!
//! - at or past the passive trip point, the CPUs slow down a step at a
//!   time while the zone warms, and speed up a step at a time once it is
//!   below it and cooling ([`radix_uacpi::thermal::Cooling`]): each step
//!   takes a fifth of their highest level off ([`crate::pstate::cool`]),
//!   for every CPU, as most firmware's `_PSL` lists them all;
//! - at or past the hot one, where Linux would hibernate, the kernel
//!   warns, having no hibernation;
//! - at or past the critical one, the machine powers off: Spark is asked
//!   to, as for the power button, and the kernel does it itself if it has
//!   not within [`crate::user::power::OVERHEAT_GRACE_NS`].
//!
//! `/kernel/class/thermal` shows the zones in Linux's layout
//! (`fs::sysfs`). AML is never run under a lock.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use radix_hal::Timer;
use radix_klog::{error, info, warn};
use radix_sync::{Once, SpinLock};
use radix_uacpi::thermal::{Cooling, STEPS, Temperature, Trips};
use radix_uacpi::{Node, NodeName};

use crate::Arch;
use crate::sched::{self, Options, Policy, ThreadRef};

/// The most zones the kernel watches.
pub const MAX_ZONES: usize = 8;

/// Notify: the temperature changed.
const TEMPERATURE_CHANGED: u64 = 0x80;
/// Notify: the trip points changed.
const TRIPS_CHANGED: u64 = 0x81;

/// Nanoseconds in a tenth of a second, ACPI's unit of time here.
const DECISECOND_NS: u64 = 100_000_000;

/// What a zone last said, so that each crossing is logged once.
const HOT: u8 = 1 << 0;
const CRITICAL: u8 = 1 << 1;
const UNREADABLE: u8 = 1 << 2;

/// A thermal zone.
pub struct Zone {
    node: Node,
    name: NodeName,
    /// `_TZP`: how often to look, in tenths of a second; 0 for only when
    /// notified.
    polling_ds: AtomicU64,
    trips: SpinLock<Trips>,
    trips_stale: AtomicBool,
    cooling: SpinLock<Cooling>,
    said: AtomicU8,
    /// When the thread looks next.
    next_ns: AtomicU64,
}

static ZONES: [Once<Zone>; MAX_ZONES] = [const { Once::new() }; MAX_ZONES];
static THREAD: Once<ThreadRef> = Once::new();

/// The zones found.
pub fn zones() -> impl Iterator<Item = &'static Zone> {
    ZONES.iter().map_while(Once::get)
}

/// Finds the zones and logs what each says. Called once, once uACPI has
/// started.
pub fn find() {
    let mut count = 0;
    radix_uacpi::find_thermal_zones(&mut |node| {
        if let Some(slot) = ZONES.get(count) {
            let _ = slot.set(Zone {
                node,
                name: node.name_text(),
                polling_ds: AtomicU64::new(0),
                trips: SpinLock::new(Trips::default()),
                trips_stale: AtomicBool::new(true),
                cooling: SpinLock::new(Cooling::default()),
                said: AtomicU8::new(0),
                next_ns: AtomicU64::new(0),
            });
        }
        count += 1;
    });
    if count > MAX_ZONES {
        warn!(
            "Thermal: the firmware has {count} thermal zones; the kernel watches the first {MAX_ZONES}"
        );
    }
    for zone in zones() {
        if let Err(err) = zone.node.on_notify(notified) {
            warn!("Thermal: {}'s changes cannot be heard: {err}", zone.name);
        }
        let trips = zone.read_trips();
        let mut line = radix_klog::TextBuf::<192>::new();
        match zone.temperature() {
            Some(now) => {
                let _ = write!(line, "{} is at {now}", zone.name);
            }
            None => {
                let _ = write!(line, "{} does not say how warm it is", zone.name);
            }
        }
        for (kind, at) in trips.listed() {
            let _ = write!(line, "; {kind} at {at}");
        }
        let polling_ds = zone.node.integer(c"_TZP").unwrap_or(0);
        zone.polling_ds.store(polling_ds, Ordering::Relaxed);
        let _ = match polling_ds {
            0 => write!(line, "; looked at when the firmware says it changed"),
            ds => write!(line, "; looked at every {}.{} s", ds / 10, ds % 10),
        };
        info!("Thermal: {}", line.as_str());
    }
}

/// Starts the thread that watches the zones, if there are any. Called once,
/// once the CPUs' speed is set (`crate::pstate::start`), which passive
/// cooling caps.
pub fn start() {
    if zones().next().is_none() {
        return;
    }
    let options = Options::new().policy(Policy::Fair { nice: -10 });
    match sched::spawn("thermal", run, 0, options) {
        Ok(thread) => {
            let _ = THREAD.set(thread);
        }
        Err(err) => error!("Thermal: could not start the thread that watches the zones: {err}"),
    }
}

impl Zone {
    /// Its trip points, as last read.
    pub fn trips(&self) -> Trips {
        *self.trips.lock()
    }

    /// Reads its temperature now, `_TMP`.
    pub fn temperature(&self) -> Option<Temperature> {
        self.node.integer(c"_TMP").ok().map(Temperature)
    }

    /// Reads its trip points again.
    fn read_trips(&self) -> Trips {
        let get = |name: &core::ffi::CStr| self.node.integer(name).ok();
        let trips = Trips::new(
            get(c"_CRT"),
            get(c"_HOT"),
            get(c"_PSV"),
            [get(c"_TC1"), get(c"_TC2"), get(c"_TSP")],
        );
        self.trips_stale.store(false, Ordering::Release);
        *self.trips.lock() = trips;
        trips
    }

    /// Looks at the zone: reads its temperature and acts on it. Returns
    /// when to look next, or `None` for when the firmware notifies.
    fn check(&self, index: usize) -> Option<u64> {
        let polling_ds = self.polling_ds.load(Ordering::Relaxed);
        let trips =
            if self.trips_stale.load(Ordering::Acquire) { self.read_trips() } else { self.trips() };
        let Some(now) = self.temperature() else {
            if self.said.fetch_or(UNREADABLE, Ordering::Relaxed) & UNREADABLE == 0 {
                warn!("Thermal: {}'s _TMP fails", self.name);
            }
            return (polling_ds > 0).then_some(polling_ds * DECISECOND_NS);
        };
        self.said.fetch_and(!UNREADABLE, Ordering::Relaxed);
        self.crossed(CRITICAL, trips.critical, now, |at| {
            error!(
                "Thermal: {} is at {now}, at or past its critical trip point, {at}: powering off",
                self.name
            );
            crate::user::power::overheated(self.name.as_str());
        });
        self.crossed(HOT, trips.hot, now, |at| {
            warn!("Thermal: {} is at {now}, at or past its hot trip point, {at}", self.name);
        });
        let mut period_ds = polling_ds;
        if let Some(passive) = trips.passive {
            let (before, after, active) = {
                let mut cooling = self.cooling.lock();
                let before = cooling.step();
                let after = cooling.update(now, &passive);
                (before, after, cooling.active(now, &passive))
            };
            if before != after {
                let slowed = crate::pstate::cool(index, after);
                let what = match (slowed, after > before, after) {
                    (false, _, _) => {
                        "the firmware picks the CPUs' speed, so the kernel cannot slow them"
                    }
                    (true, true, _) => "the CPUs slow down a step",
                    (true, false, 0) => "the CPUs are back to full speed",
                    (true, false, _) => "the CPUs speed up a step",
                };
                let place = if now >= passive.at { "at or past" } else { "below" };
                info!(
                    "Thermal: {} is at {now}, {place} its passive trip point, {}: {what} ({after} of {STEPS})",
                    self.name, passive.at
                );
            }
            if active {
                period_ds = passive.period_ds;
            }
        }
        (period_ds > 0).then_some(period_ds * DECISECOND_NS)
    }

    /// Calls `say` once the zone reaches `trip`, and again only after it
    /// has been below it.
    fn crossed(
        &self,
        flag: u8,
        trip: Option<Temperature>,
        now: Temperature,
        say: impl FnOnce(Temperature),
    ) {
        let Some(at) = trip else { return };
        if now >= at {
            if self.said.fetch_or(flag, Ordering::Relaxed) & flag == 0 {
                say(at);
            }
        } else {
            self.said.fetch_and(!flag, Ordering::Relaxed);
        }
    }
}

/// In the notify thread: a zone's temperature or trip points changed.
fn notified(node: Node, value: u64) {
    let Some(zone) = zones().find(|zone| zone.node == node) else { return };
    if value == TRIPS_CHANGED {
        zone.trips_stale.store(true, Ordering::Release);
    }
    if value == TEMPERATURE_CHANGED || value == TRIPS_CHANGED {
        zone.next_ns.store(0, Ordering::Release);
        if let Some(thread) = THREAD.get() {
            sched::wake(thread);
        }
    }
}

/// The `thermal` thread: looks at each zone when it is due. A zone's
/// `next_ns` is 0 for a look at once and `u64::MAX` for none until the
/// firmware notifies.
fn run(_: usize) {
    loop {
        let now = Arch::now_ns();
        let mut wake = u64::MAX;
        for (index, zone) in zones().enumerate() {
            if zone.next_ns.load(Ordering::Acquire) <= now {
                zone.next_ns.store(u64::MAX, Ordering::Release);
                let next = zone.check(index).map_or(u64::MAX, |ns| now.saturating_add(ns));
                // Unless a notify asked for another look meanwhile.
                let _ = zone.next_ns.compare_exchange(
                    u64::MAX,
                    next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            wake = wake.min(zone.next_ns.load(Ordering::Acquire));
        }
        match wake {
            u64::MAX => sched::block("thermal"),
            wake => sched::block_until(wake, "thermal"),
        }
    }
}
