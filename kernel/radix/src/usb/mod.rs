// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB (Phase 11, ADRs 0146 and 0243): host controllers, and the devices
//! on their root ports.
//!
//! The PCI scan (`fs::disk`) hands each USB controller to [`found`]; once
//! it is done, [`bind_found`] moves the USB 2.0 ports of Intel's 7 to 9
//! Series chipsets from their EHCI controllers to their xHCI one
//! (`radix_usbhc::intel`), then binds the xHCI controllers, then the EHCI
//! ones, then their companions, as each must take its ports before the
//! next. Binding maps a controller's registers, turns on bus mastering,
//! gives an xHCI controller an interrupt (MSI-X, else MSI; a controller
//! with neither is polled every 10 ms) and starts a kernel thread for it,
//! `usb1`, `usb2` and on: [`controller`] for xHCI, [`legacy`] for EHCI,
//! OHCI and UHCI, which are polled.
//! The thread takes the controller from the firmware and then runs it
//! alone: the interrupt only wakes it. It sets up every device it finds,
//! on the root ports and behind hubs ([`hub`], ADR 0147), reads the
//! keyboards, mice and tablets among them ([`hid`], ADR 0148), and
//! carries the transfers of the sticks and disks among them for their
//! own threads ([`storage`], ADR 0161). With the
//! `dbc` flag, the first controller with a Debug Capability carries the
//! console to a second computer ([`dbc`], ADR 0160). Bus numbers
//! count controllers from 1, and a device is named as Linux names one, by
//! its bus, the root port and the hub ports it is behind: `1-3.2`.
//! `/kernel/bus/usb/devices` shows them ([`shown`], ADR 0230).
//!
//! `usb=off` leaves the controllers as the firmware left them.
//! [`shutdown`] halts every controller before the machine is powered off
//! or restarted, so that none goes on writing to memory, and routes the
//! Intel ports back as the firmware left them.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use radix_hal::{Devices, Timer};
use radix_hw::Mmio;
use radix_hw::pci::{Bar, Function, Msi, MsiX};
use radix_klog::{info, warn};
use radix_sync::{Once, SpinLock};
use radix_usbhc::intel;

use crate::Arch;
use crate::ioports::IoWindow;
use crate::sched::{self, Options, Policy, ThreadRef};

mod controller;
pub mod dbc;
mod hid;
mod hub;
mod legacy;
pub mod shown;
pub mod storage;

/// The most controllers driven.
const MAX_CONTROLLERS: usize = 16;
/// The PCI class and subclass of USB controllers; the programming
/// interface says which.
const USB_CLASS: (u8, u8) = (0x0c, 0x03);

/// Which interface a controller has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Xhci,
    Ehci,
    Ohci,
    Uhci,
}

impl Kind {
    /// The kind of the USB controller `function`, if the kernel drives it.
    fn of(function: &Function) -> Option<Kind> {
        match function.class() {
            radix_xhci::PCI_CLASS => Some(Kind::Xhci),
            radix_usbhc::ehci::PCI_CLASS => Some(Kind::Ehci),
            radix_usbhc::ohci::PCI_CLASS => Some(Kind::Ohci),
            radix_usbhc::uhci::PCI_CLASS => Some(Kind::Uhci),
            _ => None,
        }
    }

    /// Its name, for the log.
    fn name(self) -> &'static str {
        match self {
            Kind::Xhci => "xHCI",
            Kind::Ehci => "EHCI",
            Kind::Ohci => "OHCI",
            Kind::Uhci => "UHCI",
        }
    }

    /// The order the kinds are bound in: a controller before its
    /// companions.
    fn order(self) -> u8 {
        match self {
            Kind::Xhci => 0,
            Kind::Ehci => 1,
            Kind::Ohci | Kind::Uhci => 2,
        }
    }
}

/// Set by `usb=off`.
static OFF: AtomicBool = AtomicBool::new(false);
/// Set when the controllers are halted for the machine to go down: the
/// polled ones' threads then read no more.
static HALTED: AtomicBool = AtomicBool::new(false);
/// Controllers bound so far.
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// Controllers that have looked at what was plugged into them at boot,
/// or could not start.
static LOOKED: AtomicUsize = AtomicUsize::new(0);
/// How long the end of boot waits for the disks plugged in at boot, at
/// most.
const DISKS_NS: u64 = 5_000_000_000;

/// Where a controller's registers are: memory, or I/O ports (UHCI's).
#[derive(Clone)]
enum Window {
    Memory(Mmio),
    Ports(IoWindow),
}

impl Window {
    /// Its memory-mapped registers, if they are.
    fn memory(&self) -> Option<&Mmio> {
        match self {
            Window::Memory(regs) => Some(regs),
            Window::Ports(_) => None,
        }
    }
}

/// What a controller's thread starts from.
struct Start {
    kind: Kind,
    regs: Window,
    /// The function, whose configuration space EHCI's handoff is in.
    function: Function,
    /// Where it is on PCI, for the log.
    address: radix_hw::pci::Address,
    /// Its interrupt vector, and whether it is MSI-X or MSI, if it has one.
    interrupt: Option<(u32, &'static str)>,
}

/// The USB controllers the PCI scan found, not yet bound.
static FOUND: SpinLock<[Option<Function>; MAX_CONTROLLERS]> =
    SpinLock::new([const { None }; MAX_CONTROLLERS]);
/// The Intel xHCI controllers whose ports were moved, and how the firmware
/// had routed them, for [`shutdown`].
static ROUTED: [Once<(Function, intel::Routing)>; 2] = [const { Once::new() }; 2];

/// Each controller's start, taken by its thread.
static STARTS: [Once<Start>; MAX_CONTROLLERS] = [const { Once::new() }; MAX_CONTROLLERS];
/// Each controller's thread, which its interrupt wakes.
static THREADS: [Once<ThreadRef>; MAX_CONTROLLERS] = [const { Once::new() }; MAX_CONTROLLERS];
/// Each controller's kind and registers, for [`shutdown`].
static REGISTERS: [Once<(Kind, Window)>; MAX_CONTROLLERS] =
    [const { Once::new() }; MAX_CONTROLLERS];

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

/// Notes the USB controller `function`, found by the PCI scan, for
/// [`bind_found`].
pub fn found(function: &Function) {
    if function.class().0 != USB_CLASS.0 || function.class().1 != USB_CLASS.1 {
        return;
    }
    let mut found = FOUND.lock();
    match found.iter_mut().find(|f| f.is_none()) {
        Some(free) => *free = Some(function.clone()),
        None => warn!("USB: too many controllers; the one at {} is left alone", function.address()),
    }
}

/// Once the PCI scan is done: moves the Intel chipset's USB 2.0 ports to
/// its xHCI controller, then binds the controllers found, xHCI first and
/// each EHCI before its companions.
pub fn bind_found() {
    let found = core::mem::replace(&mut *FOUND.lock(), [const { None }; MAX_CONTROLLERS]);
    let off = OFF.load(Ordering::Relaxed);
    let intel_ehci = found.iter().flatten().any(intel::is_intel_ehci);
    let mut routed = 0;
    for function in found.iter().flatten() {
        if off || !intel::switchable(function, intel_ehci) || routed >= ROUTED.len() {
            continue;
        }
        let before = intel::routing(function);
        let after = intel::switch_ports(function);
        info!(
            "USB: the chipset's USB 2.0 ports {:#x} and USB 3 ports {:#x} go to the xHCI controller at {}",
            after.usb2,
            after.usb3,
            function.address()
        );
        let _ = ROUTED[routed].set((function.clone(), before));
        routed += 1;
    }
    for order in 0..=2 {
        for function in found.iter().flatten() {
            match Kind::of(function) {
                Some(kind) if kind.order() == order => bind(function, kind),
                _ => {}
            }
        }
    }
    for function in found.iter().flatten().filter(|f| Kind::of(f).is_none()) {
        let (_, _, interface) = function.class();
        info!(
            "USB: the controller at {} (interface {interface:#04x}) is not driven",
            function.address()
        );
    }
}

/// Takes the controller `function`, of `kind`.
fn bind(function: &Function, kind: Kind) {
    let name = kind.name();
    if OFF.load(Ordering::Relaxed) {
        info!("USB: leaving the {name} controller at {} alone (usb=off)", function.address());
        return;
    }
    let index = COUNT.load(Ordering::Relaxed);
    if index >= MAX_CONTROLLERS {
        warn!("USB: too many controllers; the one at {} is left alone", function.address());
        return;
    }
    let bar = match kind {
        Kind::Xhci => radix_xhci::BAR,
        Kind::Ehci => radix_usbhc::ehci::BAR,
        Kind::Ohci => radix_usbhc::ohci::BAR,
        Kind::Uhci => radix_usbhc::uhci::BAR,
    };
    let regs = match function.bar(bar) {
        Some(Bar::Memory { address, size, .. }) if kind != Kind::Uhci => {
            let Some(regs) = usize::try_from(size)
                .ok()
                .and_then(|size| <Arch as Devices>::map_device(address, size))
            else {
                warn!("USB: the {name} controller at {} could not be mapped", function.address());
                return;
            };
            Window::Memory(regs)
        }
        Some(Bar::Io { port, size }) if kind == Kind::Uhci && port + size <= 0x1_0000 => {
            Window::Ports(IoWindow::new(port as u16, size.min(0x100) as u16))
        }
        _ => {
            warn!("USB: the {name} controller at {} has no registers", function.address());
            return;
        }
    };
    COUNT.store(index + 1, Ordering::Relaxed);
    function.enable_memory_and_dma();
    if kind == Kind::Uhci {
        function.set_command(function.command() | radix_hw::pci::command::IO);
    }
    // The older controllers are polled: the kernel routes no INTx yet.
    let interrupt = if kind != Kind::Xhci { None } else { interrupt(function, index) };
    let _ = REGISTERS[index].set((kind, regs.clone()));
    let start =
        Start { kind, regs, function: function.clone(), address: function.address(), interrupt };
    let _ = STARTS[index].set(start);
    let mut thread_name = radix_klog::TextBuf::<8>::new();
    let _ = core::fmt::Write::write_fmt(&mut thread_name, format_args!("usb{}", index + 1));
    let options = Options::new().held().policy(Policy::Fair { nice: -5 });
    let main = if kind == Kind::Xhci { controller::run } else { legacy::run };
    match sched::spawn(thread_name.as_str(), main, index, options) {
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

/// One interrupt for an xHCI controller's interrupter 0: an MSI-X entry,
/// else MSI.
fn interrupt(function: &Function, index: usize) -> Option<(u32, &'static str)> {
    MsiX::find(function)
        .and_then(|msix| {
            let (bar, offset) = msix.table_location();
            let Some(Bar::Memory { address, .. }) = function.bar(bar) else { return None };
            let table = <Arch as Devices>::map_device(address + offset, msix.table_len())?;
            let (vector, message) =
                <Arch as Devices>::allocate_interrupt(function.address(), on_interrupt, index)?;
            msix.enable_masked(function);
            msix.set_entry(&table, 0, message);
            msix.unmask_all(function);
            Some((vector, "MSI-X"))
        })
        .or_else(|| {
            let msi = Msi::find(function)?;
            let (vector, message) =
                <Arch as Devices>::allocate_interrupt(function.address(), on_interrupt, index)?;
            msi.enable(function, message);
            Some((vector, "MSI"))
        })
}

/// A controller has looked at what was plugged into it at boot: the
/// devices on its ports, and behind their hubs.
fn looked() {
    LOOKED.fetch_add(1, Ordering::AcqRel);
}

/// At the end of boot, before the first program: waits until every
/// controller has looked at what was plugged into it, and every stick
/// found then has found its disks, for at most [`DISKS_NS`], so that the
/// disks plugged in at boot are there for it (ADR 0161).
pub fn wait_for_disks() {
    let deadline = <Arch as Timer>::now_ns().saturating_add(DISKS_NS);
    let settled = || {
        LOOKED.load(Ordering::Acquire) >= COUNT.load(Ordering::Acquire)
            && storage::PROBING.load(Ordering::Acquire) == 0
    };
    while !settled() {
        if <Arch as Timer>::now_ns() >= deadline {
            info!("USB: the devices plugged in at boot are not all set up yet; going on");
            break;
        }
        sched::sleep_ns(10_000_000);
    }
    storage::BOOTED.store(true, Ordering::Release);
}

/// The keyboards' lock lights are to change: wakes every controller's
/// thread, which lights its keyboards' (`fs::keyboard::lights`).
pub fn lights_changed() {
    for thread in THREADS.iter().filter_map(Once::get) {
        sched::wake(thread);
    }
}

/// Whether the controllers were halted for the machine to go down.
fn halted() -> bool {
    HALTED.load(Ordering::SeqCst)
}

/// Halts every controller, before the machine is powered off, restarted
/// or halted.
pub fn shutdown() {
    HALTED.store(true, Ordering::SeqCst);
    dbc::stop();
    for (kind, regs) in REGISTERS.iter().filter_map(Once::get) {
        let mut delay = |us: u32| Arch::delay_us(u64::from(us));
        let halted = match (kind, regs) {
            (Kind::Xhci, Window::Memory(regs)) => radix_xhci::stop(regs, &mut delay),
            (Kind::Ehci, Window::Memory(regs)) => radix_usbhc::ehci::operational(regs)
                .is_some_and(|op| radix_usbhc::ehci::stop(&op, &mut delay)),
            (Kind::Ohci, Window::Memory(regs)) => radix_usbhc::ohci::stop(regs),
            (Kind::Uhci, Window::Ports(io)) => radix_usbhc::uhci::stop(io, &mut delay),
            _ => true,
        };
        if !halted {
            warn!("USB: a controller did not halt");
        }
    }
    for (function, before) in ROUTED.iter().filter_map(Once::get) {
        intel::restore(function, *before);
    }
}
