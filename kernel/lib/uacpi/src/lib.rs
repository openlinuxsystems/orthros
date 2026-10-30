// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! uACPI in the Radix kernel (Phase 11c, ADR 0200): the interpreter of the
//! AML in the firmware's DSDT and SSDTs, through which ACPI's buttons,
//! batteries, lids, thermal zones and sleep states are reached.
//!
//! uACPI 7.0.0 by Daniil Tatianin (MIT, <https://github.com/uACPI/uACPI>),
//! at commit c0edece, which adds the fix for a table lock left held after
//! a bad checksum, is in `vendor/`, unchanged; `build.rs` compiles it with
//! Clang for the kernel's target. It asks the kernel for what it needs
//! through some forty `uacpi_kernel_*` functions
//! (`vendor/include/uacpi/kernel_api.h`), which `ffi` defines on top of
//! the safe [`Host`] trait the kernel implements: physical memory, I/O
//! ports, PCI configuration space, time, sleeping and waking threads, and
//! the System Control Interrupt. The rest is made here from those:
//!
//! - uACPI's one blocking primitive, a counting semaphore, is an atomic
//!   count whose waiters sleep in the kernel's wait table ([`Host::wait`]);
//! - its spin locks are atomic flags held with interrupts masked;
//! - its deferred work goes on two queues, for the kernel's two threads to
//!   run ([`run_work`]): the methods of general-purpose events, on the boot
//!   CPU as uACPI asks, and `Notify`'s handlers;
//! - its memory comes from the kernel's heap, through [`Host::heap`];
//! - the embedded controller's regions, which laptops' AML reads their
//!   batteries and lids through, go through the controller's ports, and
//!   its events run their `_Qxx` methods (`ec`, ADR 0201).
//!
//! The kernel finds devices with [`find_devices`] and evaluates their
//! objects through [`Node`]; [`power`] reads a battery's packages as
//! Linux does.
//! This crate is part of the kernel's trusted computing base, as the C it
//! links is (docs/coding-standard.md): `unsafe` is allowed only in `ffi`,
//! where the C is called and calls back. The crate's tests run uACPI on
//! the build host against a simulated machine, with tables made by `iasl`
//! from `src/tests/*.asl`.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]
#![feature(allocator_ext)]
#![feature(vec_push_within_capacity)]

extern crate alloc;

mod ec;
mod ffi;
pub mod power;
mod sync;
#[cfg(all(test, target_arch = "x86_64"))]
mod tests;
mod work;

use core::alloc::Allocator;
use core::fmt;

use radix_sync::Once;

pub use ec::{Found as EmbeddedController, found as embedded_controller};
pub use ffi::{
    Error, Node, NodeName, NodePath, Package, StartError, eval_integer, find_devices,
    finish_events, interrupt, on_power_button, prepare_sleep, run_work, sleep, start,
};

/// The version of uACPI in `vendor/`.
pub const VERSION: &str = "7.0.0";

/// How serious a message of uACPI's is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Something failed: a method, a table, an access.
    Error,
    /// Something is wrong but uACPI goes on, as firmware often needs.
    Warn,
    /// What uACPI loaded and found.
    Info,
    /// uACPI's trace and debug messages, which it prints only when asked.
    Debug,
}

/// A PCI function, as AML names it for its `PCI_Config` regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciAddress {
    /// The PCI segment (domain).
    pub segment: u16,
    /// The bus in it.
    pub bus: u8,
    /// The device on the bus, below 32.
    pub device: u8,
    /// The function of the device, below 8.
    pub function: u8,
}

impl fmt::Display for PciAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:02x}:{:02x}.{}", self.segment, self.bus, self.device, self.function)
    }
}

/// Which of the kernel's threads runs a piece of uACPI's deferred work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Queue {
    /// A general-purpose event's method, which uACPI asks to run on the
    /// boot CPU: some firmware's System Management Mode code expects it.
    Events,
    /// The handlers of a `Notify`, on any CPU.
    Notify,
}

/// An ACPI sleep state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepState {
    /// S5: soft off.
    Off,
}

/// What uACPI needs from the kernel. Every method may be called on any
/// CPU; those marked so are also called in interrupt handlers or with
/// interrupts masked, and must not sleep.
pub trait Host: Sync {
    /// The physical address of the ACPI RSDP, or of a copy of it in RAM
    /// that stays there.
    fn rsdp(&self) -> Option<u64>;

    /// Maps `len` bytes of physical memory from `phys` for reading and
    /// writing, until [`Host::unmap`]: RAM as RAM, anything else uncached,
    /// as it may be device registers. Returns the address of `phys`.
    fn map(&self, phys: u64, len: usize) -> Option<usize>;

    /// Undoes a [`Host::map`] that returned `virt` for `len` bytes.
    fn unmap(&self, virt: usize, len: usize);

    /// The heap uACPI's memory comes from; memory from it may be freed
    /// through it from any context. Called with interrupts masked too.
    fn heap(&self) -> &(dyn Allocator + Sync);

    /// Logs one of uACPI's messages, or one about it. Called with
    /// interrupts masked too.
    fn log(&self, level: Level, message: fmt::Arguments<'_>);

    /// Reads `width` bytes (1, 2 or 4) of `address`'s configuration space
    /// at `offset`; `None` if the machine gives no way to reach it.
    fn pci_read(&self, address: PciAddress, offset: usize, width: u8) -> Option<u32>;

    /// Writes `width` bytes (1, 2 or 4) of `address`'s configuration
    /// space at `offset`; false if the machine gives no way to reach it.
    fn pci_write(&self, address: PciAddress, offset: usize, width: u8, value: u32) -> bool;

    /// Reads `width` bytes (1, 2 or 4) from I/O port `port`. Called with
    /// interrupts masked too.
    fn port_read(&self, port: u16, width: u8) -> u32;

    /// Writes `width` bytes (1, 2 or 4) to I/O port `port`. Called with
    /// interrupts masked too.
    fn port_write(&self, port: u16, width: u8, value: u32);

    /// The kernel's clock: nanoseconds since boot, never going back.
    /// Called with interrupts masked too.
    fn now_ns(&self) -> u64;

    /// Waits `us` microseconds without sleeping. Called with interrupts
    /// masked too.
    fn stall_us(&self, us: u32);

    /// Sleeps for `ms` milliseconds.
    fn sleep_ms(&self, ms: u64);

    /// A number for the running thread, the same for as long as it runs
    /// and never `usize::MAX`. Called with interrupts masked too.
    fn thread_id(&self) -> usize;

    /// Masks interrupts on this CPU, returning whether they were on.
    fn disable_interrupts(&self) -> bool;

    /// Unmasks interrupts on this CPU if `were_enabled`.
    fn restore_interrupts(&self, were_enabled: bool);

    /// Sleeps until `ready` holds or the clock reaches `deadline_ns`;
    /// whoever makes it hold calls [`Host::wake_all`] with `key`. Returns
    /// whether it held. `ready` must only read atomics.
    fn wait(&self, key: usize, deadline_ns: Option<u64>, ready: &dyn Fn() -> bool) -> bool;

    /// Wakes every thread waiting with `key`. Called with interrupts
    /// masked too.
    fn wake_all(&self, key: usize);

    /// Routes the machine's interrupt `irq` (on a PC, the FADT's SCI, an
    /// ISA interrupt or a global system interrupt), level-triggered and
    /// active low as ACPI has it unless the firmware overrides it, so
    /// that each one calls [`interrupt`]. Returns false if it cannot.
    fn start_interrupt(&self, irq: u32) -> bool;

    /// Stops what [`Host::start_interrupt`] started.
    fn stop_interrupt(&self);

    /// Work is waiting on `queue`: its thread should call [`run_work`].
    /// Called in interrupt handlers too.
    fn work_queued(&self, queue: Queue);
}

/// The kernel's side, once [`start`] has it.
static HOST: Once<&'static dyn Host> = Once::new();

/// The host. uACPI calls back only after [`start`] has set it.
fn host() -> &'static dyn Host {
    match HOST.get() {
        Some(host) => *host,
        None => panic!("uACPI called the kernel before it was started"),
    }
}
