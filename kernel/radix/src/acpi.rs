// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The firmware's AML, run by uACPI (Phase 11c, ADR 0200).
//!
//! [`start`] gives uACPI the kernel's services (`radix_uacpi::Host`) and
//! starts it, once the PCI scan is done: it reads the ACPI tables, takes
//! the machine into ACPI mode, loads the DSDT's and SSDTs' AML, routes the
//! System Control Interrupt and runs the devices' `_STA` and `_INI`. Its
//! deferred work runs in two threads, `acpi-events` on the boot CPU for
//! general-purpose events' methods, as uACPI asks, and `acpi-notify`.
//! [`power_button`] then hears the fixed power button through uACPI, and
//! [`finish_events`] turns on every general-purpose event that has a
//! method. Powering off goes through uACPI's S5, which runs `_PTS` first
//! ([`power_off`]).
//!
//! With `aml=off`, or if uACPI cannot start, no AML runs, and the kernel
//! hears the fixed power button by itself as before (ADR 0142).

use core::alloc::Allocator;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use radix_hal::{Aml, Cpu, Devices, PciSegment, Timer};
use radix_hw::Mmio;
use radix_klog::{debug, error, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::{Once, SpinLock};
use radix_uacpi::{Host, Level, PciAddress, Queue, SleepState};

use crate::Arch;
use crate::sched::{self, Options, Policy, ThreadRef, wait};

/// Whether uACPI runs.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// The CPU general-purpose events' methods run on.
const EVENTS_CPU: usize = 0;

/// The threads that run uACPI's work: events, then notify.
static THREADS: [Once<ThreadRef>; 2] = [Once::new(), Once::new()];

/// The heap, for callers that may sleep and for those that may not.
static MAY_SLEEP: Kmalloc = HEAP.with(Flags::MAY_SLEEP);
static ATOMIC: Kmalloc = HEAP.with(Flags::ATOMIC);

/// The most PCI buses whose configuration space AML reaches, each mapped
/// once and kept.
const MAX_BUSES: usize = 16;

/// The buses mapped so far: segment, bus, and its configuration space.
static BUSES: SpinLock<[Option<(u16, u8, Mmio)>; MAX_BUSES]> =
    SpinLock::new([const { None }; MAX_BUSES]);

/// Whether uACPI runs the firmware's AML.
pub fn running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

/// Starts uACPI, unless `off` (`aml=off`). Called once, after the PCI
/// scan, from a thread.
pub fn start(off: bool) {
    if off {
        info!("ACPI: the firmware's AML is not run (`aml=off`)");
        return;
    }
    if Arch::acpi_rsdp().is_none() {
        info!("ACPI: no tables, so no AML to run");
        return;
    }
    for (index, name) in ["acpi-events", "acpi-notify"].into_iter().enumerate() {
        let options = Options::new().policy(Policy::Fair { nice: -5 });
        let options = if index == 0 { options.pinned(EVENTS_CPU) } else { options };
        match sched::spawn(name, run, index, options) {
            Ok(thread) => {
                let _ = THREADS[index].set(thread);
            }
            Err(err) => {
                error!("ACPI: could not start the thread `{name}`: {err}; no AML runs");
                return;
            }
        }
    }
    match radix_uacpi::start(&KERNEL) {
        Ok(()) => {
            RUNNING.store(true, Ordering::Release);
            info!("ACPI: uACPI {} runs the firmware's AML", radix_uacpi::VERSION);
        }
        Err(err) => error!("ACPI: uACPI {err}; no AML runs"),
    }
}

/// Hears the fixed power button through uACPI: `pressed` is called, in an
/// interrupt handler, at each press. Returns what the button is, or
/// `None` if uACPI does not run or cannot.
pub fn power_button(pressed: fn()) -> Option<&'static str> {
    if !running() {
        return None;
    }
    match radix_uacpi::on_power_button(pressed) {
        Ok(()) => Some("the ACPI power button (through uACPI)"),
        Err(err) => {
            // A hardware-reduced machine, or one whose button is a device
            // of the namespace (Phase 11c's next part).
            debug!("ACPI: uACPI does not hear a fixed power button: {err}");
            None
        }
    }
}

/// Turns on the general-purpose events that have methods, once the
/// kernel's handlers are in.
pub fn finish_events() {
    if running()
        && let Err(err) = radix_uacpi::finish_events()
    {
        warn!("ACPI: uACPI could not turn on the general-purpose events: {err}");
    }
}

/// Powers the machine off through uACPI, if it runs: `_PTS(5)`, then S5.
/// Returns only if the machine stayed on.
pub fn power_off() {
    if !running() {
        return;
    }
    if let Err(err) = radix_uacpi::prepare_sleep(SleepState::Off) {
        warn!("ACPI: the firmware's preparation to power off failed: {err}");
    }
    info!("Power: powering off (ACPI S5, through uACPI)");
    let were = Arch::disable_interrupts();
    let result = radix_uacpi::sleep(SleepState::Off);
    Arch::restore_interrupts(were);
    if let Err(err) = result {
        warn!("ACPI: uACPI could not power the machine off: {err}");
    }
}

/// A thread that runs uACPI's work: the events queue's (0) or the notify
/// queue's (1).
fn run(index: usize) {
    let queue = if index == 0 { Queue::Events } else { Queue::Notify };
    loop {
        while radix_uacpi::run_work(queue) {}
        sched::block("acpi");
    }
}

/// The System Control Interrupt.
fn on_interrupt() {
    radix_uacpi::interrupt();
}

/// Configuration space of the PCI bus `bus` of segment `segment`, mapped
/// the first time AML reaches it.
fn bus_space(segment: u16, bus: u8) -> Option<Mmio> {
    {
        let buses = BUSES.lock_irq();
        if let Some(found) = buses.iter().flatten().find(|b| b.0 == segment && b.1 == bus) {
            return Some(found.2.clone());
        }
        if buses.iter().all(Option::is_some) {
            // AML reaching more buses than this is not known; refusing
            // keeps each access from mapping another megabyte.
            return None;
        }
    }
    let mut base = None;
    <Arch as Devices>::pci_segments(&mut |found: PciSegment| {
        if found.segment == segment && (found.first_bus..=found.last_bus).contains(&bus) {
            base = Some(found.base + (u64::from(bus) << 20));
        }
    });
    // Mapped with interrupts on; should another CPU map it meanwhile, its
    // mapping is kept and this one only costs address space.
    let space = <Arch as Devices>::map_device(base?, 1 << 20)?;
    let mut buses = BUSES.lock_irq();
    if let Some(found) = buses.iter().flatten().find(|b| b.0 == segment && b.1 == bus) {
        return Some(found.2.clone());
    }
    let slot = buses.iter_mut().find(|slot| slot.is_none())?;
    *slot = Some((segment, bus, space.clone()));
    Some(space)
}

/// The function's 4 KiB of configuration space.
fn function_space(address: PciAddress) -> Option<Mmio> {
    let offset = (usize::from(address.device) << 15) | (usize::from(address.function) << 12);
    bus_space(address.segment, address.bus)?.window(offset, 4096)
}

/// The kernel, as uACPI's host.
struct Kernel;

static KERNEL: Kernel = Kernel;

impl Host for Kernel {
    fn rsdp(&self) -> Option<u64> {
        Arch::acpi_rsdp()
    }

    fn map(&self, phys: u64, len: usize) -> Option<usize> {
        Arch::map_firmware(phys, len)
    }

    fn unmap(&self, virt: usize, len: usize) {
        Arch::unmap_firmware(virt, len);
    }

    fn heap(&self) -> &(dyn Allocator + Sync) {
        if sched::can_sleep() { &MAY_SLEEP } else { &ATOMIC }
    }

    fn log(&self, level: Level, message: fmt::Arguments<'_>) {
        match level {
            // As Linux marks its interpreter's: `ACPI Error:`.
            Level::Error => error!("ACPI: error: {message}"),
            Level::Warn => warn!("ACPI: warning: {message}"),
            Level::Info => info!("ACPI: {message}"),
            Level::Debug => debug!("ACPI: {message}"),
        }
    }

    fn pci_read(&self, address: PciAddress, offset: usize, width: u8) -> Option<u32> {
        let space = function_space(address)?;
        if !offset.is_multiple_of(usize::from(width)) || offset + usize::from(width) > 4096 {
            return None;
        }
        Some(match width {
            1 => u32::from(space.read8(offset)),
            2 => u32::from(space.read16(offset)),
            _ => space.read32(offset),
        })
    }

    fn pci_write(&self, address: PciAddress, offset: usize, width: u8, value: u32) -> bool {
        let Some(space) = function_space(address) else { return false };
        if !offset.is_multiple_of(usize::from(width)) || offset + usize::from(width) > 4096 {
            return false;
        }
        match width {
            1 => space.write8(offset, value as u8),
            2 => space.write16(offset, value as u16),
            _ => space.write32(offset, value),
        }
        true
    }

    fn port_read(&self, port: u16, width: u8) -> u32 {
        Arch::read_port(port, width)
    }

    fn port_write(&self, port: u16, width: u8, value: u32) {
        Arch::write_port(port, width, value);
    }

    fn now_ns(&self) -> u64 {
        Arch::now_ns()
    }

    fn stall_us(&self, us: u32) {
        Arch::delay_us(u64::from(us));
    }

    fn sleep_ms(&self, ms: u64) {
        sched::sleep_ns(ms.saturating_mul(1_000_000));
    }

    fn thread_id(&self) -> usize {
        // The running thread's token, 0 before threads run; never all ones.
        sched::current_token()
    }

    fn disable_interrupts(&self) -> bool {
        Arch::disable_interrupts()
    }

    fn restore_interrupts(&self, were_enabled: bool) {
        Arch::restore_interrupts(were_enabled);
    }

    fn wait(&self, key: usize, deadline_ns: Option<u64>, ready: &dyn Fn() -> bool) -> bool {
        match deadline_ns {
            Some(deadline) => wait::wait_until_or_timeout(key, "acpi", deadline, ready),
            None => {
                wait::wait_until(key, "acpi", ready);
                true
            }
        }
    }

    fn wake_all(&self, key: usize) {
        wait::wake_all(key);
    }

    fn start_interrupt(&self, irq: u32) -> bool {
        Arch::start_firmware_interrupt(irq, on_interrupt)
    }

    fn stop_interrupt(&self) {
        Arch::stop_firmware_interrupt();
    }

    fn work_queued(&self, queue: Queue) {
        let index = match queue {
            Queue::Events => 0,
            Queue::Notify => 1,
        };
        if let Some(thread) = THREADS[index].get() {
            sched::wake(thread);
        }
    }
}
