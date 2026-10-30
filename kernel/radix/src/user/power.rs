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

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_abi::errors::Kernel;
use radix_abi::power::{
    self as abi, HaltArgs, NotifyButtonArgs, PetWatchdogArgs, PowerOffArgs, RestartArgs,
    StartWatchdogArgs, StartWatchdogResults, StopWatchdogArgs,
};
use radix_cap::{Capability, ObjectKind, Rights};
use radix_hal::{Cpu, Devices, Platform, Smp};
use radix_klog::crashlog::Ending;
use radix_klog::{error as log_error, info, warn};
use radix_sync::{Once, SpinLock};

use super::methods::{Answer, answer};
use super::notify::NotificationRef;
use super::objects;
use super::{Result, UserRef, cap_error, error};
use crate::sched::{self, Options, Policy, ThreadRef};
use crate::{Arch, console, crashlog, watchdog};

/// Whether the platform hears the power button.
static HEARD: AtomicBool = AtomicBool::new(false);
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

fn run(_: usize) {
    loop {
        let presses = PRESSES.load(Ordering::Acquire);
        if PASSED.swap(presses, Ordering::AcqRel) != presses {
            info!("Power: the power button was pressed");
            match BUTTON.lock().clone() {
                Some((notification, badge)) => notification.notify(badge),
                None => warn!("Power: nothing listens for the power button"),
            }
        }
        sched::block("power");
    }
}

/// What a method does to the machine.
#[derive(Clone, Copy)]
enum End {
    PowerOff,
    Restart,
    Halt,
}

/// A method of the power capability `cap`, called by `user`'s thread.
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
