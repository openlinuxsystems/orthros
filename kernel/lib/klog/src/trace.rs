// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Trace points: a lock-free recorder for events too frequent to log.
//!
//! A trace point is a static at its call site, created by the
//! [`tracepoint!`](crate::tracepoint) macro. When tracing is on, each hit
//! stores a timestamp, the CPU, the trace point and up to three numbers in a
//! fixed ring of [`ENTRIES`] events. Recording takes no lock, so trace points
//! are safe in interrupt and exception handlers and work from the first
//! instruction of Rust code. When tracing is off, a hit costs one relaxed
//! atomic load.
//!
//! Tracing starts at boot with the `trace` kernel flag, or from the kernel
//! debugger with `trace on`; `trace` in the debugger prints the ring.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use radix_sync::Once;

/// Events the ring keeps.
pub const ENTRIES: usize = 1024;
/// Most distinct trace points.
pub const MAX_POINTS: usize = 256;

static ENABLED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU16 = AtomicU16::new(1);
static POINTS: [Once<&'static TracePoint>; MAX_POINTS] = [const { Once::new() }; MAX_POINTS];
static HEAD: AtomicU64 = AtomicU64::new(0);
static RING: [Slot; ENTRIES] = [const { Slot::new() }; ENTRIES];

/// A named place in the code that can record events.
#[derive(Debug)]
pub struct TracePoint {
    name: &'static str,
    id: AtomicU16,
}

impl TracePoint {
    /// Creates a trace point; use [`tracepoint!`](crate::tracepoint).
    pub const fn new(name: &'static str) -> Self {
        Self { name, id: AtomicU16::new(0) }
    }

    /// The trace point's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The trace point's number, assigned on its first hit.
    fn id(&'static self) -> u16 {
        let id = self.id.load(Ordering::Relaxed);
        if id != 0 {
            return id;
        }
        let new = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        if usize::from(new) >= MAX_POINTS {
            return 0;
        }
        match self.id.compare_exchange(0, new, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                let _ = POINTS[usize::from(new)].set(self);
                new
            }
            Err(existing) => existing,
        }
    }
}

struct Slot {
    /// Sequence number of the event in the slot, or `u64::MAX` while it is
    /// being written.
    seq: AtomicU64,
    time_ns: AtomicU64,
    /// CPU in the upper 48 bits, trace point number in the lower 16.
    origin: AtomicU64,
    args: [AtomicU64; 3],
}

impl Slot {
    const fn new() -> Self {
        Self {
            seq: AtomicU64::new(u64::MAX),
            time_ns: AtomicU64::new(0),
            origin: AtomicU64::new(0),
            args: [const { AtomicU64::new(0) }; 3],
        }
    }
}

/// One recorded event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// Position in the sequence of all events.
    pub seq: u64,
    /// When it happened.
    pub time_ns: u64,
    /// CPU it happened on.
    pub cpu: usize,
    /// Name of the trace point.
    pub name: &'static str,
    /// The numbers recorded with it.
    pub args: [u64; 3],
}

/// Whether trace points record.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turns recording on or off.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Records a hit of `point`. Called by [`tracepoint!`](crate::tracepoint)
/// when tracing is on.
pub fn hit(point: &'static TracePoint, args: [u64; 3]) {
    let id = point.id();
    let seq = HEAD.fetch_add(1, Ordering::Relaxed);
    let slot = &RING[(seq % ENTRIES as u64) as usize];
    slot.seq.store(u64::MAX, Ordering::Relaxed);
    slot.time_ns.store(crate::now_ns(), Ordering::Relaxed);
    slot.origin.store(((crate::current_cpu() as u64) << 16) | u64::from(id), Ordering::Relaxed);
    for (stored, value) in slot.args.iter().zip(args) {
        stored.store(value, Ordering::Relaxed);
    }
    slot.seq.store(seq, Ordering::Release);
}

/// Name of trace point number `id`.
fn point_name(id: u16) -> &'static str {
    POINTS.get(usize::from(id)).and_then(Once::get).map_or("?", |point| point.name)
}

/// Total events recorded since boot, including those overwritten.
pub fn recorded() -> u64 {
    HEAD.load(Ordering::Relaxed)
}

/// Calls `f` for each event still in the ring, oldest first. Events being
/// overwritten while this runs are skipped.
pub fn for_each(mut f: impl FnMut(&Event)) {
    let head = HEAD.load(Ordering::Acquire);
    for seq in head.saturating_sub(ENTRIES as u64)..head {
        let slot = &RING[(seq % ENTRIES as u64) as usize];
        if slot.seq.load(Ordering::Acquire) != seq {
            continue;
        }
        let origin = slot.origin.load(Ordering::Relaxed);
        let event = Event {
            seq,
            time_ns: slot.time_ns.load(Ordering::Relaxed),
            cpu: (origin >> 16) as usize,
            name: point_name(origin as u16),
            args: [0, 1, 2].map(|i| slot.args[i].load(Ordering::Relaxed)),
        };
        if slot.seq.load(Ordering::Acquire) == seq {
            f(&event);
        }
    }
}

/// Calls `f` for every trace point hit at least once since boot.
pub fn for_each_point(mut f: impl FnMut(&'static str)) {
    let count = usize::from(NEXT_ID.load(Ordering::Relaxed)).min(MAX_POINTS);
    for slot in &POINTS[1..count] {
        if let Some(point) = slot.get() {
            f(point.name);
        }
    }
}

/// Records an event at this point in the code when tracing is on.
///
/// ```ignore
/// tracepoint!("apic.timer");
/// tracepoint!("smp.ap-online", cpu, apic_id);
/// ```
#[macro_export]
macro_rules! tracepoint {
    ($name:literal) => {
        $crate::tracepoint!($name, 0u64, 0u64, 0u64)
    };
    ($name:literal, $a:expr) => {
        $crate::tracepoint!($name, $a, 0u64, 0u64)
    };
    ($name:literal, $a:expr, $b:expr) => {
        $crate::tracepoint!($name, $a, $b, 0u64)
    };
    ($name:literal, $a:expr, $b:expr, $c:expr) => {{
        static POINT: $crate::trace::TracePoint = $crate::trace::TracePoint::new($name);
        if $crate::trace::enabled() {
            $crate::trace::hit(&POINT, [($a) as u64, ($b) as u64, ($c) as u64]);
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_when_enabled() {
        let _serial = crate::tests::SERIAL.lock().unwrap();
        set_enabled(false);
        let before = recorded();
        crate::tracepoint!("test.off", 1);
        assert_eq!(recorded(), before);

        set_enabled(true);
        for i in 0..3u64 {
            crate::tracepoint!("test.on", i, i * 2);
        }
        set_enabled(false);
        let mut events = Vec::new();
        for_each(|event| events.push(*event));
        let mine: Vec<_> = events.iter().filter(|e| e.name == "test.on").collect();
        assert_eq!(mine.len(), 3);
        assert_eq!(mine[2].args, [2, 4, 0]);
        let mut names = Vec::new();
        for_each_point(|name| names.push(name));
        assert!(names.contains(&"test.on"));
        assert!(!names.contains(&"test.off"));
    }

    #[test]
    fn ring_keeps_the_latest_entries() {
        let _serial = crate::tests::SERIAL.lock().unwrap();
        set_enabled(true);
        for i in 0..(ENTRIES as u64 + 10) {
            crate::tracepoint!("test.wrap", i);
        }
        set_enabled(false);
        let mut count = 0;
        let mut last = 0;
        for_each(|event| {
            count += 1;
            last = event.args[0];
        });
        assert_eq!(count, ENTRIES);
        assert_eq!(last, ENTRIES as u64 + 9);
    }
}
