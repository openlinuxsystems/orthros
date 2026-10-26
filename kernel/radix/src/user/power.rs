// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods of the power capability (Phase 9, radix-power(2)): what
//! Spark calls at the end of an ordered shutdown to turn the machine off,
//! restart it or halt it.

use radix_abi::errors::Kernel;
use radix_abi::power::{self as abi, HaltArgs, PowerOffArgs, RestartArgs};
use radix_cap::{Capability, ObjectKind, Rights};
use radix_hal::{Cpu, Platform, Smp};
use radix_klog::crashlog::Ending;
use radix_klog::{info, warn};

use super::methods::Answer;
use super::{Result, UserRef, cap_error, error};
use crate::{Arch, console, crashlog};

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
    if let Some(vfs) = crate::fs::vfs()
        && let Err(err) = vfs.sync_all()
    {
        warn!("Power: a filesystem could not write back its changes: {err}");
    }
    crate::fs::disk::shutdown();
    // The screen is the log's again, whatever program held it, so the last
    // lines show.
    console::give_back();
    match end {
        End::PowerOff => {
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
