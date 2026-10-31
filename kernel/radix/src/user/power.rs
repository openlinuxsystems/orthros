// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods of the power capability (Phase 9, radix-power(2)): what
//! Spark calls at the end of an ordered shutdown to turn the machine off,
//! restart it or halt it, and how it hears the power button (Phase 11,
//! ADR 0142).
//!
//! [`start`] asks uACPI for the button when it runs the firmware's AML
//! (ADR 0200), or else the platform. Its presses come in an interrupt
//! handler, which counts them and wakes the `power` thread; the
//! thread signals the notification Spark gave `notify_button`, which
//! cannot be done with interrupts masked. The watchdog's methods go to
//! [`crate::watchdog`] (ADR 0143).
//!
//! A thermal zone past its critical trip point ([`overheated`], ADR 0203)
//! reaches Spark as a press of the button, and the thread powers the
//! machine off itself if it is still on [`OVERHEAT_GRACE_NS`] later.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::errors::Kernel;
use radix_abi::power::{
    self as abi, HaltArgs, NotifyButtonArgs, PetWatchdogArgs, PowerOffArgs, RestartArgs,
    StartWatchdogArgs, StartWatchdogResults, StopWatchdogArgs,
};
use radix_cap::{Capability, ObjectKind, Rights};
use radix_hal::{Cpu, Devices, Platform, Smp, Timer};
use radix_klog::crashlog::Ending;
use radix_klog::{error as log_error, info, warn};
use radix_sync::{Once, SpinLock};

use super::methods::{Answer, answer};
use super::notify::NotificationRef;
use super::objects;
use super::{Result, UserRef, cap_error, error};
use crate::sched::{self, Options, Policy, ThreadRef};
use crate::{Arch, console, crashlog, watchdog};

/// How long Spark has to power the machine off once a thermal zone is past
/// its critical trip point, before the kernel does, as Linux's
/// `hw_protection_shutdown` waits.
pub const OVERHEAT_GRACE_NS: u64 = 30_000_000_000;

/// Whether the platform hears the power button.
static HEARD: AtomicBool = AtomicBool::new(false);
/// When the kernel powers an overheated machine off itself; 0 while none
/// is.
static OVERHEAT_DEADLINE: AtomicU64 = AtomicU64::new(0);
/// Presses of the button so far, and those passed on.
static PRESSES: AtomicU64 = AtomicU64::new(0);
static PASSED: AtomicU64 = AtomicU64::new(0);
/// The notification `notify_button` set, and the bits to set.
static BUTTON: SpinLock<Option<(NotificationRef, u64)>> = SpinLock::new(None);
static THREAD: Once<ThreadRef> = Once::new();

/// Starts the thread, then the power button.
pub fn start() {
    match sched::spawn("power", run, 0, Options::new().policy(Policy::Fair { nice: -5 })) {
        Ok(thread) => {
            let _ = THREAD.set(thread);
        }
        Err(err) => {
            log_error!("Power: could not start the thread that passes the power button on: {err}");
            return;
        }
    }
    // uACPI owns the SCI when it runs, so the platform's own button would
    // fight it for the interrupt.
    let button = if crate::acpi::running() {
        crate::acpi::power_button(pressed)
    } else {
        Arch::start_power_button(pressed).map(|name| {
            let mut line = crate::acpi::devices::Line::new();
            let _ = line.write_str(name);
            line
        })
    };
    match button {
        Some(name) => {
            HEARD.store(true, Ordering::Release);
            info!("Power: {} asks Spark to power the machine off", name.as_str());
        }
        None => info!("Power: no power button the kernel can hear; the firmware keeps it"),
    }
}

/// The platform's interrupt handler: one more press.
fn pressed() {
    PRESSES.fetch_add(1, Ordering::AcqRel);
    if let Some(thread) = THREAD.get() {
        sched::wake(thread);
    }
}

/// A thermal zone, `zone`, is past its critical trip point: Spark is asked
/// to power the machine off, as for the power button, and the `power`
/// thread does it if Spark has not within [`OVERHEAT_GRACE_NS`].
pub fn overheated(zone: &str) {
    let deadline = Arch::now_ns().saturating_add(OVERHEAT_GRACE_NS).max(1);
    if OVERHEAT_DEADLINE.compare_exchange(0, deadline, Ordering::AcqRel, Ordering::Acquire).is_err()
    {
        return;
    }
    info!("Power: {zone} is too hot; asking Spark to power the machine off");
    if let Some(thread) = THREAD.get() {
        sched::wake(thread);
    }
}

/// Tells Spark of a press of the button. Returns whether it listens.
fn tell_spark() -> bool {
    let listener = BUTTON.lock().clone();
    match listener {
        Some((notification, badge)) => {
            notification.notify(badge);
            true
        }
        None => false,
    }
}

fn run(_: usize) {
    let mut overheat_told = false;
    loop {
        let presses = PRESSES.load(Ordering::Acquire);
        if PASSED.swap(presses, Ordering::AcqRel) != presses {
            info!("Power: the power button was pressed");
            if !tell_spark() {
                warn!("Power: nothing listens for the power button");
            }
        }
        let deadline = OVERHEAT_DEADLINE.load(Ordering::Acquire);
        if deadline == 0 {
            sched::block("power");
            continue;
        }
        if !overheat_told {
            overheat_told = true;
            if !tell_spark() {
                log_error!("Power: nothing listens for the power button; powering off at once");
                overheat_power_off();
            }
        } else if Arch::now_ns() >= deadline {
            log_error!(
                "Power: the machine is still on {} s after it overheated; powering it off",
                OVERHEAT_GRACE_NS / 1_000_000_000
            );
            overheat_power_off();
        }
        sched::block_until(deadline, "power");
    }
}

/// Powers an overheated machine off without Spark, as the method does
/// once Spark has stopped everything; halts it if the firmware cannot.
fn overheat_power_off() -> ! {
    watchdog::ending(false);
    wind_down();
    crate::acpi::power_off();
    Arch::power_off();
    log_error!("Power: this machine cannot be powered off by the kernel; halting it to cool");
    Arch::stop_other_cpus();
    Arch::halt()
}

/// Writes the filesystems back and quiets the disks and USB, before the
/// machine ends.
fn wind_down() {
    if let Some(vfs) = crate::fs::vfs()
        && let Err(err) = vfs.sync_all()
    {
        warn!("Power: a filesystem could not write back its changes: {err}");
    }
    crate::fs::disk::shutdown();
    crate::usb::shutdown();
    // The screen is the log's again, whatever program held it, so the last
    // lines show.
    console::give_back();
}

/// What a method does to the machine.
#[derive(Clone, Copy)]
enum End {
    PowerOff,
    Restart,
    Halt,
}

/// A method of the power capability `cap`, called by `user`'s thread,
/// kept out of line, as [`super::methods::call`] says.
#[inline(never)]
pub fn method(user: &UserRef, cap: &Capability, label: u32, args: &[u64; 8]) -> Result<Answer> {
    let end = match label {
        abi::NOTIFY_BUTTON => {
            let a = NotifyButtonArgs::decode(args)?;
            cap.check(ObjectKind::Power, Rights::CONTROL).map_err(cap_error)?;
            let (notified, notification) =
                objects::notification(user.cspace, a.notification, Rights::SEND)?;
            if !HEARD.load(Ordering::Acquire) {
                return Err(error(Kernel::NotSupported));
            }
            *BUTTON.lock() = Some((notification, notified.badge()));
            return answer(label, 0, [0; 8]);
        }
        abi::START_WATCHDOG => {
            let a = StartWatchdogArgs::decode(args)?;
            cap.check(ObjectKind::Power, Rights::CONTROL).map_err(cap_error)?;
            let name = user.process.name();
            let seconds = watchdog::start(a.seconds, name.as_str())
                .ok_or_else(|| error(Kernel::NotSupported))?;
            let restarted = watchdog::found().is_some_and(|found| found.restarted);
            let results = StartWatchdogResults { seconds, restarted };
            return answer(label, StartWatchdogResults::WORDS, results.encode());
        }
        abi::PET_WATCHDOG => {
            PetWatchdogArgs::decode(args)?;
            cap.check(ObjectKind::Power, Rights::CONTROL).map_err(cap_error)?;
            if !watchdog::pet() {
                return Err(error(Kernel::NotSupported));
            }
            return answer(label, 0, [0; 8]);
        }
        abi::STOP_WATCHDOG => {
            StopWatchdogArgs::decode(args)?;
            cap.check(ObjectKind::Power, Rights::CONTROL).map_err(cap_error)?;
            if !watchdog::stop(user.process.name().as_str()) {
                return Err(error(Kernel::NotSupported));
            }
            return answer(label, 0, [0; 8]);
        }
        abi::POWER_OFF => {
            PowerOffArgs::decode(args)?;
            End::PowerOff
        }
        abi::RESTART => {
            RestartArgs::decode(args)?;
            End::Restart
        }
        abi::HALT => {
            HaltArgs::decode(args)?;
            End::Halt
        }
        _ => return Err(error(Kernel::NoSuchMethod)),
    };
    cap.check(ObjectKind::Power, Rights::CONTROL).map_err(cap_error)?;
    let verb = match end {
        End::PowerOff => "power off",
        End::Restart => "restart",
        End::Halt => "halt",
    };
    info!("Power: `{}` asked to {verb} the machine; syncing the filesystems", user.process.name());
    watchdog::ending(matches!(end, End::Halt));
    wind_down();
    match end {
        End::PowerOff => {
            crate::acpi::power_off();
            Arch::power_off();
            // The firmware gave no way to: the caller restarts or halts.
            warn!("Power: this machine cannot be powered off by the kernel");
            Err(error(Kernel::NotSupported))
        }
        End::Restart => {
            info!("Power: restarting");
            crashlog::set_ending(Ending::Restarted);
            Arch::reset()
        }
        End::Halt => {
            info!("Power: the system is halted; it is safe to turn the machine off");
            Arch::stop_other_cpus();
            Arch::halt()
        }
    }
}
