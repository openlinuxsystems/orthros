//! Time: the TSC clock, reference timers for calibration, and the local
//! APIC's periodic tick.
//!
//! The kernel's clock is the time-stamp counter, converted to nanoseconds
//! since the boot CPU first ran Rust code. Its rate comes from CPUID leaf
//! 0x15 when the processor reports it exactly, and is otherwise measured
//! against the best reference timer the machine has: the HPET, the ACPI PM
//! timer, or the 8254 PIT. The local APIC timer is measured in the same
//! window.
//!
//! The timer then runs one-shot on every CPU, in TSC-deadline mode where
//! the CPU has it and counting down otherwise, and is re-armed on every
//! interrupt for whichever comes first: the next periodic tick (at
//! [`TICK_HZ`]) or the deadline the scheduler asked for
//! ([`set_deadline`]), so slices and sleeps end on time without a faster
//! tick.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use radix_acpi::{Fadt, GenericAddress, Hpet};
use radix_sync::Once;

use crate::apic;
use crate::cpu;
use crate::interrupts::VECTOR_TIMER;
use crate::io::{inb, inl, io_delay, outb};
use crate::paging::{self, Access, Cache};
use crate::percpu;

/// Rate of the periodic timer interrupt.
pub const TICK_HZ: u32 = 100;

/// How long calibration measures for.
const CALIBRATION_NS: u64 = 50_000_000;

/// TSC value at the first Rust instruction.
static TSC_START: AtomicU64 = AtomicU64::new(0);
/// TSC frequency in Hz; 0 until calibrated.
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds per TSC tick, times 2^32.
static NS_PER_TICK: AtomicU64 = AtomicU64::new(0);
/// Local APIC timer input frequency (after divide-by-16), in Hz.
static APIC_TIMER_HZ: AtomicU64 = AtomicU64::new(0);
/// Whether the periodic tick runs.
static TICKING: AtomicBool = AtomicBool::new(false);
/// The initial count for one tick period.
static TICK_COUNT: AtomicU32 = AtomicU32::new(0);
/// Whether the timer runs in TSC-deadline mode.
static DEADLINE_MODE: AtomicBool = AtomicBool::new(false);
/// The kernel's timer handler.
static HANDLER: Once<fn(bool)> = Once::new();
/// One tick period.
const TICK_NS: u64 = 1_000_000_000 / TICK_HZ as u64;

/// How the TSC rate was found, for the boot report.
#[derive(Clone, Copy, Debug)]
pub struct Calibration {
    /// The reference timer.
    pub reference: &'static str,
    /// TSC rate in Hz.
    pub tsc_hz: u64,
    /// Whether the rate came from CPUID rather than measurement.
    pub tsc_from_cpuid: bool,
    /// The local APIC timer's input rate, in Hz before its divider.
    pub apic_timer_hz: u64,
    /// The TSC keeps a constant rate in every power state.
    pub invariant: bool,
}

static CALIBRATION: Once<Calibration> = Once::new();

/// The calibration results.
pub fn calibration() -> Option<&'static Calibration> {
    CALIBRATION.get()
}

/// Records the TSC at boot; the clock counts from here.
pub fn record_start() {
    TSC_START.store(cpu::rdtsc(), Ordering::Relaxed);
}

/// Nanoseconds since boot, or 0 before calibration.
pub fn now_ns() -> u64 {
    let ns_per_tick = NS_PER_TICK.load(Ordering::Relaxed);
    if ns_per_tick == 0 {
        return 0;
    }
    let ticks = cpu::rdtsc().wrapping_sub(TSC_START.load(Ordering::Relaxed));
    ((u128::from(ticks) * u128::from(ns_per_tick)) >> 32) as u64
}

/// Waits at least `us` microseconds.
pub fn delay_us(us: u64) {
    let hz = TSC_HZ.load(Ordering::Relaxed);
    if hz == 0 {
        // Each write to port 0x80 takes about a microsecond on PC hardware.
        for _ in 0..us {
            io_delay();
        }
        return;
    }
    let start = cpu::rdtsc();
    let ticks = (u128::from(us) * u128::from(hz) / 1_000_000) as u64;
    while cpu::rdtsc().wrapping_sub(start) < ticks {
        cpu::pause();
    }
}

/// A timer of known rate to measure the others against.
enum Reference {
    Hpet { counter: *const u64, period_fs: u64, wide: bool },
    PmTimer { register: GenericAddress, virt: usize, wide: bool },
    Pit,
}

impl Reference {
    fn name(&self) -> &'static str {
        match self {
            Reference::Hpet { .. } => "HPET",
            Reference::PmTimer { .. } => "ACPI PM timer",
            Reference::Pit => "8254 PIT",
        }
    }

    /// Reads the counter.
    fn read(&self) -> u64 {
        match *self {
            Reference::Hpet { counter, wide, .. } => {
                // SAFETY: `counter` points at the main counter register in
                // the HPET's register page, mapped uncached.
                let value = unsafe { counter.read_volatile() };
                if wide { value } else { value & 0xffff_ffff }
            }
            Reference::PmTimer { register, virt, wide } => {
                let value = if register.space == GenericAddress::IO {
                    // SAFETY: the FADT names this port as the PM timer;
                    // reading it has no side effects.
                    unsafe { inl(register.address as u16) }
                } else {
                    // SAFETY: `virt` maps the PM timer register uncached.
                    unsafe { (virt as *const u32).read_volatile() }
                };
                u64::from(if wide { value } else { value & 0x00ff_ffff })
            }
            Reference::Pit => 0,
        }
    }

    /// Nanoseconds between two readings.
    fn elapsed_ns(&self, from: u64, to: u64) -> u64 {
        match *self {
            Reference::Hpet { period_fs, wide, .. } => {
                let ticks =
                    if wide { to.wrapping_sub(from) } else { to.wrapping_sub(from) & 0xffff_ffff };
                (u128::from(ticks) * u128::from(period_fs) / 1_000_000) as u64
            }
            Reference::PmTimer { wide, .. } => {
                let mask = if wide { 0xffff_ffff } else { 0x00ff_ffff };
                let ticks = to.wrapping_sub(from) & mask;
                ticks * 1_000_000_000 / Fadt::PM_TIMER_HZ
            }
            Reference::Pit => 0,
        }
    }
}

fn hpet_reference(hpet: &Hpet) -> Option<Reference> {
    let base = paging::map_physical(hpet.address, 1024, Access::ReadWrite, Cache::Uncached);
    // SAFETY: `base` maps the HPET's 1 KiB register block uncached. Offset 0
    // is the capabilities register, 0x10 the configuration register.
    let capabilities = unsafe { (base as *const u64).read_volatile() };
    let period_fs = capabilities >> 32;
    if period_fs == 0 || period_fs > 100_000_000 {
        return None;
    }
    // SAFETY: as above; setting bit 0 of the configuration register starts
    // the main counter (with interrupts from its comparators left off).
    unsafe {
        let config = (base + 0x10) as *mut u64;
        config.write_volatile(config.read_volatile() | 1);
    }
    Some(Reference::Hpet {
        counter: (base + 0xf0) as *const u64,
        period_fs,
        wide: capabilities & (1 << 13) != 0,
    })
}

fn pm_timer_reference(fadt: &Fadt) -> Option<Reference> {
    let (register, wide) = fadt.pm_timer?;
    let virt = match register.space {
        GenericAddress::IO if register.address <= 0xffff => 0,
        GenericAddress::MEMORY => {
            paging::map_physical(register.address, 4, Access::ReadOnly, Cache::Uncached)
        }
        _ => return None,
    };
    Some(Reference::PmTimer { register, virt, wide })
}

/// Measures TSC and APIC timer ticks against the PIT: channel 2 counts
/// down from `count` in mode 0 while the TSC runs.
fn measure_with_pit() -> (u64, u64, u64) {
    const PIT_HZ: u64 = 1_193_182;
    let count = (PIT_HZ * CALIBRATION_NS / 1_000_000_000) as u16;
    // SAFETY: ports 0x61 (speaker/gate control), 0x42 and 0x43 belong to
    // the PIT; this gates channel 2 on with the speaker off and programs it
    // in mode 0, touching no memory.
    unsafe {
        outb(0x61, (inb(0x61) & !0x02) | 0x01);
        outb(0x43, 0b1011_0000);
        outb(0x42, count as u8);
        outb(0x42, (count >> 8) as u8);
    }
    apic::timer_start_calibration();
    let start = cpu::rdtsc();
    // OUT2 (bit 5 of port 0x61) goes high when the count reaches zero.
    // SAFETY: reading port 0x61 has no side effects.
    while unsafe { inb(0x61) } & 0x20 == 0 {
        cpu::pause();
    }
    let tsc = cpu::rdtsc() - start;
    let apic = u64::from(u32::MAX - apic::timer_current());
    (u64::from(count) * 1_000_000_000 / PIT_HZ, tsc, apic)
}

/// Measures the TSC and the local APIC timer over about 50 ms.
fn measure(reference: &Reference) -> (u64, u64, u64) {
    if matches!(reference, Reference::Pit) {
        return measure_with_pit();
    }
    apic::timer_start_calibration();
    let tsc_start = cpu::rdtsc();
    let ref_start = reference.read();
    let (mut ref_now, mut elapsed);
    loop {
        ref_now = reference.read();
        elapsed = reference.elapsed_ns(ref_start, ref_now);
        if elapsed >= CALIBRATION_NS {
            break;
        }
        cpu::pause();
    }
    let tsc = cpu::rdtsc() - tsc_start;
    let apic = u64::from(u32::MAX - apic::timer_current());
    (elapsed, tsc, apic)
}

/// The TSC rate from CPUID leaf 0x15 (and 0x16 for the crystal on parts
/// that leave it out), when the processor reports it.
fn tsc_hz_from_cpuid() -> Option<u64> {
    if cpu::max_leaf() < 0x15 {
        return None;
    }
    let leaf = cpu::cpuid(0x15, 0);
    let (denominator, numerator, crystal) = (leaf.eax, leaf.ebx, leaf.ecx);
    if denominator == 0 || numerator == 0 || crystal == 0 {
        return None;
    }
    Some(u64::from(crystal) * u64::from(numerator) / u64::from(denominator))
}

/// Calibrates the clocks. Called once, on the boot CPU, with the APIC
/// initialised and interrupts off.
pub fn calibrate(hpet: Option<&Hpet>, fadt: Option<&Fadt>) {
    let reference = hpet
        .and_then(hpet_reference)
        .or_else(|| fadt.and_then(pm_timer_reference))
        .unwrap_or(Reference::Pit);
    let (elapsed_ns, tsc_ticks, apic_ticks) = measure(&reference);
    apic::timer_stop();
    let measured = (u128::from(tsc_ticks) * 1_000_000_000 / u128::from(elapsed_ns.max(1))) as u64;
    // CPUID's figure is exact, but trust it only if the measurement agrees
    // to within 2 %: hypervisors do not always report it correctly.
    let exact = tsc_hz_from_cpuid().filter(|&hz| hz.abs_diff(measured) <= measured / 50);
    let tsc_hz = exact.unwrap_or(measured).max(1);
    let apic_hz =
        (u128::from(apic_ticks) * 16 * 1_000_000_000 / u128::from(elapsed_ns.max(1))) as u64;

    TSC_HZ.store(tsc_hz, Ordering::Relaxed);
    NS_PER_TICK.store(((1_000_000_000u128 << 32) / u128::from(tsc_hz)) as u64, Ordering::Relaxed);
    APIC_TIMER_HZ.store(apic_hz, Ordering::Relaxed);
    let per_tick = apic_hz / 16 / u64::from(TICK_HZ);
    TICK_COUNT.store(per_tick.clamp(1, u64::from(u32::MAX)) as u32, Ordering::Relaxed);
    let _ = CALIBRATION.set(Calibration {
        reference: reference.name(),
        tsc_hz,
        tsc_from_cpuid: exact.is_some(),
        apic_timer_hz: apic_hz,
        invariant: cpu::feature::invariant_tsc(),
    });
}

/// Starts the tick on this CPU.
pub fn start_tick() {
    let count = TICK_COUNT.load(Ordering::Relaxed);
    if count == 0 {
        return;
    }
    let deadline = cpu::feature::tsc_deadline();
    if percpu::current_index() == 0 {
        DEADLINE_MODE.store(deadline, Ordering::Relaxed);
    }
    radix_klog::tracepoint!("timer.start", percpu::current_index(), count);
    let cpu = percpu::current();
    cpu.next_tick_ns.store(now_ns() + TICK_NS, Ordering::Relaxed);
    cpu.deadline_ns.store(u64::MAX, Ordering::Relaxed);
    if DEADLINE_MODE.load(Ordering::Relaxed) {
        apic::timer_start_tsc_deadline(VECTOR_TIMER);
    } else {
        apic::timer_start_one_shot(VECTOR_TIMER);
    }
    program(cpu.next_tick_ns.load(Ordering::Relaxed));
    TICKING.store(true, Ordering::Relaxed);
}

/// How the timer is programmed.
pub fn mode_name() -> &'static str {
    if !TICKING.load(Ordering::Relaxed) {
        "not running"
    } else if DEADLINE_MODE.load(Ordering::Relaxed) {
        "TSC deadline"
    } else {
        "APIC one-shot"
    }
}

/// Arms this CPU's timer for `at_ns`.
fn program(at_ns: u64) {
    if DEADLINE_MODE.load(Ordering::Relaxed) {
        let hz = TSC_HZ.load(Ordering::Relaxed);
        let ticks = (u128::from(at_ns) * u128::from(hz) / 1_000_000_000) as u64;
        apic::timer_arm_deadline(TSC_START.load(Ordering::Relaxed).wrapping_add(ticks));
    } else {
        let delta = at_ns.saturating_sub(now_ns()).max(1_000);
        let rate = APIC_TIMER_HZ.load(Ordering::Relaxed) / 16;
        let count = u128::from(delta) * u128::from(rate) / 1_000_000_000;
        apic::timer_arm(count.clamp(1, u128::from(u32::MAX)) as u32);
    }
}

/// Registers the kernel's timer handler.
pub fn set_handler(handler: fn(bool)) {
    let _ = HANDLER.set(handler);
}

/// Asks for a timer interrupt on this CPU at `deadline_ns`, or cancels
/// the request with `u64::MAX`. Call with interrupts masked.
pub fn set_deadline(deadline_ns: u64) {
    if !TICKING.load(Ordering::Relaxed) {
        return;
    }
    let cpu = percpu::current();
    cpu.deadline_ns.store(deadline_ns, Ordering::Relaxed);
    program(cpu.next_tick_ns.load(Ordering::Relaxed).min(deadline_ns));
}

/// Rate of the tick, or 0 when it is not running.
pub fn tick_hz() -> u32 {
    if TICKING.load(Ordering::Relaxed) { TICK_HZ } else { 0 }
}

/// The timer interrupt: counts a tick if one is due, re-arms the timer,
/// and calls the kernel's handler.
pub fn on_tick() {
    // No trace point here: at 100 Hz on every CPU it would push everything
    // else out of the trace ring within seconds.
    let cpu = percpu::current();
    let now = now_ns();
    let next = cpu.next_tick_ns.load(Ordering::Relaxed);
    let tick = now >= next;
    if tick {
        cpu.ticks.fetch_add(1, Ordering::Relaxed);
        // Ticks missed while interrupts were off are not made up.
        let after = if next + TICK_NS > now { next + TICK_NS } else { now + TICK_NS };
        cpu.next_tick_ns.store(after, Ordering::Relaxed);
    }
    let deadline = cpu.deadline_ns.load(Ordering::Relaxed);
    let due = deadline <= now;
    if due {
        cpu.deadline_ns.store(u64::MAX, Ordering::Relaxed);
    }
    crate::paging::serve_shootdown();
    program(cpu.next_tick_ns.load(Ordering::Relaxed).min(cpu.deadline_ns.load(Ordering::Relaxed)));
    apic::end_of_interrupt();
    if (tick || due)
        && let Some(handler) = HANDLER.get()
    {
        handler(tick);
    }
}
