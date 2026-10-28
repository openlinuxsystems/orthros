// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB (Phase 11, ADR 0146): xHCI host controllers, and the devices on
//! their root ports.
//!
//! The PCI scan (`fs::disk`) hands each xHCI controller to [`bind`], which
//! maps its registers, turns on bus mastering, gives it an interrupt
//! (MSI-X, else MSI; a controller with neither is polled every 10 ms) and
//! starts a kernel thread for it, `usb1`, `usb2` and on ([`controller`]).
//! The thread takes the controller from the firmware and then runs it
//! alone: the interrupt only wakes it. It sets up every device it finds,
//! on the root ports and behind hubs ([`hub`], ADR 0147). Bus numbers
//! count controllers from 1, and a device is named as Linux names one, by
//! its bus, the root port and the hub ports it is behind: `1-3.2`.
//!
//! `usb=off` leaves the controllers as the firmware left them.
//! [`shutdown`] halts every controller before the machine is powered off
//! or restarted, so that none goes on writing to memory.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use radix_hal::{Devices, Timer};
use radix_hw::Mmio;
use radix_hw::pci::{Bar, Function, Msi, MsiX};
use radix_klog::{info, warn};
use radix_sync::Once;

use crate::Arch;
use crate::sched::{self, Options, Policy, ThreadRef};

mod controller;
mod hub;

/// The most controllers driven.
const MAX_CONTROLLERS: usize = 8;

/// Set by `usb=off`.
static OFF: AtomicBool = AtomicBool::new(false);
/// Controllers bound so far.
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// What a controller's thread starts from.
struct Start {
    regs: Mmio,
    /// Where it is on PCI, for the log.
    address: radix_hw::pci::Address,
    /// Its interrupt vector, and whether it is MSI-X or MSI, if it has one.
    interrupt: Option<(u32, &'static str)>,
}

/// Each controller's start, taken by its thread.
static STARTS: [Once<Start>; MAX_CONTROLLERS] = [const { Once::new() }; MAX_CONTROLLERS];
/// Each controller's thread, which its interrupt wakes.
static THREADS: [Once<ThreadRef>; MAX_CONTROLLERS] = [const { Once::new() }; MAX_CONTROLLERS];
/// Each controller's registers, for [`shutdown`].
static REGISTERS: [Once<Mmio>; MAX_CONTROLLERS] = [const { Once::new() }; MAX_CONTROLLERS];

/// Reads the `usb` flag: `usb=off` drives no controller.
pub fn init(off: bool) {
    OFF.store(off, Ordering::Relaxed);
}

/// A controller's interrupt: wakes its thread.
fn on_interrupt(index: usize) {
    if let Some(thread) = THREADS.get(index).and_then(Once::get) {
        sched::wake(thread);
    }
}

/// Takes the xHCI controller `function`, found by the PCI scan.
pub fn bind(function: &Function) {
    if OFF.load(Ordering::Relaxed) {
        info!("USB: leaving the xHCI controller at {} alone (usb=off)", function.address());
        return;
    }
    let index = COUNT.load(Ordering::Relaxed);
    if index >= MAX_CONTROLLERS {
        warn!("USB: too many controllers; the one at {} is left alone", function.address());
        return;
    }
    let Some(Bar::Memory { address, size, .. }) = function.bar(radix_xhci::BAR) else {
        warn!("USB: the xHCI controller at {} has no registers", function.address());
        return;
    };
    let Some(regs) =
        usize::try_from(size).ok().and_then(|size| <Arch as Devices>::map_device(address, size))
    else {
        warn!("USB: the xHCI controller at {} could not be mapped", function.address());
        return;
    };
    COUNT.store(index + 1, Ordering::Relaxed);
    function.enable_memory_and_dma();
    // One interrupt, for interrupter 0: an MSI-X entry, else MSI.
    let interrupt = MsiX::find(function)
        .and_then(|msix| {
            let (bar, offset) = msix.table_location();
            let Some(Bar::Memory { address, .. }) = function.bar(bar) else { return None };
            let table = <Arch as Devices>::map_device(address + offset, msix.table_len())?;
            let (vector, message) = <Arch as Devices>::allocate_interrupt(on_interrupt, index)?;
            msix.enable_masked(function);
            msix.set_entry(&table, 0, message);
            msix.unmask_all(function);
            Some((vector, "MSI-X"))
        })
        .or_else(|| {
            let msi = Msi::find(function)?;
            let (vector, message) = <Arch as Devices>::allocate_interrupt(on_interrupt, index)?;
            msi.enable(function, message);
            Some((vector, "MSI"))
        });
    let _ = REGISTERS[index].set(regs.clone());
    let _ = STARTS[index].set(Start { regs, address: function.address(), interrupt });
    let mut name = radix_klog::TextBuf::<8>::new();
    let _ = core::fmt::Write::write_fmt(&mut name, format_args!("usb{}", index + 1));
    let options = Options::new().held().policy(Policy::Fair { nice: -5 });
    match sched::spawn(name.as_str(), controller::run, index, options) {
        Ok(thread) => {
            let _ = THREADS[index].set(thread.clone());
            sched::release(&thread);
        }
        Err(err) => warn!(
            "USB: could not start the thread for the controller at {}: {err}",
            function.address()
        ),
    }
}

/// Halts every controller, before the machine is powered off, restarted
/// or halted.
pub fn shutdown() {
    for regs in REGISTERS.iter().filter_map(Once::get) {
        if !radix_xhci::stop(regs, &mut |us| Arch::delay_us(u64::from(us))) {
            warn!("USB: a controller did not halt");
        }
    }
}
