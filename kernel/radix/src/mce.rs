// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Machine checks (Phase 11, ADR 0145): the hardware errors the CPUs'
//! machine-check banks report.
//!
//! An error a CPU cannot go on after panics where the architecture takes
//! it. The rest wait in the banks for the `mce` thread, which reads every
//! CPU's banks at the end of boot, finding what an earlier boot left
//! there, and then every [`DEFAULT_SECONDS`] (`mce-poll=seconds`), as
//! Linux polls where the CPU has no interrupt for corrected errors. Each
//! error is logged with its CPU and bank: a warning for one the hardware
//! corrected, an error for one it did not but the CPU went on after.
//!
//! The page of memory an uncorrected error names leaves use for good (ADR
//! 0168): out of the frame allocator at once if it is free, or when it is
//! freed ([`radix_mm::Frames::retire`]). A program that read it has ended
//! by then, or its SIGBUS handler runs (`user::on_fault`, which reads the
//! banks first).

use core::sync::atomic::{AtomicBool, Ordering};

use radix_hal::{Cpu, CpuLocal};
use radix_klog::{error, info, warn};
use radix_mm::{FRAMES, Retired};

use crate::Arch;
use crate::sched::{self, Options, Policy};
use crate::work;

/// How often the banks are read, by default: Linux's interval too.
const DEFAULT_SECONDS: u64 = 300;

/// The CPU the thread runs on, which posts the reads to the others.
const HOME: usize = 0;

/// The first read, at the end of boot.
static AT_BOOT: AtomicBool = AtomicBool::new(true);

/// Starts the thread, reading the banks every `seconds` (`mce-poll`).
pub fn start(seconds: Option<&str>) {
    let banks = Arch::machine_check_banks();
    if banks == 0 {
        info!("Machine check: off (the CPU has no machine-check architecture, or `mce=off`)");
        return;
    }
    let seconds = match seconds.map(str::parse::<u64>) {
        None => DEFAULT_SECONDS,
        Some(Ok(seconds)) if (1..=86_400).contains(&seconds) => seconds,
        Some(_) => {
            warn!("Machine check: `mce-poll` takes 1 to 86400 seconds; using {DEFAULT_SECONDS}");
            DEFAULT_SECONDS
        }
    };
    info!("Machine check: {banks} banks on each CPU, read at boot and every {seconds} s");
    let options = Options::new().pinned(HOME).policy(Policy::Fair { nice: 0 });
    if let Err(err) = sched::spawn("mce", run, seconds as usize, options) {
        error!("Machine check: could not start the thread that reads the banks: {err}");
    }
}

fn run(seconds: usize) {
    loop {
        work::on_all(HOME, read);
        AT_BOOT.store(false, Ordering::Relaxed);
        sched::sleep_until(sched::now_ns().saturating_add(seconds as u64 * 1_000_000_000));
    }
}

/// Reads this CPU's banks, logs what they held, and retires the pages
/// they name.
pub(crate) fn read() {
    let cpu = Arch::current_cpu();
    let when = if AT_BOOT.load(Ordering::Relaxed) { ", at boot" } else { "" };
    Arch::poll_machine_checks(&mut |bank, corrected, page, what| {
        if corrected {
            warn!("Machine check: CPU {cpu} bank {bank}{when}: {what}");
        } else {
            error!("Machine check: CPU {cpu} bank {bank}{when}: {what}");
        }
        let Some(page) = page else { return };
        match FRAMES.retire(page) {
            Retired::Now => warn!("Machine check: the page at {page:#x} is out of use for good"),
            Retired::WhenFreed => {
                warn!("Machine check: the page at {page:#x} leaves use for good once freed");
            }
            Retired::NotManaged => {}
            Retired::TooMany => {
                error!("Machine check: the page at {page:#x} cannot be retired: too many are");
            }
        }
    });
}
